//! Editable public provider list.
//!
//! The server list is **not** compiled into the binary.  e-lite reads
//! `public-servers.toml` next to the active `config.toml`; when the file is
//! missing on a first run it copies the shipped template.  If neither the file
//! nor a template exists, e-lite runs with no public providers (fail closed)
//! instead of falling back to a hidden list in code.

use crate::config::Config;
use el_model::Chain;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct NetworkServers {
    #[serde(default)]
    esplora: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ProviderStatus {
    pub url: String,
    pub healthy: bool,
    pub latency_ms: u64,
    /// `tor`, `clearnet` or `none`.
    pub via: String,
    pub error: Option<String>,
}

impl ProviderStatus {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "url": self.url,
            "healthy": self.healthy,
            "latency_ms": self.latency_ms,
            "via": self.via,
            "error": self.error,
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct ProbeOutcome {
    pub statuses: Vec<ProviderStatus>,
    pub ready: bool,
    pub tor_ready: bool,
    pub clearnet_fallback: bool,
}

/// After the first healthy provider, the gate waits this long for the rest
/// of the round before opening; slower providers are reported as such.
const GATE_GRACE: Duration = Duration::from_secs(2);

/// Probe the providers and wait (up to `cfg.public_servers.ready_timeout_seconds`)
/// until at least one answers. With a known genesis each provider gets one
/// constant request, `/block/<genesis>/raw`, checked against the header hash;
/// otherwise `/block-height/0` first. Neither contains wallet data.
///
/// Providers are probed in parallel, each through its own Tor circuit, so a
/// round lasts one timeout instead of one per provider. A Tor attempt gets at
/// least twice `rawtx.tor_stall_timeout_seconds` (24 s by default, the onion
/// limit of a block attempt): a cold rendezvous circuit to an onion service
/// took up to 14 s to the first byte on mainnet. The genesis block is tiny,
/// so the long whole-block attempt limit is not needed.
/// Least time a Tor probe attempt gets (see `probe`).
pub fn probe_tor_floor(cfg: &Config) -> Duration {
    Duration::from_secs(cfg.rawtx.tor_stall_timeout_seconds.saturating_mul(2))
}

pub fn probe(
    cfg: &Config,
    urls: &[String],
    wait_for_ready: bool,
    expected_genesis: Option<&str>,
) -> ProbeOutcome {
    if urls.is_empty() {
        return ProbeOutcome::default();
    }
    let timeout = Duration::from_secs(cfg.public_servers.probe_timeout_seconds.max(1));
    let plan = ProbePlan {
        tor_socks: cfg
            .rawtx
            .tor_socks
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        tor_timeout: timeout.max(probe_tor_floor(cfg)),
        direct: reqwest::blocking::Client::builder()
            .timeout(timeout)
            .user_agent(el_rawtx::HTTP_USER_AGENT)
            .build()
            .ok(),
        allow_clearnet: cfg.rawtx.allow_clearnet,
        fallback: cfg.rawtx.tor_fallback_clearnet,
        expected_genesis: expected_genesis.map(str::to_ascii_lowercase),
    };
    // `doctor` (no wait) reports every provider; the gate opens shortly
    // after the first healthy one.
    let grace = wait_for_ready.then_some(GATE_GRACE);

    let deadline =
        Instant::now() + Duration::from_secs(cfg.public_servers.ready_timeout_seconds.max(1));
    loop {
        let outcome = probe_round(urls, &plan, grace);
        if outcome.ready || !wait_for_ready || Instant::now() >= deadline {
            if !outcome.ready && wait_for_ready {
                eprintln!(
                    "[e-lite] WARNING: no public provider answered for {} after {}s",
                    cfg.network.name, cfg.public_servers.ready_timeout_seconds
                );
            }
            return outcome;
        }
        eprintln!(
            "[e-lite] waiting for a public provider ({} configured, retrying)",
            urls.len()
        );
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// Everything one provider probe needs, shareable with its thread.
#[derive(Clone)]
struct ProbePlan {
    tor_socks: Option<String>,
    tor_timeout: Duration,
    direct: Option<reqwest::blocking::Client>,
    allow_clearnet: bool,
    fallback: bool,
    expected_genesis: Option<String>,
}

/// Result of one provider probe; `clearnet_fallback` marks a provider that
/// was only reachable directly although Tor should have been used.
struct ProbeResult {
    status: ProviderStatus,
    via_tor: bool,
    clearnet_fallback: bool,
}

/// Probe every provider in parallel. Statuses keep the configured order.
/// With `grace`, providers still running `grace` after the first healthy
/// answer are reported as slow and left to finish in the background (their
/// requests are bounded by the client timeouts).
fn probe_round(urls: &[String], plan: &ProbePlan, grace: Option<Duration>) -> ProbeOutcome {
    let started = Instant::now();
    let (tx, rx) = std::sync::mpsc::channel::<(usize, ProbeResult)>();
    let mut spawned = 0;
    let mut results: Vec<Option<ProbeResult>> = urls.iter().map(|_| None).collect();
    for (idx, url) in urls.iter().enumerate() {
        let tx = tx.clone();
        let url = url.clone();
        let plan = plan.clone();
        let thread = std::thread::Builder::new()
            .name("el-probe".to_string())
            .spawn(move || {
                let _ = tx.send((idx, probe_provider(&url, &plan)));
            });
        match thread {
            Ok(_) => spawned += 1,
            Err(e) => {
                results[idx] = Some(ProbeResult {
                    status: ProviderStatus {
                        url: url_base(urls[idx].as_str()).to_string(),
                        error: Some(format!("cannot start probe thread: {e}")),
                        ..Default::default()
                    },
                    via_tor: false,
                    clearnet_fallback: false,
                })
            }
        }
    }
    drop(tx);

    let mut received = 0;
    let mut cutoff: Option<Instant> = None;
    while received < spawned {
        let next = match cutoff {
            Some(at) => {
                let left = at.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                rx.recv_timeout(left).map_err(|_| ())
            }
            None => rx.recv().map_err(|_| ()),
        };
        let Ok((idx, result)) = next else {
            break;
        };
        if result.status.healthy && cutoff.is_none() {
            cutoff = grace.map(|g| Instant::now() + g);
        }
        results[idx] = Some(result);
        received += 1;
    }

    let mut outcome = ProbeOutcome::default();
    for (idx, result) in results.into_iter().enumerate() {
        let result = result.unwrap_or_else(|| ProbeResult {
            status: ProviderStatus {
                url: url_base(&urls[idx]).to_string(),
                latency_ms: started.elapsed().as_millis() as u64,
                error: Some("no answer yet when another provider was ready".to_string()),
                ..Default::default()
            },
            via_tor: false,
            clearnet_fallback: false,
        });
        if result.status.healthy {
            outcome.ready = true;
            outcome.tor_ready |= result.via_tor;
        }
        outcome.clearnet_fallback |= result.clearnet_fallback;
        outcome.statuses.push(result.status);
    }
    outcome
}

fn url_base(url: &str) -> &str {
    url.trim_end_matches('/')
}

/// Probe one provider: Tor first (its own circuit), then clearnet when
/// allowed. `.onion` providers are Tor-only.
fn probe_provider(url: &str, plan: &ProbePlan) -> ProbeResult {
    let base = url_base(url);
    let is_onion = base.to_ascii_lowercase().contains(".onion");
    let started = Instant::now();
    let mut status = ProviderStatus {
        url: base.to_string(),
        ..Default::default()
    };
    let mut via_tor = false;
    let mut clearnet_fallback = false;
    let tor = plan
        .tor_socks
        .as_deref()
        .and_then(|socks| el_rawtx::isolated_tor_client(socks, plan.tor_timeout));
    let expected = plan.expected_genesis.as_deref();

    if is_onion {
        match tor.as_ref() {
            Some(client) => match probe_with(client, base, expected) {
                Ok(()) => {
                    status.healthy = true;
                    status.via = "tor".to_string();
                    via_tor = true;
                }
                Err(e) => status.error = Some(e),
            },
            None => status.error = Some("no tor client".to_string()),
        }
    } else {
        let mut tor_error = None;
        if let Some(client) = tor.as_ref() {
            match probe_with(client, base, expected) {
                Ok(()) => {
                    status.healthy = true;
                    status.via = "tor".to_string();
                    via_tor = true;
                }
                Err(e) => tor_error = Some(e),
            }
        }
        if !status.healthy && (plan.fallback || plan.allow_clearnet) {
            match plan.direct.as_ref() {
                Some(client) => match probe_with(client, base, expected) {
                    Ok(()) => {
                        status.healthy = true;
                        status.via = "clearnet".to_string();
                        if (tor_error.is_some() || tor.is_none()) && !plan.allow_clearnet {
                            clearnet_fallback = true;
                        }
                    }
                    Err(e) => {
                        status.error = Some(match tor_error {
                            Some(t) => format!("tor: {t}; clearnet: {e}"),
                            None => e,
                        });
                    }
                },
                None => status.error = Some("no clearnet client".to_string()),
            }
        } else if !status.healthy {
            status.error = Some(match tor_error {
                Some(t) => format!("tor: {t}; clearnet fallback disabled"),
                None => "no Tor and clearnet disabled".to_string(),
            });
        }
    }
    status.latency_ms = started.elapsed().as_millis() as u64;
    ProbeResult {
        status,
        via_tor,
        clearnet_fallback,
    }
}

/// One provider check through one client. With a known genesis this is a
/// single request; a provider of another network does not have that block.
fn probe_with(
    client: &reqwest::blocking::Client,
    base: &str,
    expected_genesis: Option<&str>,
) -> Result<(), String> {
    let hash = match expected_genesis {
        Some(hash) => hash.to_string(),
        None => {
            let response = client
                .get(format!("{base}/block-height/0"))
                .send()
                .map_err(|e| e.without_url().to_string())?;
            let status_code = response.status();
            if !status_code.is_success() {
                return Err(format!("HTTP {status_code}"));
            }
            let body = response.text().map_err(|e| e.to_string())?;
            let hash = body.trim().to_ascii_lowercase();
            if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("unexpected block-height response".to_string());
            }
            hash
        }
    };
    // A provider that cannot serve the genesis block cannot serve old
    // transactions either: this is a real raw-block probe.
    let raw_response = client
        .get(format!("{base}/block/{hash}/raw"))
        .send()
        .map_err(|e| format!("raw block: {}", e.without_url()))?;
    let status_code = raw_response.status();
    if !status_code.is_success() {
        return Err(if expected_genesis.is_some() {
            format!("genesis block not served: HTTP {status_code} (wrong network?)")
        } else {
            format!("raw block: HTTP {status_code}")
        });
    }
    let bytes = raw_response
        .bytes()
        .map_err(|e| format!("raw block: {e}"))?;
    if bytes.len() < 80 {
        return Err("raw block: too short".to_string());
    }
    use bitcoin::hashes::Hash as _;
    let mut computed = bitcoin::hashes::sha256d::Hash::hash(&bytes[..80]).to_byte_array();
    computed.reverse();
    if hex::encode(computed) != hash {
        return Err(if expected_genesis.is_some() {
            "genesis mismatch: the served block is not this network's genesis (wrong network)"
                .to_string()
        } else {
            "raw block: header hash mismatch".to_string()
        });
    }
    Ok(())
}

#[derive(Debug, Clone, Default)]
pub struct Resolved {
    /// Path of the file that was read, even when it did not exist.
    pub path: PathBuf,
    /// `file`, `template:<path>` or `none`.
    pub source: String,
    /// Effective Esplora bases for the active network.
    pub endpoints: Vec<String>,
}

impl Resolved {
    /// True when at least one provider is configured for the active network.
    pub fn configured(&self) -> bool {
        !self.endpoints.is_empty()
    }
}

/// Where `public-servers.toml` lives: next to the active config file, unless
/// `[public_servers] file` overrides it.
pub fn target_path(cfg: &Config, config_path: Option<&Path>) -> PathBuf {
    if let Some(path) = cfg
        .public_servers
        .file
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        return PathBuf::from(path);
    }
    let config = config_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| crate::config::home().join(".config/e-lite/config.toml"));
    config
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("public-servers.toml")
}

fn template_candidates(target: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    // Running from a source checkout or from the packaged tarball.
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("public-servers.toml"));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("public-servers.toml"));
            candidates.extend(checkout_template(dir));
        }
    }
    // System packages.
    candidates.push(PathBuf::from("/usr/share/e-lite/public-servers.toml"));
    candidates.push(PathBuf::from("/usr/local/share/e-lite/public-servers.toml"));
    candidates.retain(|path| path != target);
    candidates
}

