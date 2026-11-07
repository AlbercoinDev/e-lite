//! TLS certificate preparation.
//!
//! TLS is independent of Tor. When no operator certificate is configured,
//! e-lite generates a per-instance **local CA** and a server certificate signed
//! by it. Serving a leaf that is not itself a CA and asking the wallet to trust
//! the CA is what Java-based wallets (Sparrow) require: Java rejects a
//! non-CA certificate used as a `TrustAnchor`.
//!
//! Files under `<data-dir>/tls/`:
//! - `ca.crt` / `ca.key`: the local CA (import `ca.crt` into the wallet trust
//!   store; the key stays private, mode 0600);
//! - `self-signed.crt`: the server leaf followed by the CA (TLS chain);
//! - `self-signed.key`: the leaf private key;
//! - `self-signed.san` and `cert-model`: regeneration markers.
//!
//! The leaf is re-signed with the same CA when the SAN set changes (for
//! example when the onion address appears), so wallets keep trusting the CA.
//! Operator PEMs are never overwritten.

use crate::config::Config;
use el_model::Chain;
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::CertificateDer;
use std::fs;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const CERT_MODEL: &str = "ca-v1";

pub fn auto_cert_paths(cfg: &Config, chain: Chain) -> (PathBuf, PathBuf) {
    let dir = cfg.data_dir(chain).join("tls");
    (dir.join("self-signed.crt"), dir.join("self-signed.key"))
}

