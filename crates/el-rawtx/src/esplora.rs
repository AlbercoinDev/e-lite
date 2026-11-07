//! Esplora-compatible source: `GET /block/:hash/raw` (whole block only).
//!
//! Requests never mention a TXID. With Tor configured, the client uses a
//! SOCKS5h proxy and `.onion` endpoints are allowed; without Tor, external
//! clearnet is refused unless `allow_clearnet` is explicitly enabled.

use crate::{BlockSource, RawtxError, Result};
use el_model::Hash32;
use rand::seq::SliceRandom;
use rand::thread_rng;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct EsploraSource {
    bases: Vec<String>,
    tor_client: Option<reqwest::blocking::Client>,
    direct_client: Option<reqwest::blocking::Client>,
    fallback_clearnet: bool,
    allow_clearnet: bool,
    fallback_warned: AtomicBool,
    cooldown: Duration,
    tor_failures: Mutex<HashMap<String, Instant>>,
    direct_failures: Mutex<HashMap<String, Instant>>,
    last_good: Mutex<Option<String>>,
}

const MAX_BLOCK_BYTES: u64 = 8 * 1024 * 1024;

impl EsploraSource {
    pub fn new(
        bases: Vec<String>,
        tor_socks: Option<String>,
        allow_clearnet: bool,
        fallback_clearnet: bool,
        timeout: Duration,
        cooldown: Duration,
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
            fallback_warned: AtomicBool::new(false),
            cooldown,
            tor_failures: Mutex::new(HashMap::new()),
            direct_failures: Mutex::new(HashMap::new()),
            last_good: Mutex::new(None),
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

    fn cooled(failures: &Mutex<HashMap<String, Instant>>, base: &str) -> bool {
        failures
            .lock()
            .map(|m| m.get(base).is_some_and(|until| *until > Instant::now()))
            .unwrap_or(false)
    }

    fn mark_failed(failures: &Mutex<HashMap<String, Instant>>, base: &str, cooldown: Duration) {
        if let Ok(mut m) = failures.lock() {
            m.insert(base.to_string(), Instant::now() + cooldown);
        }
    }

    fn mark_good(&self, base: &str, tor: bool) {
        if let Ok(mut good) = self.last_good.lock() {
            *good = Some(base.to_string());
        }
        let map = if tor {
            &self.tor_failures
        } else {
            &self.direct_failures
        };
        if let Ok(mut m) = map.lock() {
            m.remove(base);
        }
    }

    fn is_last_good(&self, base: &str) -> bool {
        self.last_good
            .lock()
            .map(|good| good.as_deref() == Some(base))
            .unwrap_or(false)
    }

    /// A provider that answered recently should not be disabled for the full
    /// cooldown after one transient error; retry it soon.
    fn failure_cooldown(&self, base: &str) -> Duration {
        if self.is_last_good(base) {
            self.cooldown.min(Duration::from_secs(30))
        } else {
            self.cooldown
        }
    }

    fn attemptable(&self, base: &str) -> bool {
        let is_onion = base.to_ascii_lowercase().contains(".onion");
        let tor_ok = self.tor_client.is_some() && !Self::cooled(&self.tor_failures, base);
        let direct_ok =
            !is_onion && self.direct_client.is_some() && !Self::cooled(&self.direct_failures, base);
        tor_ok || direct_ok
    }

    /// When every provider is cooling down, force one attempt instead of
    /// failing the wallet request without touching the network.
    fn force_one(&self, order: &[usize]) {
        let idx = order
            .iter()
            .copied()
            .find(|&i| self.is_last_good(&self.bases[i]))
            .or_else(|| order.first().copied());
        let Some(idx) = idx else {
            return;
        };
        let base = &self.bases[idx];
        if let Ok(mut m) = self.tor_failures.lock() {
            m.remove(base);
        }
        if let Ok(mut m) = self.direct_failures.lock() {
            m.remove(base);
        }
    }
}

impl BlockSource for EsploraSource {
    fn name(&self) -> &str {
        "esplora"
    }

