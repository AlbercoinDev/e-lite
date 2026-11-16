//! Restart after a fatal error, without an init system.
//!
//! A daemon command (`run`, `serve`, `index`) runs as two processes of the
//! same binary: a small parent, which does nothing but watch, and the node,
//! its child. The node aborts on purpose when a thread that owns shared state
//! panics (AUD-173); the kernel may also kill it (out of memory). The parent
//! starts it again, so no systemd unit, container policy or other supervisor
//! is needed for that.
//!
//! Rules:
//!
//! * only an abnormal end is restarted: the node was killed by a signal that
//!   nobody sent to stop it (SIGABRT, SIGSEGV, SIGKILL, ...). An exit code,
//!   zero or not, is final: a configuration error or a failed integrity check
//!   would fail the same way again;
//! * SIGTERM/SIGINT/SIGUSR1 received by the parent are passed on, and a stop
//!   request is never followed by a restart;
//! * restarts back off (1 s doubling to 60 s) and stop for good after
//!   [`MAX_FAILURES`] abnormal ends inside [`FAILURE_WINDOW`];
//! * the node dies with its parent (`PR_SET_PDEATHSIG`), so `kill -9` on the
//!   parent still means what it meant before: the node stops at once.
//!
//! A managed Core is not part of this: it keeps running across a restart and
//! the new node adopts it from its pid file (`CoreSupervisor`).

use std::collections::VecDeque;
use std::ffi::OsString;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// Set in the node's environment: it is the supervised child.
pub(crate) const CHILD_ENV: &str = "E_LITE_SUPERVISED";
/// `1` runs the command as a single process, without restarts.
pub(crate) const DISABLE_ENV: &str = "E_LITE_NO_SUPERVISE";
/// How many times the node has been restarted (set for the child).
pub(crate) const RESTARTS_ENV: &str = "E_LITE_RESTARTS";
/// Why the previous node ended (set for the child).
pub(crate) const REASON_ENV: &str = "E_LITE_RESTART_REASON";

/// Abnormal ends tolerated inside [`FAILURE_WINDOW`] before giving up.
pub(crate) const MAX_FAILURES: usize = 5;
pub(crate) const FAILURE_WINDOW: Duration = Duration::from_secs(600);
const FIRST_DELAY: Duration = Duration::from_secs(1);
const MAX_DELAY: Duration = Duration::from_secs(60);
/// A node that ran this long was healthy: the delay starts over.
const HEALTHY_RUN: Duration = Duration::from_secs(600);

/// How the node ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Exit {
    Code(i32),
    Signal(i32),
}

impl Exit {
    fn of(status: ExitStatus) -> Self {
        match status.signal() {
            Some(signal) => Exit::Signal(signal),
            None => Exit::Code(status.code().unwrap_or(1)),
        }
    }

