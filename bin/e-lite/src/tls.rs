//! TLS certificate preparation.
//!
//! TLS is independent of Tor: e-lite generates and reuses a self-signed
//! certificate on first start when the operator did not configure one. The
//! certificate is regenerated when the SAN set changes (for example, when the
//! onion address becomes known) and never when the operator supplied their own
//! PEM files.

use crate::config::Config;
use el_model::Chain;
use rcgen::{CertificateParams, DnType, KeyPair, SanType};
use std::fs;
use std::io::Write;
use std::net::IpAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub fn auto_cert_paths(cfg: &Config, chain: Chain) -> (PathBuf, PathBuf) {
    let dir = cfg.data_dir(chain).join("tls");
    (dir.join("self-signed.crt"), dir.join("self-signed.key"))
}

fn write_private(path: &Path, data: &[u8]) -> Result<(), String> {
    let mut f = fs::OpenOptions::new()
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

fn hostname() -> Option<String> {
    if let Ok(v) = std::env::var("HOSTNAME") {
        let v = v.trim().to_string();
        if !v.is_empty() && v != "localhost" {
            return Some(v);
        }
    }
    let v = fs::read_to_string("/etc/hostname").ok()?;
    let v = v.trim().to_string();
    (!v.is_empty()).then_some(v)
}

/// Names covered by the generated certificate, in a stable order so the
/// fingerprint only changes when the identity actually changes.
fn san_names(onion: Option<&str>) -> Vec<String> {
    let mut names = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ];
    if let Some(host) = hostname() {
        if !names.iter().any(|n| n.eq_ignore_ascii_case(&host)) {
            names.push(host);
        }
    }
    if let Some(addr) = onion {
        let addr = addr.trim().to_ascii_lowercase();
        if crate::tor::valid_onion(&addr) && !names.contains(&addr) {
            names.push(addr);
        }
    }
    names
}

