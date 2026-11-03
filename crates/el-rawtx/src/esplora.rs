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
    client: reqwest::blocking::Client,
}

const MAX_BLOCK_BYTES: u64 = 8 * 1024 * 1024;

impl EsploraSource {
    pub fn new(
        bases: Vec<String>,
        tor_socks: Option<String>,
        allow_clearnet: bool,
        timeout: Duration,
    ) -> Result<Self> {
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
            // Skip endpoints we cannot use instead of disabling the whole
            // source (e.g. a mainnet onion URL with allow_clearnet but no Tor).
            if is_onion && tor_socks.is_none() {
                continue;
            }
            if !is_onion && tor_socks.is_none() && !allow_clearnet {
                continue;
            }
            clean.push(b);
        }
        if clean.is_empty() {
            return Err(RawtxError::Config(
                "no usable esplora endpoints (need Tor or allow_clearnet=true)".into(),
            ));
        }

        let mut builder = reqwest::blocking::Client::builder()
            .timeout(timeout)
            .user_agent(concat!("e-lite/", env!("CARGO_PKG_VERSION")));
        if let Some(socks) = tor_socks {
            let proxy = reqwest::Proxy::all(format!("socks5h://{socks}"))
                .map_err(|e| RawtxError::Config(format!("bad tor proxy: {e}")))?;
            builder = builder.proxy(proxy);
        }
        let client = builder
            .build()
            .map_err(|e| RawtxError::Config(format!("http client: {e}")))?;
        Ok(EsploraSource {
            bases: clean,
            client,
        })
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
            let url = format!("{}/block/{}/raw", base, hash.to_hex());
            match self.client.get(&url).send() {
                Ok(resp) => {
                    let status = resp.status();
                    if !status.is_success() {
                        errors.push(format!("{base}: HTTP {status}"));
                        continue;
                    }
                    if resp.content_length().unwrap_or(0) > MAX_BLOCK_BYTES {
                        errors.push(format!("{base}: block too large"));
                        continue;
                    }
                    match resp.bytes() {
                        Ok(bytes) if bytes.len() as u64 <= MAX_BLOCK_BYTES => {
                            return Ok(bytes.to_vec())
                        }
                        Ok(_) => errors.push(format!("{base}: block too large")),
                        Err(e) => errors.push(format!("{base}: {e}")),
                    }
                }
                Err(e) => errors.push(format!("{base}: {e}")),
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
            Duration::from_secs(5),
        );
        assert!(err.is_err());
    }
}
