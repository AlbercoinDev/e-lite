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

/// Probe the providers and wait (up to `cfg.public_servers.ready_timeout_seconds`)
/// until at least one answers. Uses `/block-height/0`: a constant, public query
/// that never contains wallet data.
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
    let tor = cfg
        .rawtx
        .tor_socks
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|proxy| {
            reqwest::blocking::Client::builder()
                .timeout(timeout)
                .user_agent(concat!("e-lite/", env!("CARGO_PKG_VERSION")))
                .proxy(reqwest::Proxy::all(format!("socks5h://{proxy}")).ok()?)
                .build()
                .ok()
        });
    let direct = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .user_agent(concat!("e-lite/", env!("CARGO_PKG_VERSION")))
        .build()
        .ok();
    let fallback = cfg.rawtx.tor_fallback_clearnet;
    let allow_clearnet = cfg.rawtx.allow_clearnet;

    let deadline =
        Instant::now() + Duration::from_secs(cfg.public_servers.ready_timeout_seconds.max(1));
    loop {
        let outcome = probe_once(
            urls,
            tor.as_ref(),
            direct.as_ref(),
            allow_clearnet,
            fallback,
            expected_genesis,
        );
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

fn probe_once(
    urls: &[String],
    tor: Option<&reqwest::blocking::Client>,
    direct: Option<&reqwest::blocking::Client>,
    allow_clearnet: bool,
    fallback: bool,
    expected_genesis: Option<&str>,
) -> ProbeOutcome {
    let mut outcome = ProbeOutcome::default();
    for url in urls {
        let base = url.trim_end_matches('/');
        let is_onion = base.to_ascii_lowercase().contains(".onion");
        let target = format!("{base}/block-height/0");
        let started = Instant::now();
        let mut status = ProviderStatus {
            url: base.to_string(),
            ..Default::default()
        };

        let attempt = |client: Option<&reqwest::blocking::Client>,
                       via: &str|
         -> Result<(), String> {
            let Some(client) = client else {
                return Err(format!("no {via} client"));
            };
            let response = client.get(&target).send().map_err(|e| e.to_string())?;
            let status_code = response.status();
            if !status_code.is_success() {
                return Err(format!("HTTP {status_code}"));
            }
            let body = response.text().map_err(|e| e.to_string())?;
            let hash = body.trim();
            if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("unexpected block-height response".to_string());
            }
            if let Some(expected) = expected_genesis {
                if !hash.eq_ignore_ascii_case(expected) {
                    return Err(format!(
                        "genesis mismatch: provider serves {hash}, expected {expected} \
                             (wrong network)"
                    ));
                }
            }
            // A provider that cannot serve the genesis block cannot serve
            // old transactions either. This is a real rawtx probe, not just
            // an index lookup.
            let raw_response = client
                .get(format!("{base}/block/{hash}/raw"))
                .send()
                .map_err(|e| format!("raw block: {e}"))?;
            let status_code = raw_response.status();
            if !status_code.is_success() {
                return Err(format!("raw block: HTTP {status_code}"));
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
            if hex::encode(computed) != hash.to_ascii_lowercase() {
                return Err("raw block: header hash mismatch".to_string());
            }
            Ok(())
        };

        if is_onion {
            match attempt(tor, "tor") {
                Ok(()) => {
                    status.healthy = true;
                    status.via = "tor".to_string();
                    outcome.tor_ready = true;
                }
                Err(e) => status.error = Some(e),
            }
        } else {
            let mut tor_error = None;
            if tor.is_some() {
                match attempt(tor, "tor") {
                    Ok(()) => {
                        status.healthy = true;
                        status.via = "tor".to_string();
                        outcome.tor_ready = true;
                    }
                    Err(e) => tor_error = Some(e),
                }
            }
            if !status.healthy && (fallback || allow_clearnet) {
                match attempt(direct, "clearnet") {
                    Ok(()) => {
                        status.healthy = true;
                        status.via = "clearnet".to_string();
                        if (tor_error.is_some() || tor.is_none()) && !allow_clearnet {
                            outcome.clearnet_fallback = true;
                        }
                    }
                    Err(e) => {
                        status.error = Some(match tor_error {
                            Some(t) => format!("tor: {t}; clearnet: {e}"),
                            None => e,
                        });
                    }
                }
            } else if !status.healthy {
                status.error = Some(match tor_error {
                    Some(t) => format!("tor: {t}; clearnet fallback disabled"),
                    None => "no Tor and clearnet disabled".to_string(),
                });
            }
        }

        status.latency_ms = started.elapsed().as_millis() as u64;
        if status.healthy {
            outcome.ready = true;
        }
        outcome.statuses.push(status);
    }
    outcome
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
            candidates.push(dir.join("../../public-servers.toml"));
            candidates.push(dir.join("../../../public-servers.toml"));
        }
    }
    // System packages.
    candidates.push(PathBuf::from("/usr/share/e-lite/public-servers.toml"));
    candidates.push(PathBuf::from("/usr/local/share/e-lite/public-servers.toml"));
    candidates.retain(|path| path != target);
    candidates
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
        let served_hash = hash.clone();
        let handle = std::thread::spawn(move || {
            for i in 0..2 {
                let (mut sock, _) = listener.accept().unwrap();
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf);
                let body = if i == 0 {
                    served_hash.as_bytes().to_vec()
                } else {
                    raw_block.clone()
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                sock.write_all(response.as_bytes()).unwrap();
                sock.write_all(&body).unwrap();
            }
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

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf);
            let body = "11".repeat(32);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(response.as_bytes()).unwrap();
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