/// The template of the source checkout a binary was built in. Cargo decides
/// where binaries live under `target/` and newer versions nest test binaries
/// deeper (`target/debug/build/<pkg>/<hash>/out/`), so a fixed `../../..` is
/// not enough. Walk up, but accept a directory only if it is a checkout: it
/// must hold `Cargo.toml` next to the template. An installed binary (for
/// example in `~/.local/bin`) therefore never picks up a stray
/// `public-servers.toml` from a parent directory.
fn checkout_template(exe_dir: &Path) -> Option<PathBuf> {
    exe_dir
        .ancestors()
        .skip(1)
        .take(8)
        .find(|dir| dir.join("Cargo.toml").is_file() && dir.join("public-servers.toml").is_file())
        .map(|dir| dir.join("public-servers.toml"))
}

fn find_template(target: &Path) -> Option<PathBuf> {
    template_candidates(target)
        .into_iter()
        .find(|path| path.is_file())
}

/// Read (and, on a first run, create) `public-servers.toml` and return the
/// Esplora endpoints for `chain`.
///
/// A missing file is not an error.  It is also never overwritten when it
/// exists, even if it is empty or invalid.
pub fn load(cfg: &Config, chain: Chain, config_path: Option<&Path>) -> Resolved {
    let target = target_path(cfg, config_path);
    let mut source = "none".to_string();
    let mut parse_error = None;

    if !target.is_file() {
        if cfg.public_servers.generate {
            if let Some(template) = find_template(&target) {
                if let Some(parent) = target.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                match std::fs::copy(&template, &target) {
                    Ok(_) => {
                        eprintln!(
                            "[e-lite] public servers: created {} from {}",
                            target.display(),
                            template.display()
                        );
                        source = format!("template:{}", template.display());
                    }
                    Err(e) => {
                        eprintln!(
                            "[e-lite] WARNING: cannot create public servers file {}: {e}",
                            target.display()
                        );
                    }
                }
            } else {
                eprintln!(
                    "[e-lite] WARNING: no public servers file at {} and no shipped template; \
                     using local data only (set [rawtx] esplora_urls or restore the file)",
                    target.display()
                );
            }
        } else {
            eprintln!(
                "[e-lite] public servers: {} is missing and generation is disabled; \
                 using local data only",
                target.display()
            );
        }
    }

    let endpoints = if target.is_file() {
        if source == "none" {
            source = "file".to_string();
        }
        match std::fs::read_to_string(&target) {
            Ok(text) => match toml::from_str::<HashMap<String, NetworkServers>>(&text) {
                Ok(mut networks) => {
                    let key = chain.name();
                    let mut endpoints = networks.remove(key).map(|n| n.esplora).unwrap_or_default();
                    endpoints.retain(|url| match validate(url) {
                        Ok(()) => true,
                        Err(reason) => {
                            eprintln!("[e-lite] WARNING: ignoring public server {url:?}: {reason}");
                            false
                        }
                    });
                    eprintln!(
                        "[e-lite] public servers: {} ({}, {} for {})",
                        target.display(),
                        source,
                        endpoints.len(),
                        key
                    );
                    endpoints
                }
                Err(e) => {
                    parse_error = Some(e.to_string());
                    Vec::new()
                }
            },
            Err(e) => {
                parse_error = Some(e.to_string());
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    if let Some(error) = parse_error {
        eprintln!(
            "[e-lite] WARNING: cannot read public servers file {}: {error}; \
             using local data only",
            target.display()
        );
    }

    Resolved {
        path: target,
        source,
        endpoints,
    }
}

/// Only HTTPS is allowed on the clearnet; plain HTTP is limited to `.onion`
/// hosts and still requires Tor at request time.
fn validate(url: &str) -> Result<(), &'static str> {
    let url = url.trim();
    if url.starts_with("https://") {
        return Ok(());
    }
    if let Some(rest) = url.strip_prefix("http://") {
        let host = rest.split('/').next().unwrap_or("");
        if host.ends_with(".onion") {
            return Ok(());
        }
        return Err("clearnet http:// is not allowed; use https://");
    }
    Err("expected https:// or http://<host>.onion")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "e-lite-public-servers-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn config_for(path: &Path) -> Config {
        let mut cfg = Config::default();
        cfg.public_servers.file = Some(path.display().to_string());
        cfg
    }

    #[test]
    fn probe_reports_a_healthy_provider() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let block = bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Regtest);
        let raw_block = bitcoin::consensus::serialize(&block);
        let hash = block.block_hash().to_string();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            // A known genesis needs one request: the raw genesis block.
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                raw_block.len()
            );
            sock.write_all(response.as_bytes()).unwrap();
            sock.write_all(&raw_block).unwrap();
        });

        let mut cfg = Config::default();
        cfg.rawtx.tor_socks = None;
        cfg.rawtx.allow_clearnet = false;
        cfg.rawtx.tor_fallback_clearnet = true;
        cfg.public_servers.probe_timeout_seconds = 3;
        let outcome = probe(&cfg, &[format!("http://{addr}")], false, Some(&hash));
        assert!(outcome.ready, "{outcome:?}");
        assert!(outcome.statuses[0].healthy);
        assert!(
            outcome.clearnet_fallback,
            "no Tor must surface the fallback"
        );
        handle.join().unwrap();
    }

    #[test]
    fn probe_rejects_a_wrong_network_genesis() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        // The provider answers with a real block that is not the expected
        // genesis (another network).
        let raw_block = bitcoin::consensus::serialize(
            &bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Regtest),
        );
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                raw_block.len()
            );
            sock.write_all(response.as_bytes()).unwrap();
            sock.write_all(&raw_block).unwrap();
        });

        let mut cfg = Config::default();
        cfg.rawtx.tor_socks = None;
        cfg.rawtx.allow_clearnet = true;
        cfg.public_servers.probe_timeout_seconds = 3;
        let expected = "00".repeat(32);
        let outcome = probe(
            &cfg,
            &[format!("http://{addr}")],
            false,
            Some(expected.as_str()),
        );
        assert!(!outcome.ready, "{outcome:?}");
        assert!(outcome.statuses[0]
            .error
            .as_deref()
            .unwrap_or("")
            .contains("genesis mismatch"));
        handle.join().unwrap();
    }

    #[test]
    fn gate_opens_with_the_first_healthy_provider() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let block = bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Regtest);
        let raw_block = bitcoin::consensus::serialize(&block);
        let hash = block.block_hash().to_string();
        let serve = |delay: Duration| {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let body = raw_block.clone();
            std::thread::spawn(move || {
                if let Ok((mut sock, _)) = listener.accept() {
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf);
                    std::thread::sleep(delay);
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(head.as_bytes());
                    let _ = sock.write_all(&body);
                }
            });
            format!("http://{addr}")
        };
        // The slow provider is listed first: a serial probe would wait for it.
        let urls = [serve(Duration::from_secs(8)), serve(Duration::ZERO)];

        let mut cfg = Config::default();
        cfg.rawtx.tor_socks = None;
        cfg.rawtx.allow_clearnet = true;
        cfg.public_servers.probe_timeout_seconds = 10;
        let started = Instant::now();
        let outcome = probe(&cfg, &urls, true, Some(&hash));
        let elapsed = started.elapsed();
        assert!(outcome.ready, "{outcome:?}");
        assert!(
            elapsed < Duration::from_secs(5),
            "gate waited {elapsed:?} for the slow provider"
        );
        assert!(outcome.statuses[1].healthy);
        assert!(!outcome.statuses[0].healthy);
        assert!(outcome.statuses[0]
            .error
            .as_deref()
            .unwrap_or("")
            .contains("no answer yet"));
    }

    #[test]
    fn validates_clearnet_and_onion() {
        assert!(validate("https://mempool.space/api").is_ok());
        assert!(validate(
            "http://mempoolhqx4isw62xs7abwphsq7ldayuidyx2v2oethdhhj6mlo2r6ad.onion/api"
        )
        .is_ok());
        assert!(validate("http://mempool.space/api").is_err());
        assert!(validate("ftp://example.com").is_err());
    }

    #[test]
    fn first_run_copies_the_shipped_template() {
        let dir = tmp_dir("first-run");
        let path = dir.join("public-servers.toml");
        let cfg = config_for(&path);
        let resolved = load(&cfg, Chain::Mainnet, None);
        assert!(path.is_file(), "the template must be copied on first run");
        assert!(!resolved.endpoints.is_empty(), "{resolved:?}");
        assert!(resolved
            .endpoints
            .iter()
            .any(|u| u.contains("blockstream.info")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn checkout_template_follows_any_cargo_layout_but_needs_a_checkout() {
        let root = tmp_dir("checkout");
        std::fs::write(
            root.join("public-servers.toml"),
            "[mainnet]\nesplora = []\n",
        )
        .unwrap();
        // Classic and nested Cargo layouts.
        let classic = root.join("target/debug/deps");
        let nested = root.join("target/debug/build/e-lite/0123abcd/out");
        std::fs::create_dir_all(&classic).unwrap();
        std::fs::create_dir_all(&nested).unwrap();
        // Not a checkout yet: a template in a parent directory is ignored.
        assert_eq!(checkout_template(&classic), None);
        std::fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        let expected = Some(root.join("public-servers.toml"));
        assert_eq!(checkout_template(&classic), expected);
        assert_eq!(checkout_template(&nested), expected);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn existing_file_is_never_overwritten() {
        let dir = tmp_dir("no-overwrite");
        let path = dir.join("public-servers.toml");
        std::fs::write(
            &path,
            "[mainnet]\nesplora = [\"https://custom.example/api\"]\n",
        )
        .unwrap();
        let cfg = config_for(&path);
        let resolved = load(&cfg, Chain::Mainnet, None);
        assert_eq!(resolved.endpoints, vec!["https://custom.example/api"]);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("custom.example"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_file_falls_back_to_no_providers_without_rewriting() {
        let dir = tmp_dir("invalid");
        let path = dir.join("public-servers.toml");
        std::fs::write(&path, "this is not toml").unwrap();
        let cfg = config_for(&path);
        let resolved = load(&cfg, Chain::Mainnet, None);
        assert!(resolved.endpoints.is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "this is not toml");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn generation_can_be_disabled() {
        let dir = tmp_dir("no-generate");
        let path = dir.join("public-servers.toml");
        let mut cfg = config_for(&path);
        cfg.public_servers.generate = false;
        let resolved = load(&cfg, Chain::Mainnet, None);
        assert!(!path.exists());
        assert!(resolved.endpoints.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