    pub(crate) fn describe(self) -> String {
        match self {
            Exit::Code(code) => format!("exit code {code}"),
            Exit::Signal(signal) => {
                let name = match signal {
                    libc::SIGABRT => " (SIGABRT, internal error)",
                    libc::SIGKILL => " (SIGKILL, often the out-of-memory killer)",
                    libc::SIGSEGV => " (SIGSEGV)",
                    libc::SIGBUS => " (SIGBUS)",
                    _ => "",
                };
                format!("signal {signal}{name}")
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    Restart,
    /// Leave with this exit code.
    Stop(i32),
}

/// What to do after the node ended. `stopping`: a stop was requested.
pub(crate) fn decide(exit: Exit, stopping: bool) -> Decision {
    match exit {
        Exit::Code(code) => Decision::Stop(code),
        Exit::Signal(signal) => {
            // Signals that mean "stop": someone asked this node to end.
            let asked = matches!(
                signal,
                libc::SIGTERM | libc::SIGINT | libc::SIGHUP | libc::SIGQUIT
            );
            if stopping || asked {
                Decision::Stop(128 + signal)
            } else {
                Decision::Restart
            }
        }
    }
}

/// Delay before each restart, and when to stop trying.
pub(crate) struct Backoff {
    failures: VecDeque<Instant>,
    delay: Duration,
}

impl Backoff {
    pub(crate) fn new() -> Self {
        Backoff {
            failures: VecDeque::new(),
            delay: FIRST_DELAY,
        }
    }

    /// Record an abnormal end at `now` of a node that ran for `ran_for`.
    /// `None`: too many in a short time, do not restart.
    pub(crate) fn next(&mut self, now: Instant, ran_for: Duration) -> Option<Duration> {
        if ran_for >= HEALTHY_RUN {
            self.delay = FIRST_DELAY;
        }
        self.failures.push_back(now);
        while self
            .failures
            .front()
            .is_some_and(|at| now.duration_since(*at) > FAILURE_WINDOW)
        {
            self.failures.pop_front();
        }
        if self.failures.len() >= MAX_FAILURES {
            return None;
        }
        let delay = self.delay;
        self.delay = (self.delay * 2).min(MAX_DELAY);
        Some(delay)
    }
}

/// Whether this invocation should become the watching parent.
pub(crate) fn wanted(daemon_command: bool, no_supervise_flag: bool, one_shot: bool) -> bool {
    daemon_command
        && !one_shot
        && !no_supervise_flag
        && std::env::var_os(CHILD_ENV).is_none()
        && std::env::var(DISABLE_ENV).map_or(true, |v| v != "1")
}

/// The line the restarted node prints first, from what its parent passed on.
pub(crate) fn restart_note() -> Option<String> {
    let restarts: u32 = std::env::var(RESTARTS_ENV).ok()?.parse().ok()?;
    if restarts == 0 {
        return None;
    }
    let reason = std::env::var(REASON_ENV).unwrap_or_else(|_| "unknown".to_string());
    Some(format!(
        "[e-lite] restarted automatically after the previous run ended abnormally ({reason}); \
         restart {restarts}. The index recovers from its log."
    ))
}

/// The binary to start. After an in-place update the running file is gone
/// and the kernel reports it as "<path> (deleted)": start the new one.
fn node_binary() -> std::io::Result<std::path::PathBuf> {
    let exe = std::env::current_exe()?;
    if exe.exists() {
        return Ok(exe);
    }
    let text = exe.to_string_lossy();
    match text.strip_suffix(" (deleted)") {
        Some(path) if std::path::Path::new(path).exists() => Ok(path.into()),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("{} is gone", exe.display()),
        )),
    }
}

fn spawn_node(args: &[OsString], restarts: u32, reason: &str) -> std::io::Result<Child> {
    let mut command = Command::new(node_binary()?);
    command
        .args(args)
        .env(CHILD_ENV, "1")
        .env(RESTARTS_ENV, restarts.to_string())
        .env(REASON_ENV, reason);
    // The node must not outlive its parent: without it nothing would restart
    // the node, and a second start would find the database locked.
    unsafe {
        command.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn()
}

fn signal(child: &Child, signal: i32) {
    unsafe {
        libc::kill(child.id() as libc::pid_t, signal);
    }
}

/// Run the node as a child and keep it running. Returns the process exit
/// code, or `None` when the first node could not be started at all (the
/// caller then runs the command in this process, as before).
pub(crate) fn run() -> Option<i32> {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let stop = crate::shutdown_flag();
    let reload = crate::reload_flag();
    let mut backoff = Backoff::new();
    let mut restarts = 0u32;
    let mut reason = String::new();
    loop {
        let mut child = match spawn_node(&args, restarts, &reason) {
            Ok(child) => child,
            Err(e) if restarts == 0 => {
                eprintln!(
                    "[e-lite] WARNING: cannot start the supervised node ({e}); \
                     running without automatic restart"
                );
                return None;
            }
            Err(e) => {
                eprintln!("[e-lite] supervisor: cannot restart the node: {e}; giving up");
                return Some(1);
            }
        };
        let started = Instant::now();
        let mut stopping = false;
        let status = loop {
            if stop.swap(false, Ordering::SeqCst) {
                stopping = true;
                signal(&child, libc::SIGTERM);
            }
            if reload.swap(false, Ordering::SeqCst) {
                signal(&child, libc::SIGUSR1);
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(e) => {
                    eprintln!("[e-lite] supervisor: cannot wait for the node: {e}");
                    return Some(1);
                }
            }
        };
        let exit = Exit::of(status);
        match decide(exit, stopping) {
            Decision::Stop(code) => return Some(code),
            Decision::Restart => {}
        }
        let Some(delay) = backoff.next(Instant::now(), started.elapsed()) else {
            eprintln!(
                "[e-lite] supervisor: the node ended abnormally {MAX_FAILURES} times in \
                 {} minutes (last: {}); not restarting again. Check the log, then start \
                 e-lite again.",
                FAILURE_WINDOW.as_secs() / 60,
                exit.describe()
            );
            return Some(1);
        };
        restarts += 1;
        reason = exit.describe();
        eprintln!(
            "[e-lite] supervisor: the node ended abnormally ({reason}); restarting in {} s \
             (restart {restarts})",
            delay.as_secs()
        );
        let wake = Instant::now() + delay;
        while Instant::now() < wake {
            if stop.swap(false, Ordering::SeqCst) {
                return Some(0);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_abnormal_end_is_restarted() {
        // The deliberate abort after a panic, a crash, the OOM killer.
        for signal in [libc::SIGABRT, libc::SIGSEGV, libc::SIGBUS, libc::SIGKILL] {
            assert_eq!(decide(Exit::Signal(signal), false), Decision::Restart);
        }
        // An exit code is final: clean stop, configuration or integrity error.
        assert_eq!(decide(Exit::Code(0), false), Decision::Stop(0));
        assert_eq!(decide(Exit::Code(1), false), Decision::Stop(1));
        // Someone stopped the node itself.
        for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGQUIT] {
            assert_eq!(
                decide(Exit::Signal(signal), false),
                Decision::Stop(128 + signal)
            );
        }
        // A stop request is never followed by a restart, whatever happened.
        assert_eq!(
            decide(Exit::Signal(libc::SIGKILL), true),
            Decision::Stop(128 + libc::SIGKILL)
        );
        assert_eq!(
            decide(Exit::Signal(libc::SIGABRT), true),
            Decision::Stop(128 + libc::SIGABRT)
        );
    }

    #[test]
    fn restarts_back_off_and_give_up_in_a_crash_loop() {
        let t0 = Instant::now();
        let quick = Duration::from_secs(3);
        let mut backoff = Backoff::new();
        let mut now = t0;
        let mut delays = Vec::new();
        for _ in 0..MAX_FAILURES - 1 {
            let delay = backoff.next(now, quick).expect("still restarting");
            delays.push(delay.as_secs());
            now += delay + quick;
        }
        assert_eq!(delays, vec![1, 2, 4, 8]);
        // The fifth abnormal end inside ten minutes stops the loop.
        assert_eq!(backoff.next(now, quick), None);
    }

    #[test]
    fn rare_failures_never_give_up_and_a_healthy_run_resets_the_delay() {
        let mut backoff = Backoff::new();
        let mut now = Instant::now();
        // One abnormal end every 20 minutes of healthy running, for a day.
        for _ in 0..72 {
            now += Duration::from_secs(1200);
            assert_eq!(
                backoff.next(now, Duration::from_secs(1200)),
                Some(FIRST_DELAY)
            );
        }
        // The delay never exceeds the cap while failures stay spread out.
        let mut backoff = Backoff::new();
        let mut now = Instant::now();
        let mut last = Duration::ZERO;
        for _ in 0..12 {
            now += Duration::from_secs(200);
            last = backoff.next(now, Duration::from_secs(190)).unwrap();
        }
        assert_eq!(last, MAX_DELAY);
    }

    #[test]
    fn exit_descriptions_name_the_usual_causes() {
        assert!(Exit::Signal(libc::SIGABRT).describe().contains("SIGABRT"));
        assert!(Exit::Signal(libc::SIGKILL)
            .describe()
            .contains("out-of-memory"));
        assert_eq!(Exit::Code(3).describe(), "exit code 3");
    }
}
