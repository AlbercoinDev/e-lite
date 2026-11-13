//! Esplora-compatible source: `GET /block/:hash/raw` (whole block only).
//!
//! Requests never mention a TXID. With Tor configured, the client uses a
//! SOCKS5h proxy and `.onion` endpoints are allowed; without Tor, external
//! clearnet is refused unless `allow_clearnet` is explicitly enabled.

use crate::{BlockSource, RawtxError, Result};
use el_model::Hash32;
use rand::seq::SliceRandom;
use rand::thread_rng;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct EsploraSource {
    bases: Vec<String>,
    /// Tor SOCKS address. Every block request builds its own client with a
    /// random SOCKS username, so Tor puts it on its own circuit
    /// (`IsolateSOCKSAuth`, on by default): providers cannot link the blocks
    /// of one wallet by exit IP (AUD-135).
    tor_proxy: Option<String>,
    /// Per-attempt timeout of a direct (clearnet) request.
    timeout: Duration,
    /// Per-attempt timeout of a request through Tor. Larger than `timeout`:
    /// every request builds a new circuit and then downloads a whole block.
    tor_timeout: Duration,
    direct_client: Option<reqwest::blocking::Client>,
    fallback_clearnet: bool,
    allow_clearnet: bool,
    fallback_warned: AtomicBool,
    cooldown: Duration,
    tor_failures: Mutex<HashMap<String, Instant>>,
    direct_failures: Mutex<HashMap<String, Instant>>,
    /// Providers that have answered at least once: after a failure they are
    /// cooled for a short time only. They get no priority in the order.
    good: Mutex<HashSet<String>>,
}

const MAX_BLOCK_BYTES: u64 = 8 * 1024 * 1024;

/// Total time one `fetch_block` call may spend across providers. It stays
/// below the 45 s that concurrent callers wait for a shared download.
const FETCH_BUDGET: Duration = Duration::from_secs(40);
/// An attempt with less time left than this is not started.
const MIN_ATTEMPT: Duration = Duration::from_secs(2);

/// A client whose SOCKS username is random. Tor isolates streams with
/// different SOCKS credentials onto different circuits.
pub fn isolated_tor_client(socks: &str, timeout: Duration) -> Option<reqwest::blocking::Client> {
    let user = format!("el{:016x}", rand::random::<u64>());
    reqwest::blocking::Client::builder()
        .timeout(timeout)
        .user_agent(crate::HTTP_USER_AGENT)
        .proxy(reqwest::Proxy::all(format!("socks5h://{user}:x@{socks}")).ok()?)
        .build()
        .ok()
}

impl EsploraSource {
    pub fn new(
        bases: Vec<String>,
        tor_socks: Option<String>,
        allow_clearnet: bool,
        fallback_clearnet: bool,
        timeout: Duration,
        cooldown: Duration,
    ) -> Result<Self> {
        Self::with_budget(
            bases,
            tor_socks,
            allow_clearnet,
            fallback_clearnet,
            timeout,
            timeout,
            cooldown,
        )
    }

    /// Like `new`, with a separate per-attempt timeout for requests that go
    /// through Tor.
    #[allow(clippy::too_many_arguments)]
    pub fn with_budget(
        bases: Vec<String>,
        tor_socks: Option<String>,
        allow_clearnet: bool,
        fallback_clearnet: bool,
        timeout: Duration,
        tor_timeout: Duration,
        cooldown: Duration,
    ) -> Result<Self> {
        // Keep the proxy only if a client can actually be built with it.
        let tor_proxy = tor_socks.filter(|socks| isolated_tor_client(socks, tor_timeout).is_some());
        let direct_client = if allow_clearnet || fallback_clearnet {
            reqwest::blocking::Client::builder()
                .timeout(timeout)
                .user_agent(crate::HTTP_USER_AGENT)
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
            if is_onion && tor_proxy.is_none() {
                continue;
            }
            if !is_onion && tor_proxy.is_none() && direct_client.is_none() {
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
            tor_proxy,
            timeout,
            tor_timeout,
            direct_client,
            fallback_clearnet,
            allow_clearnet,
            fallback_warned: AtomicBool::new(false),
            cooldown,
            tor_failures: Mutex::new(HashMap::new()),
            direct_failures: Mutex::new(HashMap::new()),
            good: Mutex::new(HashSet::new()),
        })
    }

