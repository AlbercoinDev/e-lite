//! Esplora-compatible source: `GET /block/:hash/raw` (whole block only).
//!
//! Requests never mention a TXID. With Tor configured, the client uses a
//! SOCKS5h proxy and `.onion` endpoints are allowed; without Tor, external
//! clearnet is refused unless `allow_clearnet` is explicitly enabled.

use crate::{BlockSource, RawtxError, Result};
use el_model::Hash32;
use rand::seq::SliceRandom;
use rand::thread_rng;
use std::time::Duration;

pub struct EsploraSource {
    bases: Vec<String>,
    tor_client: Option<reqwest::blocking::Client>,
    direct_client: Option<reqwest::blocking::Client>,
    fallback_clearnet: bool,
    allow_clearnet: bool,
    fallback_warned: std::sync::atomic::AtomicBool,
}

const MAX_BLOCK_BYTES: u64 = 8 * 1024 * 1024;

impl EsploraSource {
    pub fn new(
        bases: Vec<String>,
        tor_socks: Option<String>,
        allow_clearnet: bool,
        fallback_clearnet: bool,
        timeout: Duration,
    ) -> Result<Self> {
        let tor_client = tor_socks.and_then(|socks| {
            reqwest::blocking::Client::builder()
                .timeout(timeout)
                .user_agent(concat!("e-lite/", env!("CARGO_PKG_VERSION")))
                .proxy(reqwest::Proxy::all(format!("socks5h://{socks}")).ok()?)
                .build()
                .ok()
        });
        let direct_client = if allow_clearnet || fallback_clearnet {
            reqwest::blocking::Client::builder()
                .timeout(timeout)
                .user_agent(concat!("e-lite/", env!("CARGO_PKG_VERSION")))
                .build()
                .ok()
        } else {
            None
        };

        let mut clean = Vec::new();
        for base in bases {
            let b = base.trim_end_matches('/').to_string();
            if b.is_empty() {
                continue;
            }
            let lower = b.to_ascii_lowercase();
            if !lower.starts_with("http://") && !lower.starts_with("https://") {
                continue;
            }
            let is_onion = lower.contains(".onion");
            // Onion endpoints only work through Tor; clearnet works through Tor
            // or, when allowed/fallback is configured, directly.
            if is_onion && tor_client.is_none() {
                continue;
            }
            if !is_onion && tor_client.is_none() && direct_client.is_none() {
                continue;
            }
            clean.push(b);
        }
        if clean.is_empty() {
            return Err(RawtxError::Config(
                "no usable esplora endpoints (need Tor or clearnet access)".into(),
            ));
        }

        Ok(EsploraSource {
            bases: clean,
            tor_client,
            direct_client,
            fallback_clearnet,
            allow_clearnet,
            fallback_warned: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn fetch_with(
        client: &reqwest::blocking::Client,
        base: &str,
        hash: &Hash32,
    ) -> Result<Vec<u8>> {
        let url = format!("{}/block/{}/raw", base, hash.to_hex());
        let resp = client
            .get(&url)
            .send()
            .map_err(|e| RawtxError::Unavailable(format!("{base}: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(RawtxError::Unavailable(format!("{base}: HTTP {status}")));
        }
        if resp.content_length().unwrap_or(0) > MAX_BLOCK_BYTES {
            return Err(RawtxError::Unavailable(format!("{base}: block too large")));
        }
        let bytes = resp
            .bytes()
            .map_err(|e| RawtxError::Unavailable(format!("{base}: {e}")))?;
        if bytes.len() as u64 > MAX_BLOCK_BYTES {
            return Err(RawtxError::Unavailable(format!("{base}: block too large")));
        }
        Ok(bytes.to_vec())
    }
}

impl BlockSource for EsploraSource {
    fn name(&self) -> &str {
        "esplora"
    }

    fn fetch_block(&self, hash: &Hash32) -> Result<Vec<u8>> {
        let mut order: Vec<usize> = (0..self.bases.len()).collect();
        order.shuffle(&mut thread_rng());
        let mut errors = Vec::new();
        for idx in order {
            let base = &self.bases[idx];
            let is_onion = base.to_ascii_lowercase().contains(".onion");
            if let Some(client) = &self.tor_client {
                match Self::fetch_with(client, base, hash) {
                    Ok(bytes) => return Ok(bytes),
                    Err(e) => errors.push(e.to_string()),
                }
            }
            if !is_onion {
                if let Some(client) = &self.direct_client {
                    let via_fallback = self.fallback_clearnet && !self.allow_clearnet;
                    match Self::fetch_with(client, base, hash) {
                        Ok(bytes) => {
                            if via_fallback
                                && !self
                                    .fallback_warned
                                    .swap(true, std::sync::atomic::Ordering::Relaxed)
                            {
                                eprintln!(
                                    "[e-lite] WARNING: Tor request failed; using clearnet for {base} (operator IP exposed)"
                                );
                            }
                            return Ok(bytes);
                        }
                        Err(e) => errors.push(e.to_string()),
                    }
                }
            }
        }
        Err(RawtxError::Unavailable(errors.join("; ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn serve_once(body: Vec<u8>) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf);
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            sock.write_all(resp.as_bytes()).unwrap();
            sock.write_all(&body).unwrap();
        });
        (format!("http://{addr}"), handle)
    }

    #[test]
    fn fetches_whole_block() {
        let (base, handle) = serve_once(vec![1, 2, 3, 4]);
        let src = EsploraSource::new(
            vec![base],
            None,
            true, // local test server
            false,
            Duration::from_secs(5),
        )
        .unwrap();
        let got = src.fetch_block(&Hash32([0x11; 32])).unwrap();
        assert_eq!(got, vec![1, 2, 3, 4]);
        handle.join().unwrap();
    }

    #[test]
    fn refuses_clearnet_without_tor() {
        let err = EsploraSource::new(
            vec!["https://blockstream.info/api".into()],
            None,
            false,
            false,
            Duration::from_secs(5),
        );
        assert!(err.is_err());
    }

    #[test]
    fn refuses_non_http_endpoints() {
        let err = EsploraSource::new(
            vec!["file:///tmp".into()],
            None,
            true,
            false,
            Duration::from_secs(5),
        );
        assert!(err.is_err());
    }

    #[test]
    fn onion_requires_tor() {
        let err = EsploraSource::new(
            vec!["http://example.onion/api".into()],
            None,
            true,
            false,
            Duration::from_secs(5),
        );
        assert!(err.is_err());
    }
}