    fn fetch_block(&self, hash: &Hash32) -> Result<Vec<u8>> {
        let mut order: Vec<usize> = (0..self.bases.len()).collect();
        order.shuffle(&mut thread_rng());
        // Providers that answered recently go first. This avoids paying a
        // dead provider's timeout on every block while keeping the rest
        // shuffled for less predictable traffic.
        let last_idx = self.last_good.lock().ok().and_then(|good| {
            good.as_ref()
                .and_then(|base| self.bases.iter().position(|b| b == base))
        });
        if let Some(idx) = last_idx {
            if let Some(pos) = order.iter().position(|&i| i == idx) {
                let idx = order.remove(pos);
                order.insert(0, idx);
            }
        }
        if !order.iter().any(|&i| self.attemptable(&self.bases[i])) {
            self.force_one(&order);
        }

        let mut errors = Vec::new();
        for idx in order {
            let base = &self.bases[idx];
            let is_onion = base.to_ascii_lowercase().contains(".onion");

            if let Some(client) = &self.tor_client {
                if Self::cooled(&self.tor_failures, base) {
                    errors.push(format!("tor {base}: cooling down"));
                } else {
                    match Self::fetch_with(client, base, hash) {
                        Ok(bytes) => {
                            self.mark_good(base, true);
                            return Ok(bytes);
                        }
                        Err(e) => {
                            let cooldown = self.failure_cooldown(base);
                            Self::mark_failed(&self.tor_failures, base, cooldown);
                            errors.push(format!("tor {e}"));
                        }
                    }
                }
            }

            if !is_onion {
                if let Some(client) = &self.direct_client {
                    if Self::cooled(&self.direct_failures, base) {
                        errors.push(format!("direct {base}: cooling down"));
                        continue;
                    }
                    let via_fallback = self.fallback_clearnet && !self.allow_clearnet;
                    match Self::fetch_with(client, base, hash) {
                        Ok(bytes) => {
                            self.mark_good(base, false);
                            if via_fallback && !self.fallback_warned.swap(true, Ordering::Relaxed) {
                                eprintln!(
                                    "[e-lite] WARNING: Tor request failed; using clearnet for {base} (operator IP exposed)"
                                );
                            }
                            return Ok(bytes);
                        }
                        Err(e) => {
                            let cooldown = self.failure_cooldown(base);
                            Self::mark_failed(&self.direct_failures, base, cooldown);
                            errors.push(format!("direct {e}"));
                        }
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
            Duration::from_secs(5),
        )
        .unwrap();
        let got = src.fetch_block(&Hash32([0x11; 32])).unwrap();
        assert_eq!(got, vec![1, 2, 3, 4]);
        handle.join().unwrap();
    }

    fn serve_ok(body: Vec<u8>, requests: std::sync::Arc<std::sync::atomic::AtomicUsize>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            while let Ok((mut sock, _)) = listener.accept() {
                requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes());
                let _ = sock.write_all(&body);
            }
        });
        format!("http://{addr}")
    }

    fn serve_500(requests: std::sync::Arc<std::sync::atomic::AtomicUsize>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            while let Ok((mut sock, _)) = listener.accept() {
                requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf);
                let _ = sock.write_all(
                    b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            }
        });
        format!("http://{addr}")
    }

    #[test]
    fn failed_provider_is_skipped_when_another_works() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let bad_requests = Arc::new(AtomicUsize::new(0));
        let good_requests = Arc::new(AtomicUsize::new(0));
        let bad = serve_500(Arc::clone(&bad_requests));
        let good = serve_ok(vec![1, 2, 3, 4], Arc::clone(&good_requests));
        let src = EsploraSource::new(
            vec![bad, good],
            None,
            true,
            false,
            Duration::from_secs(2),
            Duration::from_secs(60),
        )
        .unwrap();
        assert_eq!(
            src.fetch_block(&Hash32([0x22; 32])).unwrap(),
            vec![1, 2, 3, 4]
        );
        assert_eq!(
            src.fetch_block(&Hash32([0x23; 32])).unwrap(),
            vec![1, 2, 3, 4]
        );
        assert!(
            bad_requests.load(Ordering::SeqCst) <= 1,
            "the failed provider must not be retried on every block"
        );
        assert!(good_requests.load(Ordering::SeqCst) >= 2);
    }

    #[test]
    fn all_providers_cooling_down_still_get_one_attempt() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let requests = Arc::new(AtomicUsize::new(0));
        let bad = serve_500(Arc::clone(&requests));
        let src = EsploraSource::new(
            vec![bad],
            None,
            true,
            false,
            Duration::from_secs(2),
            Duration::from_secs(60),
        )
        .unwrap();
        assert!(src.fetch_block(&Hash32([0x24; 32])).is_err());
        assert!(src.fetch_block(&Hash32([0x25; 32])).is_err());
        assert_eq!(
            requests.load(Ordering::SeqCst),
            2,
            "each request gets one forced attempt instead of failing while cooling"
        );
    }

    #[test]
    fn refuses_clearnet_without_tor() {
        let err = EsploraSource::new(
            vec!["https://blockstream.info/api".into()],
            None,
            false,
            false,
            Duration::from_secs(5),
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
            Duration::from_secs(5),
        );
        assert!(err.is_err());
    }
}
