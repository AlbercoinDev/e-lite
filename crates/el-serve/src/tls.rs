//! tls module (split from lib.rs in alpha.29.17.0).

use super::*;

/// Build a rustls server configuration from PEM files. Any certificate chain
/// works, including a public CA certificate reused from a web server.
pub fn build_tls_config(cert: &Path, key: &Path) -> Result<ServerConfig, String> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .map_err(|e| format!("cannot read TLS certificate {}: {e}", cert.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("invalid TLS certificate {}: {e}", cert.display()))?;
    if certs.is_empty() {
        return Err(format!(
            "TLS certificate {} contains no certificates",
            cert.display()
        ));
    }
    let key_der = PrivateKeyDer::from_pem_file(key)
        .map_err(|e| format!("cannot read TLS private key {}: {e}", key.display()))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS configuration error: {e}"))?
        .with_no_client_auth()
        .with_single_cert(certs, key_der)
        .map_err(|e| format!("invalid TLS certificate/key: {e}"))
}

/// Reloads the TLS certificate when the PEM files change on disk. This lets a
/// Let's Encrypt renewal take effect without restarting the indexer.
pub struct CertWatcher {
    cert: PathBuf,
    key: PathBuf,
    interval: Duration,
    state: Mutex<CertState>,
}

pub(crate) struct CertState {
    config: Arc<ServerConfig>,
    cert_mtime: Option<SystemTime>,
    key_mtime: Option<SystemTime>,
    checked: Instant,
    error_logged: bool,
}

impl CertWatcher {
    pub fn load(cert: PathBuf, key: PathBuf, interval: Duration) -> Result<Self, String> {
        let config = build_tls_config(&cert, &key)?;
        let cert_mtime = file_mtime(&cert);
        let key_mtime = file_mtime(&key);
        Ok(Self {
            cert,
            key,
            interval,
            state: Mutex::new(CertState {
                config: Arc::new(config),
                cert_mtime,
                key_mtime,
                checked: Instant::now(),
                error_logged: false,
            }),
        })
    }

    /// Current server configuration, reloading it when the files changed.
    /// A failed reload keeps the previous certificate in use.
    pub fn config(&self) -> Arc<ServerConfig> {
        let mut state = match self.state.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        if state.checked.elapsed() >= self.interval {
            state.checked = Instant::now();
            let cert_mtime = file_mtime(&self.cert);
            let key_mtime = file_mtime(&self.key);
            if cert_mtime != state.cert_mtime || key_mtime != state.key_mtime {
                match build_tls_config(&self.cert, &self.key) {
                    Ok(config) => {
                        state.config = Arc::new(config);
                        state.cert_mtime = cert_mtime;
                        state.key_mtime = key_mtime;
                        state.error_logged = false;
                        eprintln!(
                            "[e-lite] TLS certificate reloaded from {}",
                            self.cert.display()
                        );
                    }
                    Err(e) => {
                        if !state.error_logged {
                            eprintln!(
                                "[e-lite] WARNING: TLS certificate reload failed ({e}); \
                                 keeping the previous certificate"
                            );
                            state.error_logged = true;
                        }
                    }
                }
            }
        }
        Arc::clone(&state.config)
    }
}

pub(crate) fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}