/// Write a self-signed certificate covering the active identities. Reuses the
/// existing files when the SAN set did not change.
pub fn ensure_self_signed(
    cfg: &Config,
    chain: Chain,
    onion: Option<&str>,
) -> Result<(String, String), String> {
    let (cert_path, key_path) = auto_cert_paths(cfg, chain);
    let dir = cert_path
        .parent()
        .ok_or_else(|| "invalid TLS certificate path".to_string())?;
    fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut perms = fs::metadata(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .permissions();
    perms.set_mode(0o700);
    let _ = fs::set_permissions(dir, perms);

    let names = san_names(onion);
    let san = format!("{}\n", names.join("\n"));
    let san_path = dir.join("self-signed.san");
    if cert_path.is_file()
        && key_path.is_file()
        && fs::read_to_string(&san_path).ok().as_deref() == Some(san.as_str())
    {
        return Ok((
            cert_path.display().to_string(),
            key_path.display().to_string(),
        ));
    }

    let mut params = CertificateParams::new(Vec::<String>::new())
        .map_err(|e| format!("cannot prepare the TLS certificate: {e}"))?;
    params
        .distinguished_name
        .push(DnType::CommonName, "e-lite self-signed");
    for name in &names {
        match name.parse::<IpAddr>() {
            Ok(ip) => params.subject_alt_names.push(SanType::IpAddress(ip)),
            Err(_) => {
                let dns = name
                    .clone()
                    .try_into()
                    .map_err(|e| format!("invalid certificate name {name:?}: {e}"))?;
                params.subject_alt_names.push(SanType::DnsName(dns));
            }
        }
    }
    let key = KeyPair::generate().map_err(|e| format!("cannot generate the TLS key: {e}"))?;
    let cert = params
        .self_signed(&key)
        .map_err(|e| format!("cannot generate the TLS certificate: {e}"))?;
    write_private(&key_path, key.serialize_pem().as_bytes())?;
    write_private(&cert_path, cert.pem().as_bytes())?;
    write_private(&san_path, san.as_bytes())?;
    Ok((
        cert_path.display().to_string(),
        key_path.display().to_string(),
    ))
}

/// Resolve TLS before transports start. Returns true when the certificate is
/// managed by e-lite (so `tor::spawn` may regenerate it with the onion SAN).
pub fn prepare(cfg: &mut Config, chain: Chain) -> Result<bool, String> {
    if !cfg.server.tls {
        return Ok(false);
    }
    let (auto_cert, auto_key) = auto_cert_paths(cfg, chain);
    let using_auto = cfg.server.tls_cert.trim() == auto_cert.display().to_string()
        && cfg.server.tls_key.trim() == auto_key.display().to_string();
    if !using_auto {
        for path in [&cfg.server.tls_cert, &cfg.server.tls_key] {
            let path = Path::new(path.trim());
            if !path.is_file() {
                return Err(format!(
                    "TLS certificate/key {} does not exist",
                    path.display()
                ));
            }
        }
        return Ok(false);
    }
    if !cfg.server.tls_auto_cert || !cfg.tor.tls_auto_cert {
        // An explicit opt-out must not silently serve an invalid/missing cert.
        return Err(
            "[server] tls_auto_cert = false but no TLS certificate exists; configure \
             tls_cert/tls_key or enable tls_auto_cert"
                .to_string(),
        );
    }
    let onion = crate::tor::read_hostname(&cfg.tor_dir(chain));
    let (cert, key) = ensure_self_signed(cfg, chain, onion.as_deref())?;
    cfg.server.tls_cert = cert.clone();
    cfg.server.tls_key = key;
    let bytes = fs::read(&cert).map_err(|e| format!("{cert}: {e}"))?;
    use bitcoin::hashes::Hash as _;
    let digest = bitcoin::hashes::sha256::Hash::hash(&bytes);
    eprintln!(
        "[e-lite] TLS certificate: {cert} (self-signed; sha256 {digest}). \
         Add it to the wallet trust store or use a public CA certificate for a real domain."
    );
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn temp_cfg(name: &str) -> (Config, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "e-lite-tls-{name}-{}-{}",
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
        cfg.server.listen = "127.0.0.1:55311".to_string();
        cfg.server.tls_listen = "127.0.0.1:55322".to_string();
        (cfg, dir)
    }

    #[test]
    fn self_signed_certificate_is_written_with_onion_san() {
        let (mut cfg, dir) = temp_cfg("cert");
        cfg.server.tls_cert.clear();
        cfg.server.tls_key.clear();
        let onion = format!("{}.onion", "b".repeat(56));
        let (cert, key) = ensure_self_signed(&cfg, Chain::Regtest, Some(&onion)).unwrap();
        assert!(Path::new(&cert).is_file());
        assert!(Path::new(&key).is_file());
        let mode = fs::metadata(&key).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let san = fs::read_to_string(dir.join("tls/self-signed.san")).unwrap();
        assert!(san.contains(&onion));
        // The same SAN reuses the files instead of regenerating them.
        let before = fs::metadata(&cert).unwrap().modified().unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let _ = ensure_self_signed(&cfg, Chain::Regtest, Some(&onion)).unwrap();
        assert_eq!(before, fs::metadata(&cert).unwrap().modified().unwrap());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prepare_generates_a_certificate_without_tor() {
        let (mut cfg, dir) = temp_cfg("prepare");
        cfg.server.tls_cert.clear();
        cfg.server.tls_key.clear();
        cfg.apply_network_defaults(Chain::Regtest);
        assert!(cfg.server.tls_enabled());
        let managed = prepare(&mut cfg, Chain::Regtest).unwrap();
        assert!(managed, "the auto certificate is managed by e-lite");
        assert!(Path::new(&cfg.server.tls_cert).is_file());
        assert!(Path::new(&cfg.server.tls_key).is_file());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn operator_certificate_is_never_overwritten() {
        let (mut cfg, dir) = temp_cfg("operator");
        let cert = dir.join("operator.crt");
        let key = dir.join("operator.key");
        fs::write(&cert, "operator cert").unwrap();
        fs::write(&key, "operator key").unwrap();
        cfg.server.tls_cert = cert.display().to_string();
        cfg.server.tls_key = key.display().to_string();
        assert!(!prepare(&mut cfg, Chain::Regtest).unwrap());
        assert_eq!(fs::read_to_string(&cert).unwrap(), "operator cert");
        let _ = fs::remove_dir_all(&dir);
    }
}
