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

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct NetworkServers {
    #[serde(default)]
    esplora: Vec<String>,
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
