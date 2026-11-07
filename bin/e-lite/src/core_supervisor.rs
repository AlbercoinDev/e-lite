//! Starting, adopting, supervising and stopping Bitcoin Core.
//!
//! e-lite only controls a Core process it started itself. A Core that was
//! already running is used read-only: e-lite never pauses, prunes or stops a
//! node it does not own.

use crate::config::{Config, CoreManage};
use el_model::Chain;
use el_rpc::CoreClient;
use serde_json::json;
use std::fs::{self, OpenOptions};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreStartOutcome {
    /// `manage = external`: e-lite never starts Core.
    External,
    /// Core is already reachable and was not started by e-lite.
    AlreadyRunning,
    /// e-lite started Core.
    Started,
    /// e-lite adopted a Core child from a previous run (pid file).
    Adopted,
    /// A previous start is still in backoff; try again later.
    Backoff,
}

pub struct CoreSupervisor {
    manage: CoreManage,
    bitcoind: String,
    chain: Chain,
    datadir: PathBuf,
    rpc_port: u16,
    prune_mib: u64,
    extra_args: Vec<String>,
    restart: bool,
    stop_on_exit: bool,
    log_file: PathBuf,
    pid_file: PathBuf,
    child: Option<Child>,
    owned_pid: Option<u32>,
    last_start: Option<Instant>,
    backoff: u64,
    last_error: Option<String>,
}

impl CoreSupervisor {
    pub fn new(cfg: &Config, chain: Chain) -> Self {
        CoreSupervisor {
            manage: cfg.core.manage,
            bitcoind: cfg.core.bitcoind.clone(),
            chain,
            datadir: cfg.core_datadir(chain),
            rpc_port: rpc_port_from_url(&cfg.rpc_url(chain), chain.default_rpc_port()),
            prune_mib: cfg.core.prune_mib,
            extra_args: cfg.core.extra_args.clone(),
            restart: cfg.core.restart,
            stop_on_exit: cfg.core.stop_on_exit,
            log_file: cfg.log_file(chain),
            pid_file: cfg.pid_file(chain),
            child: None,
            owned_pid: None,
            last_start: None,
            backoff: 1,
            last_error: None,
        }
    }

    /// True when e-lite owns the running Core and may pause/prune/stop it.
    pub fn can_control(&self) -> bool {
        self.owned_pid.is_some() || self.child.is_some()
    }

    pub fn can_control_core(&self) -> bool {
        self.can_control()
    }

    /// Make sure Core is running when `manage` allows it. Never blocks for the
    /// full startup; callers retry until RPC answers.
    pub fn ensure_started(&mut self, core: &CoreClient) -> Result<CoreStartOutcome, String> {
        if self.manage == CoreManage::External {
            return Ok(CoreStartOutcome::External);
        }

        // Reap a child that exited.
        if let Some(child) = self.child.as_mut() {
            match child.try_wait() {
                Ok(None) => return Ok(CoreStartOutcome::AlreadyRunning),
                Ok(Some(status)) => {
                    eprintln!(
                        "[e-lite] Core (pid {}) exited with {status}",
                        self.owned_pid.unwrap_or(0)
                    );
                    self.child = None;
                    self.owned_pid = None;
                    let _ = fs::remove_file(&self.pid_file);
                }
                Err(e) => {
                    self.last_error = Some(format!("cannot poll Core child: {e}"));
                }
            }
        }

        // Adopt a child left behind by a previous e-lite run.
        if self.owned_pid.is_none() {
            if let Some(pid) = read_pid(&self.pid_file) {
                if process_alive(pid)
                    && process_is_bitcoind(pid)
                    && process_matches_datadir(pid, &self.datadir)
                {
                    self.owned_pid = Some(pid);
                    eprintln!(
                        "[e-lite] adopted Core pid {pid} from {}",
                        self.pid_file.display()
                    );
                    return Ok(CoreStartOutcome::Adopted);
                }
                let _ = fs::remove_file(&self.pid_file);
            }
        }

        // If Core already answers, use it read-only (auto) or reject (managed
        // when e-lite did not start it).
        if core.blockchain_info().is_ok() {
            if self.manage == CoreManage::Managed && !self.can_control() {
                return Err(
                    "Bitcoin Core is already running but was not started by e-lite; \
                     stop it first or use [core] manage = \"auto\"/\"external\""
                        .to_string(),
                );
            }
            return Ok(CoreStartOutcome::AlreadyRunning);
        }

        if !self.restart && self.last_start.is_some() {
            return Ok(CoreStartOutcome::Backoff);
        }
        if let Some(last) = self.last_start {
            if last.elapsed() < Duration::from_secs(self.backoff) {
                return Ok(CoreStartOutcome::Backoff);
            }
        }

        match self.spawn() {
            Ok(()) => Ok(CoreStartOutcome::Started),
            Err(e) => {
                self.last_error = Some(e.clone());
                self.last_start = Some(Instant::now());
                self.backoff = (self.backoff.saturating_mul(2)).clamp(1, 60);
                Err(e)
            }
        }
    }