pub fn auto_ca_paths(cfg: &Config, chain: Chain) -> (PathBuf, PathBuf) {
    let dir = cfg.data_dir(chain).join("tls");
    (dir.join("ca.crt"), dir.join("ca.key"))
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

/// Names covered by the generated server certificate, in a stable order so the
/// leaf only changes when the identity actually changes.
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

/// Parameters of the e-lite-managed local CA. They are deterministic so an
/// existing `ca.crt` can be used as the issuer without parsing ASN.1: the
/// subject and key identifier are derived from this DN and the CA key.
fn ca_params(chain: Chain) -> CertificateParams {
    let mut params = CertificateParams::default();
    params.distinguished_name = DistinguishedName::new();
    params.distinguished_name.push(
        DnType::CommonName,
        format!("e-lite {} local CA", chain.name()),
    );
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params
}

/// Load the local CA or create a new one. Returns the CA certificate PEM and
/// its private key PEM.
fn ensure_ca(chain: Chain, cert_path: &Path, key_path: &Path) -> Result<(String, String), String> {
    if pair_is_valid(cert_path, key_path) {
        let cert =
            fs::read_to_string(cert_path).map_err(|e| format!("{}: {e}", cert_path.display()))?;
        let key =
            fs::read_to_string(key_path).map_err(|e| format!("{}: {e}", key_path.display()))?;
        return Ok((cert, key));
    }
    let key = KeyPair::generate().map_err(|e| format!("cannot generate the CA key: {e}"))?;
    let cert = ca_params(chain)
        .self_signed(&key)
        .map_err(|e| format!("cannot generate the local CA: {e}"))?;
    let key_pem = key.serialize_pem();
    // Store the key first: a crash leaves no orphan CA cert without its key.
    write_private(key_path, key_pem.as_bytes())?;
    write_private(cert_path, cert.pem().as_bytes())?;
    Ok((cert.pem(), key_pem))
}

/// Create or refresh the server certificate. Reuses the CA and the leaf key
/// when possible; a corrupt pair or an old single-cert model is replaced.
pub fn ensure_self_signed(
    cfg: &Config,
    chain: Chain,
    onion: Option<&str>,
) -> Result<(String, String), String> {
    let (cert_path, key_path) = auto_cert_paths(cfg, chain);
    let (ca_cert_path, ca_key_path) = auto_ca_paths(cfg, chain);
    let dir = cert_path
        .parent()
        .ok_or_else(|| "invalid TLS certificate path".to_string())?;
    fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut perms = fs::metadata(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .permissions();
    perms.set_mode(0o700);
    let _ = fs::set_permissions(dir, perms);

    let (ca_pem, ca_key_pem) = ensure_ca(chain, &ca_cert_path, &ca_key_path)?;
    let ca_key =
        KeyPair::from_pem(&ca_key_pem).map_err(|e| format!("cannot read the local CA key: {e}"))?;
    let ca_cert = ca_params(chain)
        .self_signed(&ca_key)
        .map_err(|e| format!("cannot use the local CA: {e}"))?;
    let ca_fp = cert_fingerprint(&ca_cert_path)?;
    let model = format!("{CERT_MODEL} {ca_fp}\n");
    let model_path = dir.join("cert-model");

    let names = san_names(onion);
    let san = format!("{}\n", names.join("\n"));
    let san_path = dir.join("self-signed.san");
    let pair_valid = pair_is_valid(&cert_path, &key_path);
    let model_ok = fs::read_to_string(&model_path).ok().as_deref() == Some(model.as_str());
    if pair_valid && model_ok && fs::read_to_string(&san_path).ok().as_deref() == Some(san.as_str())
    {
        return Ok((
            cert_path.display().to_string(),
            key_path.display().to_string(),
        ));
    }

    // Keep the key when the current leaf is valid and only the SAN set or the
    // CA changed; never derive a new cert from an unreadable key.
    let reused_key = if pair_valid {
        fs::read_to_string(&key_path)
            .ok()
            .and_then(|pem| KeyPair::from_pem(&pem).ok())
    } else {
        None
    };
    let mut params = CertificateParams::new(names.clone())
        .map_err(|e| format!("cannot prepare the TLS certificate: {e}"))?;
    params.distinguished_name = DistinguishedName::new();
    params.distinguished_name.push(
        DnType::CommonName,
        format!("e-lite {} server", chain.name()),
    );
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.use_authority_key_identifier_extension = true;
    let key = match reused_key {
        Some(k) => k,
        None => KeyPair::generate().map_err(|e| format!("cannot generate the TLS key: {e}"))?,
    };
    let leaf = params
        .signed_by(&key, &ca_cert, &ca_key)
        .map_err(|e| format!("cannot sign the TLS certificate: {e}"))?;
    // Chain = leaf + CA. rustls sends the intermediate so the wallet can build
    // the path to the trusted local CA.
    let chain = format!("{}{}", leaf.pem(), ca_pem);
    write_private(&key_path, key.serialize_pem().as_bytes())?;
    write_private(&cert_path, chain.as_bytes())?;
    write_private(&san_path, san.as_bytes())?;
    write_private(&model_path, model.as_bytes())?;
    if !pair_is_valid(&cert_path, &key_path) {
        return Err("the generated TLS certificate/key pair is not usable".to_string());
    }
    Ok((
        cert_path.display().to_string(),
        key_path.display().to_string(),
    ))
}

/// SHA-256 of the first DER certificate in a PEM file, the value `openssl x509
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
            let (ca_cert, _) = auto_ca_paths(cfg, chain);
            if !ca_cert.is_file() {
                eprintln!(
                    "[e-lite] WARNING: tls_auto_cert is off and {} is missing; wallets \
                     cannot validate the server certificate",
                    ca_cert.display()
                );
            }
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
    let (ca_cert, _) = auto_ca_paths(cfg, chain);
    let leaf_fp = cert_fingerprint(Path::new(&cert))?;
    let ca_fp = cert_fingerprint(&ca_cert)?;
    eprintln!(
        "[e-lite] TLS certificate: {cert} (leaf sha256 {leaf_fp}). \
         Local CA: {} (sha256 {ca_fp}); trust the CA certificate in the wallet, \
         or use a public CA certificate for a real domain.",
        ca_cert.display(),
        ca_fp = ca_fp
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

    fn chain_len(path: &Path) -> usize {
        CertificateDer::pem_file_iter(path).unwrap().count()
    }

    #[test]
    fn generated_chain_has_a_ca_and_a_server_leaf() {
        let (mut cfg, dir) = temp_cfg("chain");
        cfg.server.tls_cert.clear();
        cfg.server.tls_key.clear();
        let onion = format!("{}.onion", "b".repeat(56));
        let (cert, key) = ensure_self_signed(&cfg, Chain::Regtest, Some(&onion)).unwrap();
        let (ca_cert, ca_key) = auto_ca_paths(&cfg, Chain::Regtest);
        assert_eq!(chain_len(Path::new(&cert)), 2, "leaf + CA");
        assert_eq!(chain_len(&ca_cert), 1);
        assert!(ca_cert.is_file() && ca_key.is_file());
        let ca_mode = fs::metadata(&ca_key).unwrap().permissions().mode() & 0o777;
        assert_eq!(ca_mode, 0o600);
        let key_mode = fs::metadata(&key).unwrap().permissions().mode() & 0o777;
        assert_eq!(key_mode, 0o600);
        let san = fs::read_to_string(dir.join("tls/self-signed.san")).unwrap();
        assert!(san.contains(&onion));
        assert!(san.contains("localhost"));
        // The same SAN reuses the files (including the CA) instead of
        // regenerating them.
        let ca_before = fs::read(&ca_cert).unwrap();
        let before = fs::metadata(&cert).unwrap().modified().unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let _ = ensure_self_signed(&cfg, Chain::Regtest, Some(&onion)).unwrap();
        assert_eq!(before, fs::metadata(&cert).unwrap().modified().unwrap());
        assert_eq!(ca_before, fs::read(&ca_cert).unwrap());
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
        assert_eq!(chain_len(&cert), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_single_cert_model_is_migrated_to_ca_plus_leaf() {
        let (mut cfg, dir) = temp_cfg("migrate");
        cfg.server.tls_cert.clear();
        cfg.server.tls_key.clear();
        // Simulate a 29.4.x install: a single self-signed leaf and no CA.
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, "e-lite self-signed");
        let old = params.self_signed(&key).unwrap();
        let (cert, key_path) = auto_cert_paths(&cfg, Chain::Regtest);
        fs::create_dir_all(cert.parent().unwrap()).unwrap();
        fs::write(&cert, old.pem()).unwrap();
        fs::write(&key_path, key.serialize_pem()).unwrap();
        ensure_self_signed(&cfg, Chain::Regtest, None).unwrap();
        assert_eq!(chain_len(&cert), 2);
        let (ca_cert, _) = auto_ca_paths(&cfg, Chain::Regtest);
        assert!(ca_cert.is_file());
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
        let (ca_cert, _) = auto_ca_paths(&cfg, Chain::Regtest);
        assert_ne!(fp, cert_fingerprint(&ca_cert).unwrap());
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
