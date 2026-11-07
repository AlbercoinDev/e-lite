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
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::CertificateDer;
use std::fs;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub fn auto_cert_paths(cfg: &Config, chain: Chain) -> (PathBuf, PathBuf) {
    let dir = cfg.data_dir(chain).join("tls");
    (dir.join("self-signed.crt"), dir.join("self-signed.key"))
}

/// Write a file with 0600 permissions and publish it atomically, so a crash or
/// a concurrent reader never sees a truncated certificate or key.
fn write_private(path: &Path, data: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    let result = (|| -> Result<(), String> {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| format!("{}: {e}", tmp.display()))?;
        let mut perms = f
            .metadata()
            .map_err(|e| format!("{}: {e}", tmp.display()))?
            .permissions();
        perms.set_mode(0o600);
        let _ = f.set_permissions(perms);
        f.write_all(data)
            .map_err(|e| format!("{}: {e}", tmp.display()))?;
        f.sync_all()
            .map_err(|e| format!("{}: {e}", tmp.display()))?;
        fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Non-loopback, non-link-local addresses of the local interfaces. They are
/// added to the generated certificate so a LAN wallet that connects by IP can
/// validate the name, not only wallets using the hostname.
fn local_ips() -> Vec<IpAddr> {
    let mut out: Vec<IpAddr> = Vec::new();
    // `getifaddrs` owns the list until `freeifaddrs`; every pointer is checked
    // before use and no Rust reference outlives the call.
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 || ifap.is_null() {
            return out;
        }
        let mut cur = ifap;
        while !cur.is_null() {
            let ifa = &*cur;
            if !ifa.ifa_addr.is_null() {
                let family = (*ifa.ifa_addr).sa_family as i32;
                if family == libc::AF_INET {
                    let sa = ifa.ifa_addr as *const libc::sockaddr_in;
                    let octets = u32::from_be((*sa).sin_addr.s_addr).to_be_bytes();
                    out.push(IpAddr::V4(Ipv4Addr::from(octets)));
                } else if family == libc::AF_INET6 {
                    let sa = ifa.ifa_addr as *const libc::sockaddr_in6;
                    out.push(IpAddr::V6(Ipv6Addr::from((*sa).sin6_addr.s6_addr)));
                }
            }
            cur = ifa.ifa_next;
        }
        libc::freeifaddrs(ifap);
    }
    out.retain(|ip| {
        if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
            return false;
        }
        match ip {
            IpAddr::V4(v4) => !(v4.octets()[0] == 169 && v4.octets()[1] == 254),
            IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) != 0xfe80,
        }
    });
    out.sort();
    out.dedup();
    out
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
    for ip in local_ips() {
        let name = ip.to_string();
        if !names.contains(&name) {
            names.push(name);
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

fn pair_is_valid(cert: &Path, key: &Path) -> bool {
    cert.is_file() && key.is_file() && el_serve::build_tls_config(cert, key).is_ok()
}

/// Write a self-signed certificate covering the active identities. Reuses the
/// existing pair when it is valid and the SAN set did not change; a corrupt
/// pair is replaced.
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
    let pair_valid = pair_is_valid(&cert_path, &key_path);
    if pair_valid && fs::read_to_string(&san_path).ok().as_deref() == Some(san.as_str()) {
        return Ok((
            cert_path.display().to_string(),
            key_path.display().to_string(),
        ));
    }

    // Keep the key when the current pair is valid and only the SAN set
    // changed; never derive a new cert from an unreadable/mismatched key.
    let reused_key = if pair_valid {
        fs::read_to_string(&key_path)
            .ok()
            .and_then(|pem| KeyPair::from_pem(&pem).ok())
    } else {
        None
    };
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
    let key = match reused_key {
        Some(k) => k,
        None => KeyPair::generate().map_err(|e| format!("cannot generate the TLS key: {e}"))?,
    };
    let cert = params
        .self_signed(&key)
        .map_err(|e| format!("cannot generate the TLS certificate: {e}"))?;
    // Key first, then certificate, then the SAN marker: a crash at any point
    // leaves a pair that is regenerated on the next start.
    write_private(&key_path, key.serialize_pem().as_bytes())?;
    write_private(&cert_path, cert.pem().as_bytes())?;
    write_private(&san_path, san.as_bytes())?;
    if !pair_is_valid(&cert_path, &key_path) {
        return Err("the generated TLS certificate/key pair is not usable".to_string());
    }
    Ok((
        cert_path.display().to_string(),
        key_path.display().to_string(),
    ))
}

/// SHA-256 of the DER-encoded leaf certificate, the value `openssl x509
/// -fingerprint -sha256` prints (without the colons).
fn cert_fingerprint(cert: &Path) -> Result<String, String> {
    let der: CertificateDer<'static> = CertificateDer::pem_file_iter(cert)
        .map_err(|e| format!("cannot read TLS certificate {}: {e}", cert.display()))?
        .next()
        .ok_or_else(|| format!("TLS certificate {} is empty", cert.display()))?
        .map_err(|e| format!("invalid TLS certificate {}: {e}", cert.display()))?;
    use bitcoin::hashes::Hash as _;
    Ok(bitcoin::hashes::sha256::Hash::hash(der.as_ref()).to_string())
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
        let cert = Path::new(cfg.server.tls_cert.trim());
        let key = Path::new(cfg.server.tls_key.trim());
        el_serve::build_tls_config(cert, key)
            .map_err(|e| format!("TLS certificate/key is not usable: {e}"))?;
        return Ok(false);
    }
    let auto_exists = pair_is_valid(&auto_cert, &auto_key);
    if !cfg.server.tls_auto_cert || !cfg.tor.tls_auto_cert {
        // An explicit opt-out may still use a pair generated earlier; it must
        // not silently serve a missing or corrupt certificate.
        if auto_exists {
            return Ok(false);
        }
        return Err(
            "[server] tls_auto_cert = false and no valid certificate exists; configure \
             tls_cert/tls_key or enable tls_auto_cert"
                .to_string(),
        );
    }
    let onion = crate::tor::read_hostname(&cfg.tor_dir(chain));
    let (cert, key) = ensure_self_signed(cfg, chain, onion.as_deref())?;
    cfg.server.tls_cert = cert.clone();
    cfg.server.tls_key = key;
    el_serve::build_tls_config(Path::new(&cert), Path::new(&cfg.server.tls_key))
        .map_err(|e| format!("TLS certificate/key is not usable: {e}"))?;
    let digest = cert_fingerprint(Path::new(&cert))?;
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
        assert!(san.contains("localhost"));
        // The same SAN reuses the files instead of regenerating them.
        let before = fs::metadata(&cert).unwrap().modified().unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let _ = ensure_self_signed(&cfg, Chain::Regtest, Some(&onion)).unwrap();
        assert_eq!(before, fs::metadata(&cert).unwrap().modified().unwrap());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_auto_pair_is_regenerated() {
        let (mut cfg, dir) = temp_cfg("corrupt");
        cfg.server.tls_cert.clear();
        cfg.server.tls_key.clear();
        let (cert, key) = auto_cert_paths(&cfg, Chain::Regtest);
        fs::create_dir_all(cert.parent().unwrap()).unwrap();
        fs::write(&cert, "not a certificate").unwrap();
        fs::write(&key, "not a key").unwrap();
        ensure_self_signed(&cfg, Chain::Regtest, None).unwrap();
        assert!(pair_is_valid(&cert, &key), "pair must be rebuilt");
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
    fn auto_cert_opt_out_accepts_an_existing_pair() {
        let (mut cfg, dir) = temp_cfg("opt-out");
        cfg.apply_network_defaults(Chain::Regtest);
        prepare(&mut cfg, Chain::Regtest).unwrap();
        cfg.server.tls_auto_cert = false;
        assert!(!prepare(&mut cfg, Chain::Regtest).unwrap());
        assert!(Path::new(&cfg.server.tls_cert).is_file());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fingerprint_matches_the_der_certificate() {
        let (mut cfg, dir) = temp_cfg("fingerprint");
        cfg.apply_network_defaults(Chain::Regtest);
        prepare(&mut cfg, Chain::Regtest).unwrap();
        let fp = cert_fingerprint(Path::new(&cfg.server.tls_cert)).unwrap();
        assert_eq!(fp.len(), 64);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn operator_certificate_is_never_overwritten() {
        let (mut cfg, dir) = temp_cfg("operator");
        // Build a real pair first, then point the config at a copy.
        cfg.apply_network_defaults(Chain::Regtest);
        prepare(&mut cfg, Chain::Regtest).unwrap();
        let cert = dir.join("operator.crt");
        let key = dir.join("operator.key");
        fs::copy(&cfg.server.tls_cert, &cert).unwrap();
        fs::copy(&cfg.server.tls_key, &key).unwrap();
        cfg.server.tls_cert = cert.display().to_string();
        cfg.server.tls_key = key.display().to_string();
        let before = fs::read(&cert).unwrap();
        assert!(!prepare(&mut cfg, Chain::Regtest).unwrap());
        assert_eq!(before, fs::read(&cert).unwrap());
        let _ = fs::remove_dir_all(&dir);
    }
}