    fn fetch_with(
        client: &reqwest::blocking::Client,
        base: &str,
        hash: &Hash32,
        timeout: Duration,
    ) -> Result<Vec<u8>> {
        let url = format!("{}/block/{}/raw", base, hash.to_hex());
        // `without_url`: reqwest errors print the request URL, which carries
        // the block hash of a wallet transaction (AUD-132).
        let resp = client
            .get(&url)
            .timeout(timeout)
            .send()
            .map_err(|e| RawtxError::Unavailable(format!("{base}: {}", e.without_url())))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(RawtxError::Unavailable(format!("{base}: HTTP {status}")));
        }
        if resp.content_length().unwrap_or(0) > MAX_BLOCK_BYTES {
            return Err(RawtxError::Unavailable(format!("{base}: block too large")));
        }
        // Bounded read: without Content-Length (chunked) the body must not be
        // buffered past the limit before it is checked (AUD-144).
        let mut bytes = Vec::new();
        resp.take(MAX_BLOCK_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| {
                RawtxError::Unavailable(format!("{base}: body read failed ({})", e.kind()))
            })?;
        if bytes.len() as u64 > MAX_BLOCK_BYTES {
            return Err(RawtxError::Unavailable(format!("{base}: block too large")));
        }
        Ok(bytes)
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
        if let Ok(mut good) = self.good.lock() {
            good.insert(base.to_string());
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

    fn is_known_good(&self, base: &str) -> bool {
        self.good
            .lock()
            .map(|good| good.contains(base))
            .unwrap_or(false)
    }

    /// A provider that answered recently should not be disabled for the full
    /// cooldown after one transient error; retry it soon.
    fn failure_cooldown(&self, base: &str) -> Duration {
        if self.is_known_good(base) {
            self.cooldown.min(Duration::from_secs(30))
        } else {
            self.cooldown
        }
    }

    fn attemptable(&self, base: &str) -> bool {
        let is_onion = base.to_ascii_lowercase().contains(".onion");
        let tor_ok = self.tor_proxy.is_some() && !Self::cooled(&self.tor_failures, base);
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
            .find(|&i| self.is_known_good(&self.bases[i]))
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
        // Purely random order (AUD-135): preferring the provider that answered
        // last sent every block of a wallet to the same one. A dead provider
        // is not paid for on every block because a failure cools it down.
        if !order.iter().any(|&i| self.attemptable(&self.bases[i])) {
            self.force_one(&order);
        }

        let started = Instant::now();
        // Time left for one attempt, or `None` once the call's budget is spent.
        let attempt_timeout = |limit: Duration| -> Option<Duration> {
            let left = FETCH_BUDGET.saturating_sub(started.elapsed());
            (left >= MIN_ATTEMPT.min(limit)).then(|| limit.min(left))
        };
        let mut errors = Vec::new();
        for idx in order {
            let base = &self.bases[idx];
            let is_onion = base.to_ascii_lowercase().contains(".onion");

            if let Some(socks) = self.tor_proxy.as_deref() {
                if Self::cooled(&self.tor_failures, base) {
                    errors.push(format!("tor {base}: cooling down"));
                } else if let Some(timeout) = attempt_timeout(self.tor_timeout) {
                    let Some(client) = isolated_tor_client(socks, timeout) else {
                        errors.push(format!("tor {base}: client unavailable"));
                        continue;
                    };
                    match Self::fetch_with(&client, base, hash, timeout) {
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
                } else {
                    errors.push("fetch budget exceeded".to_string());
                    break;
                }
            }

            if !is_onion {
                if let Some(client) = &self.direct_client {
                    if Self::cooled(&self.direct_failures, base) {
                        errors.push(format!("direct {base}: cooling down"));
                        continue;
                    }
                    let Some(timeout) = attempt_timeout(self.timeout) else {
                        errors.push("fetch budget exceeded".to_string());
                        break;
                    };
                    let via_fallback = self.fallback_clearnet && !self.allow_clearnet;
                    match Self::fetch_with(client, base, hash, timeout) {
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
    fn healthy_providers_share_the_requests() {
        // AUD-135: no provider is preferred because it answered last.
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let counters: Vec<Arc<AtomicUsize>> =
            (0..3).map(|_| Arc::new(AtomicUsize::new(0))).collect();
        let bases = counters
            .iter()
            .map(|c| serve_ok(vec![1, 2, 3], Arc::clone(c)))
            .collect();
        let src = EsploraSource::new(
            bases,
            None,
            true,
            false,
            Duration::from_secs(5),
            Duration::from_secs(60),
        )
        .unwrap();
        for i in 0..90u8 {
            src.fetch_block(&Hash32([i; 32])).unwrap();
        }
        let seen: Vec<usize> = counters.iter().map(|c| c.load(Ordering::SeqCst)).collect();
        assert_eq!(seen.iter().sum::<usize>(), 90);
        // Uniform would be 30 each; a sticky provider would take ~88.
        assert!(seen.iter().all(|n| *n >= 10), "{seen:?}");
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
    fn errors_and_logs_do_not_carry_the_block_hash() {
        // AUD-132: a connection error must not echo the request URL, which
        // contains the block hash of a wallet transaction.
        let closed = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", closed.local_addr().unwrap());
        drop(closed);
        let src = EsploraSource::new(
            vec![base],
            None,
            true,
            false,
            Duration::from_secs(2),
            Duration::from_secs(2),
        )
        .unwrap();
        let hash = Hash32([0xab; 32]);
        let err = src.fetch_block(&hash).unwrap_err().to_string();
        assert!(!err.contains(&hash.to_hex()), "{err}");
        assert!(!err.contains("/block/"), "{err}");

        let line = crate::redact_hashes(&format!(
            "transaction {} is not in the block {}; tx 12ab",
            "cd".repeat(32),
            "ef".repeat(32)
        ));
        assert_eq!(
            line,
            "transaction <hash> is not in the block <hash>; tx 12ab"
        );
    }

    /// Minimal SOCKS5 server: records the username of each connection and
    /// then refuses the CONNECT, so the request fails without a network.
    fn socks_recorder(conns: usize) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let handle = std::thread::spawn(move || {
            let mut users = Vec::new();
            for _ in 0..conns {
                let (mut sock, _) = listener.accept().unwrap();
                let mut head = [0u8; 2];
                sock.read_exact(&mut head).unwrap();
                let mut methods = vec![0u8; head[1] as usize];
                sock.read_exact(&mut methods).unwrap();
                if !methods.contains(&2) {
                    users.push(String::new());
                    let _ = sock.write_all(&[5, 0xff]);
                    continue;
                }
                sock.write_all(&[5, 2]).unwrap();
                let mut ver_len = [0u8; 2];
                sock.read_exact(&mut ver_len).unwrap();
                let mut user = vec![0u8; ver_len[1] as usize];
                sock.read_exact(&mut user).unwrap();
                let mut plen = [0u8; 1];
                sock.read_exact(&mut plen).unwrap();
                let mut pass = vec![0u8; plen[0] as usize];
                sock.read_exact(&mut pass).unwrap();
                users.push(String::from_utf8_lossy(&user).into_owned());
                sock.write_all(&[1, 0]).unwrap();
                // Refuse the CONNECT: general SOCKS server failure.
                let _ = sock.write_all(&[5, 1, 0, 1, 0, 0, 0, 0, 0, 0]);
            }
            users
        });
        (addr, handle)
    }

    #[test]
    fn each_tor_request_gets_its_own_circuit() {
        // AUD-135: distinct SOCKS usernames => distinct Tor circuits.
        let (socks, handle) = socks_recorder(2);
        let src = EsploraSource::new(
            vec!["https://provider.invalid/api".into()],
            Some(socks),
            false,
            false,
            Duration::from_secs(5),
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(src.fetch_block(&Hash32([0x31; 32])).is_err());
        assert!(src.fetch_block(&Hash32([0x32; 32])).is_err());
        let users = handle.join().unwrap();
        assert_eq!(users.len(), 2);
        assert!(users.iter().all(|u| !u.is_empty()), "{users:?}");
        assert_ne!(users[0], users[1], "requests must not share a circuit");
    }

    #[test]
    fn chunked_body_above_the_limit_is_rejected() {
        // AUD-144: no Content-Length, body larger than a block can be.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf);
            let _ = sock.write_all(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            );
            let chunk = vec![0u8; 1024 * 1024];
            for _ in 0..10 {
                if sock
                    .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                    .is_err()
                    || sock.write_all(&chunk).is_err()
                    || sock.write_all(b"\r\n").is_err()
                {
                    return;
                }
            }
            let _ = sock.write_all(b"0\r\n\r\n");
        });
        let src = EsploraSource::new(
            vec![format!("http://{addr}")],
            None,
            true,
            false,
            Duration::from_secs(10),
            Duration::from_secs(5),
        )
        .unwrap();
        let err = src
            .fetch_block(&Hash32([0x41; 32]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("block too large"), "{err}");
    }

    #[test]
    fn requests_do_not_identify_e_lite() {
        // AUD-133: the User-Agent must not name the project or its version.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = vec![0u8; 4096];
            let n = sock.read(&mut buf).unwrap();
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\nx");
            String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase()
        });
        let src = EsploraSource::new(
            vec![format!("http://{addr}")],
            None,
            true,
            false,
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
        .unwrap();
        let _ = src.fetch_block(&Hash32([0x11; 32]));
        let request = handle.join().unwrap();
        assert!(!request.contains("e-lite"), "{request}");
        assert!(request.contains("user-agent: mozilla/5.0"), "{request}");
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