    fn spawn(&mut self) -> Result<(), String> {
        fs::create_dir_all(&self.datadir)
            .map_err(|e| format!("cannot create Core datadir {}: {e}", self.datadir.display()))?;
        if let Some(parent) = self.log_file.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create Core log dir {}: {e}", parent.display()))?;
        }
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_file)
            .map_err(|e| format!("cannot open Core log {}: {e}", self.log_file.display()))?;

        let chain_arg = match self.chain {
            Chain::Mainnet => "main",
            Chain::Testnet4 => "testnet4",
            Chain::Signet => "signet",
            Chain::Regtest => "regtest",
        };

        let mut cmd = Command::new(&self.bitcoind);
        cmd.arg(format!("-datadir={}", self.datadir.display()));
        cmd.arg(format!("-chain={chain_arg}"));
        cmd.arg("-daemon=0");
        cmd.arg("-server=1");
        if self.prune_mib > 0 {
            cmd.arg(format!("-prune={}", self.prune_mib));
        }
        cmd.arg("-rpcbind=127.0.0.1");
        cmd.arg(format!("-rpcport={}", self.rpc_port));
        cmd.arg("-rpcallowip=127.0.0.1");
        for arg in &self.extra_args {
            cmd.arg(arg);
        }
        cmd.stdin(Stdio::null());
        cmd.stdout(log.try_clone().map_err(|e| e.to_string())?);
        cmd.stderr(log);

        eprintln!(
            "[e-lite] starting Core: {} -datadir={} -chain={} -prune={}",
            self.bitcoind,
            self.datadir.display(),
            chain_arg,
            if self.prune_mib > 0 {
                self.prune_mib.to_string()
            } else {
                "0 (full node)".to_string()
            }
        );
        let child = cmd
            .spawn()
            .map_err(|e| format!("cannot start {}: {e}", self.bitcoind))?;
        let pid = child.id();
        fs::write(&self.pid_file, pid.to_string()).map_err(|e| {
            format!(
                "cannot write Core pid file {}: {e}",
                self.pid_file.display()
            )
        })?;
        self.child = Some(child);
        self.owned_pid = Some(pid);
        self.last_start = Some(Instant::now());
        self.backoff = 1;
        self.last_error = None;
        Ok(())
    }

    /// Stop Core only when e-lite owns it and `stop_on_exit` is enabled.
    pub fn stop(&mut self) {
        if self.owned_pid.is_none() && self.child.is_none() {
            return;
        }
        if !self.stop_on_exit {
            eprintln!("[e-lite] leaving Core running (stop_on_exit = false)");
            return;
        }
        if let Some(mut child) = self.child.take() {
            let pid = child.id();
            let _ = terminate(pid);
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(200));
                    }
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                }
            }
            eprintln!("[e-lite] Core pid {pid} stopped");
        } else if let Some(pid) = self.owned_pid {
            let _ = terminate(pid);
            let deadline = Instant::now() + Duration::from_secs(30);
            while Instant::now() < deadline
                && process_alive(pid)
                && process_is_bitcoind(pid)
                && process_matches_datadir(pid, &self.datadir)
            {
                std::thread::sleep(Duration::from_millis(200));
            }
            if process_alive(pid)
                && process_is_bitcoind(pid)
                && process_matches_datadir(pid, &self.datadir)
            {
                let _ = kill(pid);
            }
            eprintln!("[e-lite] Core pid {pid} stopped");
        }
        self.owned_pid = None;
        let _ = fs::remove_file(&self.pid_file);
        let _ = fs::remove_file(self.pid_file.with_file_name("core-paused-by-e-lite"));
    }

    pub fn status_json(&self) -> serde_json::Value {
        json!({
            "manage": match self.manage {
                CoreManage::External => "external",
                CoreManage::Auto => "auto",
                CoreManage::Managed => "managed",
            },
            "owned": self.can_control(),
            "pid": self.owned_pid,
            "bitcoind": self.bitcoind,
            "datadir": self.datadir.display().to_string(),
            "rpc_port": self.rpc_port,
            "prune_mib": self.prune_mib,
            "log_file": self.log_file.display().to_string(),
            "last_error": self.last_error,
        })
    }
}

