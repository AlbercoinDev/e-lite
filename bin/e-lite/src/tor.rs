//! Managed Tor: a private Tor process with an onion service for Electrum.
//!
//! e-lite never edits `/etc/tor`: it renders its own `torrc` under the data
//! directory, starts `tor`, and reads the generated `hostname` file. Onion keys
//! live in `<data-dir>/tor/onion`; `e-lite onion reset` moves them aside.

use crate::config::Config;

pub use crate::tls::auto_cert_paths;
use el_model::Chain;
use el_serve::Shared;
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::{IpAddr, TcpListener};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub const DEFAULT_TLS_LISTEN: &str = "127.0.0.1:50002";

pub struct TorRuntime {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl TorRuntime {
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for TorRuntime {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Whether a target is safe to publish: only loopback listeners.
pub fn loopback_target(addr: &str) -> Result<(), String> {
    effective_target(addr).map(|_| ())
}

/// Turn a listener address into a loopback target Tor can connect to. Wildcard
/// listeners (`0.0.0.0`, `[::]`) accept loopback connections, so their onion
/// target is the loopback address; a specific non-loopback address is refused.
pub fn effective_target(addr: &str) -> Result<String, String> {
    let addr = addr.trim();
    let (host, port) = addr
        .rsplit_once(':')
        .ok_or_else(|| format!("target {addr:?} is not host:port"))?;
    let port: u16 = port
        .parse()
        .map_err(|_| format!("target {addr:?} has an invalid port"))?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let mapped = match host {
        "0.0.0.0" => format!("127.0.0.1:{port}"),
        "::" => format!("[::1]:{port}"),
        _ => {
            if host.eq_ignore_ascii_case("localhost") {
                return Ok(format!("localhost:{port}"));
            }
            let ip: IpAddr = host
                .parse()
                .map_err(|_| format!("target {addr:?} must be an IP or localhost"))?;
            if !ip.is_loopback() {
                return Err(format!(
                    "refusing to publish non-loopback target {addr:?}; bind Electrum to \
                     127.0.0.1/0.0.0.0 or set [tor] target_plain/target_tls to a loopback target"
                ));
            }
            format!("{ip}:{port}")
        }
    };
    Ok(mapped)
}

fn listener_port(addr: &str) -> Option<u16> {
    let (_, port) = addr.trim().rsplit_once(':')?;
    port.parse::<u16>().ok().filter(|p| *p > 0)
}

/// Onion virtual ports. Explicit `[tor] onion_port`/`onion_tls_port` win;
/// otherwise they mirror the Electrum listener ports, so `--listen 0.0.0.0:51001`
/// and `--tls-listen 0.0.0.0:51002` produce `:51001` and `:51002` on the onion.
pub fn effective_onion_ports(cfg: &Config) -> (u16, Option<u16>) {
    let plain = if cfg.tor.onion_port > 0 {
        cfg.tor.onion_port
    } else {
        listener_port(&cfg.server.listen).unwrap_or(50001)
    };
    let tls = if cfg.tor.tls && cfg.server.tls_enabled() {
        Some(if cfg.tor.onion_tls_port > 0 {
            cfg.tor.onion_tls_port
        } else {
            listener_port(&cfg.server.tls_listen).unwrap_or(50002)
        })
    } else {
        None
    };
    (plain, tls)
}

pub(crate) fn valid_onion(addr: &str) -> bool {
    let Some(name) = addr.strip_suffix(".onion") else {
        return false;
    };
    name.len() == 56 && name.bytes().all(|b| matches!(b, b'a'..=b'z' | b'2'..=b'7'))
}

pub fn read_hostname(tor_dir: &Path) -> Option<String> {
    let raw = fs::read_to_string(tor_dir.join("onion").join("hostname")).ok()?;
    let addr = raw.trim().to_ascii_lowercase();
    valid_onion(&addr).then_some(addr)
}

fn target_for(cfg: &Config, tls: bool) -> Result<String, String> {
    let configured = if tls {
        cfg.tor.target_tls.trim()
    } else {
        cfg.tor.target_plain.trim()
    };
    let listener = if tls {
        cfg.server.tls_listen.trim()
    } else {
        cfg.server.listen.trim()
    };
    let target = if !configured.is_empty() {
        configured
    } else {
        listener
    };
    if target.is_empty() {
        return Err("empty Electrum target".to_string());
    }
    effective_target(target)
}

fn render_torrc(cfg: &Config, tor_dir: &Path, socks_port: u16) -> Result<String, String> {
    let mut out = String::new();
    out.push_str("RunAsDaemon 0\n");
    out.push_str(&format!(
        "DataDirectory {}\n",
        tor_dir.join("data").display()
    ));
    out.push_str(&format!("SocksPort 127.0.0.1:{socks_port}\n"));
    let (plain_port, tls_port) = effective_onion_ports(cfg);
    if cfg.tor.plain || cfg.tor.tls {
        if cfg.tor.plain {
            if let Some(tls_port) = tls_port {
                if plain_port == tls_port {
                    return Err(format!(
                        "onion plain and TLS ports are both {plain_port}; set a different \
                         [tor] onion_port or onion_tls_port"
                    ));
                }
            }
        }
        out.push_str(&format!(
            "HiddenServiceDir {}\n",
            tor_dir.join("onion").display()
        ));
        if cfg.tor.plain {
            out.push_str(&format!(
                "HiddenServicePort {} {}\n",
                plain_port,
                target_for(cfg, false)?
            ));
        }
        if let Some(tls_port) = tls_port {
            out.push_str(&format!(
                "HiddenServicePort {} {}\n",
                tls_port,
                target_for(cfg, true)?
            ));
        }
    }
    out.push_str(&format!(
        "Log notice file {}\n",
        tor_dir.join("tor.log").display()
    ));
    Ok(out)
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let p = dir.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// The expert bundle ships its own libraries next to `tor` but the binary has
/// no RPATH, so the dynamic loader needs `LD_LIBRARY_PATH` pointing at that
/// directory. Prepend it to any inherited value.
fn library_path(dir: &Path, existing: Option<&OsStr>) -> OsString {
    let mut paths: Vec<PathBuf> = vec![dir.to_path_buf()];
    if let Some(existing) = existing {
        paths.extend(std::env::split_paths(existing));
    }
    match std::env::join_paths(paths) {
        Ok(joined) => joined,
        Err(_) => dir.as_os_str().to_os_string(),
    }
}

fn apply_tor_env(cmd: &mut Command, binary: &Path) {
    if let Some(dir) = binary.parent() {
        let existing = std::env::var_os("LD_LIBRARY_PATH");
        cmd.env("LD_LIBRARY_PATH", library_path(dir, existing.as_deref()));
    }
}

/// `tor --version` with the same environment e-lite uses to launch it. Used by
/// `doctor` to surface missing bundled/system libraries.
pub fn version(cfg: &Config) -> Result<String, String> {
    let binary = resolve_binary(cfg)
        .ok_or_else(|| "Tor binary not found (install it or rerun the installer)".to_string())?;
    let mut cmd = Command::new(&binary);
    apply_tor_env(&mut cmd, &binary);
    let out = cmd
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run {}: {e}", binary.display()))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = err.trim();
        return Err(if err.is_empty() {
            format!("{} exited with {}", binary.display(), out.status)
        } else {
            format!("{}: {err}", binary.display())
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string())
}

fn tail_log(path: &Path, lines: usize) -> String {
    let Ok(text) = fs::read_to_string(path) else {
        return String::new();
    };
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// `[tor] binary`, then the installer-provided expert bundle next to the
/// executable, then `tor` in PATH.
pub fn resolve_binary(cfg: &Config) -> Option<PathBuf> {
    if let Ok(env) = std::env::var("E_LITE_TOR") {
        let env = env.trim();
        if !env.is_empty() && Path::new(env).is_file() {
            return Some(PathBuf::from(env));
        }
    }
    let configured = cfg.tor.binary.trim();
    if !configured.is_empty() {
        return Some(PathBuf::from(configured));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for rel in ["../lib/e-lite/tor/tor", "tor"] {
                let p = dir.join(rel);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    }
    // .deb installs the binary in /usr/bin and the bundle in /usr/lib; user
    // installs may place it in /usr/local/lib.
    for p in ["/usr/lib/e-lite/tor/tor", "/usr/local/lib/e-lite/tor/tor"] {
        if Path::new(p).is_file() {
            return Some(PathBuf::from(p));
        }
    }
    find_in_path("tor")
}

fn port_free(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

fn choose_socks_port(cfg: &Config, chain: Chain) -> Result<u16, String> {
    if cfg.tor.socks_port > 0 {
        return Ok(cfg.tor.socks_port);
    }
    // Per-network defaults avoid a race when several managed instances start
    // together. Mainnet keeps 9050 for compatibility with existing setups.
    let preferred = chain.default_socks_port();
    if port_free(preferred) {
        return Ok(preferred);
    }
    if preferred != 9050 && port_free(9050) {
        return Ok(9050);
    }
    for port in 19_050..=19_100 {
        if port_free(port) {
            return Ok(port);
        }
    }
    Err(format!(
        "cannot find a free SOCKS port for the managed Tor ({preferred}, 9050 and          19050-19100 are busy)"
    ))
}

fn write_private(path: &Path, data: &[u8]) -> Result<(), String> {
    let mut f = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let mut perms = f
        .metadata()
        .map_err(|e| format!("{}: {e}", path.display()))?
        .permissions();
    perms.set_mode(0o600);
    let _ = f.set_permissions(perms);
    f.write_all(data)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Prepare the managed Tor for rawtx/onion use. TLS is handled separately by
/// `tls::prepare` and does not depend on Tor being available.
pub fn prepare(cfg: &mut Config, chain: Chain) -> Result<(), String> {
    if !cfg.tor.managed() {
        return Ok(());
    }
    if resolve_binary(cfg).is_none() {
        // Keep rawtx on its configured proxy; `spawn` logs the warning.
        return Ok(());
    }
    if cfg.tor.socks_port == 0 {
        cfg.tor.socks_port = choose_socks_port(cfg, chain)?;
    }
    // Use the managed Tor for rawtx too, unless the operator configured an
    // explicit non-default proxy.
    let default_proxy = cfg
        .rawtx
        .tor_socks
        .as_deref()
        .map(|s| {
            let s = s.trim();
            s.is_empty() || s == "127.0.0.1:9050"
        })
        .unwrap_or(true);
    if default_proxy {
        cfg.rawtx.tor_socks = Some(format!("127.0.0.1:{}", cfg.tor.socks_port));
    }
    let _ = chain;
    Ok(())
}

fn pid_alive(pid: u32) -> bool {
    // kill(pid, 0) reports EPERM for processes owned by other users; either way
    // the PID exists and we must not touch its onion keys.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

fn read_pid(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn terminate(child: &mut Child) {
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn log_has(path: &Path, needle: &str) -> bool {
    fs::read_to_string(path)
        .map(|s| s.contains(needle))
        .unwrap_or(false)
}

/// Start the managed Tor supervisor. Returns `None` when Tor is disabled.
/// The supervisor keeps the onion address in `Shared` and regenerates the
/// self-signed certificate when the address is first created.
pub fn spawn(
    cfg: &Config,
    chain: Chain,
    shared: Option<Arc<Shared>>,
    auto_cert: bool,
) -> Result<Option<TorRuntime>, String> {
    if !cfg.tor.managed() {
        return Ok(None);
    }
    let binary = resolve_binary(cfg).ok_or_else(|| {
        "Tor is not available: install the `tor` package or rerun the e-lite installer \
         with the Tor bundle"
            .to_string()
    })?;
    let tor_dir = cfg.tor_dir(chain);
    fs::create_dir_all(tor_dir.join("data")).map_err(|e| format!("{}: {e}", tor_dir.display()))?;
    fs::create_dir_all(tor_dir.join("onion")).map_err(|e| format!("{}: {e}", tor_dir.display()))?;
    for dir in [&tor_dir, &tor_dir.join("data"), &tor_dir.join("onion")] {
        let mut perms = fs::metadata(dir)
            .map_err(|e| format!("{}: {e}", dir.display()))?
            .permissions();
        perms.set_mode(0o700);
        fs::set_permissions(dir, perms).map_err(|e| format!("{}: {e}", dir.display()))?;
    }

    let socks = choose_socks_port(cfg, chain)?;
    let torrc = render_torrc(cfg, &tor_dir, socks)?;
    let torrc_path = tor_dir.join("torrc");
    write_private(&torrc_path, torrc.as_bytes())?;
    let (plain_port, tls_port) = effective_onion_ports(cfg);
    let target_plain = target_for(cfg, false)?;
    let target_tls = if tls_port.is_some() {
        Some(target_for(cfg, true)?)
    } else {
        None
    };
    write_state(
        &tor_dir,
        plain_port,
        tls_port,
        &target_plain,
        target_tls.as_deref(),
        &format!("127.0.0.1:{socks}"),
    );
    // Keep the Tor log bounded; it is append-only across restarts.
    let log_path = tor_dir.join("tor.log");
    if fs::metadata(&log_path).map(|m| m.len()).unwrap_or(0) > 10 * 1024 * 1024 {
        let _ = fs::rename(&log_path, tor_dir.join("tor.log.1"));
    }

    let stop = Arc::new(AtomicBool::new(false));
    let cfg = cfg.clone();
    let stop_thread = Arc::clone(&stop);
    let handle = std::thread::spawn(move || {
        supervise(
            cfg,
            chain,
            binary,
            torrc_path,
            tor_dir,
            stop_thread,
            shared,
            auto_cert,
        );
    });
    Ok(Some(TorRuntime {
        stop,
        handle: Some(handle),
    }))
}

#[allow(clippy::too_many_arguments)]
fn supervise(
    cfg: Config,
    chain: Chain,
    binary: PathBuf,
    torrc: PathBuf,
    tor_dir: PathBuf,
    stop: Arc<AtomicBool>,
    shared: Option<Arc<Shared>>,
    auto_cert: bool,
) {
    let log_path = tor_dir.join("tor.log");
    let pid_path = tor_dir.join("tor.pid");
    let timeout = Duration::from_secs(cfg.tor.start_timeout_seconds.clamp(5, 600));
    let mut attempts = 0u32;
    while !stop.load(Ordering::SeqCst) {
        let log = match OpenOptions::new().create(true).append(true).open(&log_path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("[e-lite] WARNING: Tor: {}: {e}", log_path.display());
                return;
            }
        };
        let mut command = Command::new(&binary);
        apply_tor_env(&mut command, &binary);
        let spawned = command
            .arg("-f")
            .arg(&torrc)
            .stdin(Stdio::null())
            .stdout(
                log.try_clone()
                    .map(Stdio::from)
                    .unwrap_or_else(|_| Stdio::null()),
            )
            .stderr(Stdio::from(log))
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                eprintln!(
                    "[e-lite] WARNING: Tor: cannot start {}: {e}",
                    binary.display()
                );
                return;
            }
        };
        let pid = child.id();
        let _ = write_private(&pid_path, format!("{pid}\n").as_bytes());
        let started = Instant::now();
        let mut announced = false;
        let mut bootstrapped = false;
        loop {
            if stop.load(Ordering::SeqCst) {
                terminate(&mut child);
                let _ = fs::remove_file(&pid_path);
                return;
            }
            if let Some(addr) = read_hostname(&tor_dir) {
                if !announced {
                    if let Some(sh) = &shared {
                        if let Ok(mut dst) = sh.onion.lock() {
                            *dst = Some(addr.clone());
                        }
                    }
                    if auto_cert {
                        if let Err(e) = crate::tls::ensure_self_signed(&cfg, chain, Some(&addr)) {
                            eprintln!("[e-lite] WARNING: cannot refresh TLS certificate: {e}");
                        }
                    }
                    let (plain_port, tls_port) = effective_onion_ports(&cfg);
                    eprintln!(
                        "[e-lite] Tor onion: {addr}:{plain_port} (plain){}",
                        match tls_port {
                            Some(port) => format!(", :{port} (tls)"),
                            None => String::new(),
                        }
                    );
                    announced = true;
                }
            }
            if !bootstrapped && log_has(&log_path, "Bootstrapped 100%") {
                bootstrapped = true;
                if let Some(sh) = &shared {
                    sh.stats.tor_bootstrapped.store(true, Ordering::Relaxed);
                }
                eprintln!("[e-lite] Tor ready (bootstrapped 100%)");
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    let detail = tail_log(&log_path, 5);
                    if detail.is_empty() {
                        eprintln!(
                            "[e-lite] WARNING: Tor: managed process exited ({status}); retrying"
                        );
                    } else {
                        eprintln!(
                            "[e-lite] WARNING: Tor: managed process exited ({status}); retrying. \
                             Last tor.log lines:\n{detail}"
                        );
                    }
                    break;
                }
                Ok(None) => {}
                Err(e) => {
                    eprintln!("[e-lite] WARNING: Tor: managed process wait failed: {e}");
                    break;
                }
            }
            if !announced && started.elapsed() > timeout {
                eprintln!(
                    "[e-lite] WARNING: Tor: no onion created within {}s; check {}",
                    timeout.as_secs(),
                    log_path.display()
                );
                announced = true;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        let _ = fs::remove_file(&pid_path);
        if stop.load(Ordering::SeqCst) {
            return;
        }
        attempts = attempts.saturating_add(1);
        if attempts >= 5 {
            eprintln!("[e-lite] WARNING: Tor: managed process keeps exiting; giving up");
            return;
        }
        let backoff = Duration::from_secs(1 << attempts.min(4));
        let deadline = Instant::now() + backoff;
        while Instant::now() < deadline && !stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(250));
        }
    }
}

/// Report used by `e-lite onion show` and `doctor`.
pub struct TorStatus {
    pub mode: String,
    pub binary: Option<PathBuf>,
    pub socks: String,
    pub onion: Option<String>,
    pub pid: Option<u32>,
    pub running: bool,
    pub target_plain: String,
    pub target_tls: String,
    pub plain_port: u16,
    pub tls_port: Option<u16>,
}

fn read_socks_from_torrc(tor_dir: &Path) -> Option<String> {
    let text = fs::read_to_string(tor_dir.join("torrc")).ok()?;
    text.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("SocksPort ")?;
        Some(format!("127.0.0.1:{}", rest.trim().rsplit(':').next()?))
    })
}

fn write_state(
    tor_dir: &Path,
    plain_port: u16,
    tls_port: Option<u16>,
    target_plain: &str,
    target_tls: Option<&str>,
    socks: &str,
) {
    let mut text = format!("plain_port={plain_port}\ntarget_plain={target_plain}\nsocks={socks}\n");
    if let Some(port) = tls_port {
        text.push_str(&format!("tls_port={port}\n"));
    }
    if let Some(target) = target_tls {
        text.push_str(&format!("target_tls={target}\n"));
    }
    let _ = write_private(&tor_dir.join("state"), text.as_bytes());
}

#[derive(Default)]
struct SavedState {
    plain_port: Option<u16>,
    tls_port: Option<u16>,
    target_plain: Option<String>,
    target_tls: Option<String>,
    socks: Option<String>,
}

fn read_state(tor_dir: &Path) -> SavedState {
    let mut out = SavedState::default();
    let Ok(text) = fs::read_to_string(tor_dir.join("state")) else {
        return out;
    };
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().to_string();
        match key.trim() {
            "plain_port" => out.plain_port = value.parse().ok(),
            "tls_port" => out.tls_port = value.parse().ok(),
            "target_plain" => out.target_plain = Some(value),
            "target_tls" => out.target_tls = Some(value),
            "socks" => out.socks = Some(value),
            _ => {}
        }
    }
    out
}

pub fn status(cfg: &Config, chain: Chain) -> TorStatus {
    let tor_dir = cfg.tor_dir(chain);
    let pid = read_pid(&tor_dir.join("tor.pid"));
    let running = pid.map(pid_alive).unwrap_or(false);
    let saved = read_state(&tor_dir);
    let has_state = saved.plain_port.is_some();
    let (eff_plain, eff_tls) = effective_onion_ports(cfg);
    let plain_port = saved.plain_port.unwrap_or(eff_plain);
    let tls_port = saved.tls_port.or(if has_state { None } else { eff_tls });
    TorStatus {
        mode: cfg.tor.mode.clone(),
        binary: resolve_binary(cfg),
        socks: if let Some(socks) = saved.socks.clone().filter(|s| !s.is_empty()) {
            socks
        } else if cfg.tor.socks_port > 0 {
            format!("127.0.0.1:{}", cfg.tor.socks_port)
        } else {
            read_socks_from_torrc(&tor_dir).unwrap_or_else(|| "auto".to_string())
        },
        onion: read_hostname(&tor_dir),
        pid,
        running,
        target_plain: saved
            .target_plain
            .filter(|s| !s.is_empty())
            .or_else(|| target_for(cfg, false).ok())
            .unwrap_or_else(|| cfg.server.listen.clone()),
        target_tls: if tls_port.is_some() {
            saved
                .target_tls
                .filter(|s| !s.is_empty())
                .or_else(|| target_for(cfg, true).ok())
                .unwrap_or_else(|| DEFAULT_TLS_LISTEN.to_string())
        } else {
            String::new()
        },
        plain_port,
        tls_port,
    }
}

/// Move the onion keys aside so the next start generates a new address.
pub fn reset(cfg: &Config, chain: Chain, purge: bool) -> Result<String, String> {
    let tor_dir = cfg.tor_dir(chain);
    if let Some(pid) = read_pid(&tor_dir.join("tor.pid")) {
        if pid_alive(pid) {
            return Err(format!(
                "managed Tor (pid {pid}) is running; stop e-lite before resetting the onion"
            ));
        }
        let _ = fs::remove_file(tor_dir.join("tor.pid"));
    }
    let old = read_hostname(&tor_dir).unwrap_or_else(|| "(none)".to_string());
    let onion_dir = tor_dir.join("onion");
    if onion_dir.exists() {
        if purge {
            fs::remove_dir_all(&onion_dir).map_err(|e| format!("{}: {e}", onion_dir.display()))?;
        } else {
            let prev = tor_dir.join("onion.prev");
            if prev.exists() {
                fs::remove_dir_all(&prev).map_err(|e| format!("{}: {e}", prev.display()))?;
            }
            fs::rename(&onion_dir, &prev).map_err(|e| format!("{}: {e}", onion_dir.display()))?;
        }
    }
    // Drop the certificate only when e-lite manages it, so the next start
    // regenerates it with the new onion SAN. Operator PEMs are never touched.
    let (cert, key) = auto_cert_paths(cfg, chain);
    if cfg.server.tls_cert.trim() == cert.display().to_string() {
        let _ = fs::remove_file(&cert);
        let _ = fs::remove_file(&key);
        if let Some(dir) = cert.parent() {
            let _ = fs::remove_file(dir.join("self-signed.san"));
        }
    }
    Ok(old)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn temp_cfg(name: &str) -> (Config, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "e-lite-tor-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut cfg = Config::default();
        cfg.network.name = "regtest".to_string();
        cfg.network.data_dir = Some(dir.display().to_string());
        cfg.server.listen = "127.0.0.1:50001".to_string();
        cfg.server.tls_listen = "127.0.0.1:50002".to_string();
        (cfg, dir)
    }

    #[test]
    fn onion_ports_mirror_the_electrum_listeners_unless_overridden() {
        let (mut cfg, dir) = temp_cfg("ports");
        cfg.server.listen = "0.0.0.0:51001".to_string();
        cfg.server.tls_listen = "0.0.0.0:51002".to_string();
        assert_eq!(effective_onion_ports(&cfg), (51001, Some(51002)));
        let text = render_torrc(&cfg, &dir, 19_050).unwrap();
        assert!(text.contains("HiddenServicePort 51001 127.0.0.1:51001"));
        assert!(text.contains("HiddenServicePort 51002 127.0.0.1:51002"));

        cfg.tor.onion_port = 60001;
        assert_eq!(effective_onion_ports(&cfg), (60001, Some(51002)));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn torrc_contains_both_onion_ports_and_loopback_targets() {
        let (mut cfg, dir) = temp_cfg("torrc");
        cfg.tor.plain = true;
        cfg.tor.tls = true;
        let text = render_torrc(&cfg, &dir, 19_050).unwrap();
        assert!(text.contains("SocksPort 127.0.0.1:19050"));
        assert!(text.contains("HiddenServicePort 50001 127.0.0.1:50001"));
        assert!(text.contains("HiddenServicePort 50002 127.0.0.1:50002"));
        assert!(text.contains("RunAsDaemon 0"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn wildcard_listeners_are_mapped_to_loopback_targets() {
        assert_eq!(
            effective_target("0.0.0.0:50001").unwrap(),
            "127.0.0.1:50001"
        );
        assert_eq!(effective_target("[::]:50001").unwrap(), "[::1]:50001");
        assert_eq!(
            effective_target("127.0.0.1:50001").unwrap(),
            "127.0.0.1:50001"
        );
        assert_eq!(
            effective_target("localhost:50001").unwrap(),
            "localhost:50001"
        );
        assert!(effective_target("192.168.1.2:50001").is_err());
        assert!(effective_target("0.0.0.0").is_err());
    }

    #[test]
    fn torrc_maps_wildcard_listeners_and_honours_explicit_targets() {
        let (mut cfg, dir) = temp_cfg("wildcard");
        cfg.server.listen = "0.0.0.0:50001".to_string();
        cfg.server.tls_listen = "0.0.0.0:50002".to_string();
        let text = render_torrc(&cfg, &dir, 19_050).unwrap();
        assert!(text.contains("HiddenServicePort 50001 127.0.0.1:50001"));
        assert!(text.contains("HiddenServicePort 50002 127.0.0.1:50002"));

        cfg.tor.target_plain = "127.0.0.1:49001".to_string();
        let text = render_torrc(&cfg, &dir, 19_050).unwrap();
        assert!(text.contains("HiddenServicePort 50001 127.0.0.1:49001"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tor_library_path_prepends_the_bundle_directory() {
        use std::ffi::{OsStr, OsString};
        let dir = Path::new("/opt/e-lite/tor");
        assert_eq!(library_path(dir, None), OsString::from("/opt/e-lite/tor"));
        assert_eq!(
            library_path(dir, Some(OsStr::new("/usr/lib:/lib"))),
            OsString::from("/opt/e-lite/tor:/usr/lib:/lib")
        );
    }

    #[test]
    fn onion_address_validation() {
        let good = format!("{}.onion", "a".repeat(56));
        assert!(valid_onion(&good));
        assert!(!valid_onion(&format!("{}.onion", "A".repeat(56))));
        assert!(!valid_onion("short.onion"));
        assert!(!valid_onion("example.com"));
    }

    #[test]
    fn self_signed_certificate_is_written_with_onion_san() {
        let (mut cfg, dir) = temp_cfg("cert");
        cfg.server.tls_cert.clear();
        cfg.server.tls_key.clear();
        let onion = format!("{}.onion", "b".repeat(56));
        let (cert, key) =
            crate::tls::ensure_self_signed(&cfg, Chain::Regtest, Some(&onion)).unwrap();
        assert!(Path::new(&cert).is_file());
        assert!(Path::new(&key).is_file());
        let mode = fs::metadata(&key).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let san = fs::read_to_string(dir.join("tls/self-signed.san")).unwrap();
        assert!(san.contains(&onion));
        // The same SAN reuses the files instead of regenerating them.
        let before = fs::metadata(&cert).unwrap().modified().unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let _ = crate::tls::ensure_self_signed(&cfg, Chain::Regtest, Some(&onion)).unwrap();
        assert_eq!(before, fs::metadata(&cert).unwrap().modified().unwrap());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn managed_tor_creates_the_onion_with_a_fake_binary() {
        let (mut cfg, dir) = temp_cfg("fake");
        cfg.tor.mode = "managed".to_string();
        cfg.tor.plain = true;
        cfg.tor.tls = false;
        cfg.server.tls_listen.clear();
        cfg.tor.socks_port = 0;
        let script = dir.join("fake-tor.sh");
        let hostname = format!("{}.onion", "d".repeat(56));
        let body = format!(
            "#!/bin/sh\n\
             dir=\"\"\n\
             while IFS= read -r line; do\n\
               case \"$line\" in HiddenServiceDir*) dir=\"${{line#HiddenServiceDir }}\" ;; esac\n\
             done < \"$2\"\n\
             mkdir -p \"$dir\"\n\
             printf '%s.onion\\n' \"{}\" > \"$dir/hostname\"\n\
             exec sleep 30\n",
            "d".repeat(56)
        );
        fs::write(&script, body).unwrap();
        let mut perms = fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o700);
        fs::set_permissions(&script, perms).unwrap();
        cfg.tor.binary = script.display().to_string();
        prepare(&mut cfg, Chain::Regtest).unwrap();
        let mut rt = spawn(&cfg, Chain::Regtest, None, false)
            .unwrap()
            .expect("managed runtime");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut found = None;
        while Instant::now() < deadline {
            found = read_hostname(&cfg.tor_dir(Chain::Regtest));
            if found.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(found.as_deref(), Some(hostname.as_str()));
        rt.stop();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "needs a real tor binary: set E_LITE_REAL_TOR=/path/to/tor"]
    fn managed_tor_real_binary_creates_onion() {
        let Ok(binary) = std::env::var("E_LITE_REAL_TOR") else {
            return;
        };
        let (mut cfg, dir) = temp_cfg("real");
        cfg.tor.mode = "managed".to_string();
        cfg.tor.plain = true;
        cfg.tor.tls = false;
        cfg.server.tls_listen.clear();
        cfg.tor.socks_port = 0;
        cfg.tor.binary = binary;
        prepare(&mut cfg, Chain::Regtest).unwrap();
        let mut rt = spawn(&cfg, Chain::Regtest, None, false)
            .unwrap()
            .expect("managed runtime");
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut found = None;
        while Instant::now() < deadline {
            found = read_hostname(&cfg.tor_dir(Chain::Regtest));
            if found.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        assert!(found.is_some(), "real Tor did not create an onion");
        rt.stop();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reset_moves_onion_keys_and_auto_cert() {
        let (mut cfg, dir) = temp_cfg("reset");
        cfg.apply_network_defaults(Chain::Regtest);
        let onion_dir = dir.join("tor/onion");
        fs::create_dir_all(&onion_dir).unwrap();
        let addr = format!("{}.onion", "c".repeat(56));
        fs::write(onion_dir.join("hostname"), format!("{addr}\n")).unwrap();
        let (cert, _key) =
            crate::tls::ensure_self_signed(&cfg, Chain::Regtest, Some(&addr)).unwrap();
        assert!(Path::new(&cert).is_file());
        let (ca_cert, ca_key) = crate::tls::auto_ca_paths(&cfg, Chain::Regtest);
        assert!(ca_cert.is_file() && ca_key.is_file());
        let ca_before = fs::read(&ca_cert).unwrap();
        let old = reset(&cfg, Chain::Regtest, false).unwrap();
        assert_eq!(old, addr);
        assert!(!onion_dir.exists());
        assert!(dir.join("tor/onion.prev").exists());
        assert!(!Path::new(&cert).exists());
        // The CA survives an onion reset: the new leaf is signed with the same
        // CA, so wallets do not need to import a new trust anchor.
        assert_eq!(ca_before, fs::read(&ca_cert).unwrap());
        let _ = fs::remove_dir_all(&dir);
    }
}