impl Drop for CoreSupervisor {
    fn drop(&mut self) {
        self.stop();
    }
}

fn rpc_port_from_url(url: &str, default_port: u16) -> u16 {
    let rest = url.strip_prefix("http://").unwrap_or(url);
    let rest = rest.trim_end_matches('/');
    if rest.starts_with('[') {
        if let Some(tail) = rest.split_once(']').map(|(_, t)| t) {
            if let Some(port) = tail.strip_prefix(':') {
                return port.parse().unwrap_or(default_port);
            }
        }
        return default_port;
    }
    rest.rsplit_once(':')
        .and_then(|(_, port)| port.parse().ok())
        .unwrap_or(default_port)
}

fn read_pid(path: &std::path::Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn process_alive(pid: u32) -> bool {
    std::path::Path::new("/proc").join(pid.to_string()).exists()
}

fn process_is_bitcoind(pid: u32) -> bool {
    let comm = std::path::Path::new("/proc")
        .join(pid.to_string())
        .join("comm");
    fs::read_to_string(comm)
        .map(|s| s.trim().starts_with("bitcoind"))
        .unwrap_or(false)
}

fn process_matches_datadir(pid: u32, datadir: &std::path::Path) -> bool {
    let cmdline = std::path::Path::new("/proc")
        .join(pid.to_string())
        .join("cmdline");
    let Ok(raw) = fs::read(cmdline) else {
        return false;
    };
    let needle = format!("-datadir={}", datadir.display());
    raw.split(|b| *b == 0).any(|arg| arg == needle.as_bytes())
}

#[cfg(unix)]
fn terminate(pid: u32) -> std::io::Result<()> {
    unsafe {
        if libc::kill(pid as libc::pid_t, libc::SIGTERM) == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}

#[cfg(unix)]
fn kill(pid: u32) -> std::io::Result<()> {
    unsafe {
        if libc::kill(pid as libc::pid_t, libc::SIGKILL) == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}

#[cfg(not(unix))]
fn terminate(_pid: u32) -> std::io::Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn kill(_pid: u32) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_never_starts() {
        let mut cfg = Config::default();
        cfg.core.manage = CoreManage::External;
        let mut sup = CoreSupervisor::new(&cfg, Chain::Regtest);
        // A CoreClient is required by the signature; construction does not connect.
        let core = CoreClient::connect(&el_rpc::CoreConfig {
            rpc_url: "http://127.0.0.1:1".to_string(),
            cookie_file: None,
            core_datadir: Some(std::env::temp_dir()),
            rpc_user: Some("user".to_string()),
            rpc_password: Some("pass".to_string()),
            chain: Chain::Regtest,
            timeout: Duration::from_millis(50),
        })
        .unwrap();
        assert_eq!(
            sup.ensure_started(&core).unwrap(),
            CoreStartOutcome::External
        );
        assert!(!sup.can_control());
    }

    #[test]
    fn pid_helpers_are_safe() {
        assert!(process_alive(std::process::id()));
        assert!(!process_alive(0));
        // The test process is not bitcoind.
        assert!(!process_is_bitcoind(std::process::id()));
    }
}
