//! Esplora-compatible source: `GET /block/:hash/raw` (whole block only).
//!
//! Requests never mention a TXID. With Tor configured, the client uses a
//! SOCKS5h proxy and `.onion` endpoints are allowed; without Tor, external
//! clearnet is refused unless `allow_clearnet` is explicitly enabled.

use crate::health::{Blame, HealthMap, TRANSIENT_BASE, TRANSIENT_CAP};
use crate::{AttemptCtl, BlockSource, Endpoint, FetchMode, RawtxError, Result};
use el_model::Hash32;
use rand::seq::SliceRandom;
use rand::thread_rng;
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub struct EsploraSource {
    bases: Vec<String>,
    /// Tor SOCKS address. Every block request builds its own client with a
    /// random SOCKS username, so Tor puts it on its own circuit
    /// (`IsolateSOCKSAuth`, on by default): providers cannot link the blocks
    /// of one wallet by exit IP (AUD-135).
    tor_proxy: Option<String>,
    /// Absolute limit of one attempt (Tor or direct). A slow transfer that
    /// keeps receiving bytes may use all of it (AUD-155).
    attempt_limit: Duration,
    /// Time to build the circuit and connect to the provider through Tor.
    tor_connect: Duration,
    /// No byte (or no response header) for this long through Tor: stalled.
    tor_stall: Duration,
    /// Direct (clearnet) client; its `timeout` is the stall/header timeout
    /// (`request_timeout_seconds`).
    direct_client: Option<reqwest::blocking::Client>,
    fallback_clearnet: bool,
    allow_clearnet: bool,
    fallback_warned: AtomicBool,
    /// Cooldown after a hard failure (wrong answer from the provider).
    cooldown: Duration,
    tor_health: HealthMap,
    direct_health: HealthMap,
}

const MAX_BLOCK_BYTES: u64 = 8 * 1024 * 1024;
const READ_CHUNK: usize = 64 * 1024;
/// Defaults of the Tor connect and stall timeouts (`[rawtx]
/// tor_connect_timeout_seconds` / `tor_stall_timeout_seconds`).
/// A Tor attempt that has not connected or answered by then is hedged by the
/// resolver anyway (AUD-160); waiting longer only holds a circuit that Tor
/// itself would already be retrying elsewhere. `.onion` endpoints get twice
/// these limits (rendezvous circuits take longer).
pub const DEFAULT_TOR_CONNECT: Duration = Duration::from_secs(10);
pub const DEFAULT_TOR_STALL: Duration = Duration::from_secs(12);

/// A client whose SOCKS username is random. Tor isolates streams with
/// different SOCKS credentials onto different circuits.
pub fn isolated_tor_client(socks: &str, timeout: Duration) -> Option<reqwest::blocking::Client> {
    isolated_tor_client_with(socks, timeout, timeout)
}

/// `isolated_tor_client` with separate limits. In the blocking client the
/// client `timeout` bounds the wait for the response headers **and each body
/// read**, so it is an inactivity timeout, not a total one; `connect` bounds
/// the circuit plus the TCP/TLS connection.
pub fn isolated_tor_client_with(
    socks: &str,
    connect: Duration,
    stall: Duration,
) -> Option<reqwest::blocking::Client> {
    let user = format!("el{:016x}", rand::random::<u64>());
    reqwest::blocking::Client::builder()
        .connect_timeout(connect.min(stall))
        .timeout(stall)
        .user_agent(crate::HTTP_USER_AGENT)
        .proxy(reqwest::Proxy::all(format!("socks5h://{user}:x@{socks}")).ok()?)
        .build()
        .ok()
}

/// What went wrong with a request, without the URL (it carries the block
/// hash of a wallet transaction, AUD-132).
fn describe_reqwest(e: &reqwest::Error) -> String {
    let what = if e.is_timeout() {
        "timeout"
    } else if e.is_connect() {
        "connect failed"
    } else if e.is_body() || e.is_decode() {
        "body read failed"
    } else if e.is_request() {
        "error sending request"
    } else {
        "request failed"
    };
    // The innermost cause (an io or SOCKS error) says why; it never carries
    // the URL.
    let mut source: Option<&dyn std::error::Error> = std::error::Error::source(e);
    let mut last = None;
    while let Some(s) = source {
        last = Some(s.to_string());
        source = s.source();
    }
    match last {
        Some(cause) if !cause.is_empty() && !e.is_timeout() => {
            crate::redact_hashes(&format!("{what} ({cause})"))
        }
        _ => what.to_string(),
    }
}

/// A body read error: reqwest wraps its own timeout in an `io::Error`.
fn describe_io(e: &std::io::Error) -> String {
    if let Some(inner) = e.get_ref().and_then(|i| i.downcast_ref::<reqwest::Error>()) {
        return describe_reqwest(inner);
    }
    match e.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => "stalled".into(),
        kind => format!("body read failed ({kind})"),
    }
}

/// `Retry-After` in seconds (the HTTP-date form is ignored).
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

type Attempt = std::result::Result<Vec<u8>, (RawtxError, Blame)>;

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

    /// Like `new`, with `attempt_limit` as the absolute limit of one attempt
    /// (`[rawtx] tor_request_timeout_seconds`). The Tor connect/stall limits
    /// default to `DEFAULT_TOR_CONNECT`/`DEFAULT_TOR_STALL`, never above it.
    #[allow(clippy::too_many_arguments)]
    pub fn with_budget(
        bases: Vec<String>,
        tor_socks: Option<String>,
        allow_clearnet: bool,
        fallback_clearnet: bool,
        timeout: Duration,
        attempt_limit: Duration,
        cooldown: Duration,
    ) -> Result<Self> {
        // Keep the proxy only if a client can actually be built with it.
        let tor_proxy =
            tor_socks.filter(|socks| isolated_tor_client(socks, attempt_limit).is_some());
        let direct_client = if allow_clearnet || fallback_clearnet {
            reqwest::blocking::Client::builder()
                .connect_timeout(timeout)
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

        let attempt_limit = attempt_limit.max(Duration::from_secs(1));
        Ok(EsploraSource {
            bases: clean,
            tor_proxy,
            attempt_limit,
            tor_connect: DEFAULT_TOR_CONNECT.min(attempt_limit),
            tor_stall: DEFAULT_TOR_STALL.min(attempt_limit),
            direct_client,
            fallback_clearnet,
            allow_clearnet,
            fallback_warned: AtomicBool::new(false),
            cooldown,
            tor_health: HealthMap::new(),
            direct_health: HealthMap::new(),
        })
    }

    /// Set the Tor circuit/connect and stall timeouts (capped by the attempt
    /// limit).
    pub fn with_tor_timeouts(mut self, connect: Duration, stall: Duration) -> Self {
        self.tor_connect = connect.max(Duration::from_secs(1)).min(self.attempt_limit);
        self.tor_stall = stall.max(Duration::from_secs(1)).min(self.attempt_limit);
        self
    }

    /// One attempt: headers within the client's stall timeout, then the body
    /// in chunks while bytes keep arriving, up to `deadline`.
    fn fetch_with(
        client: &reqwest::blocking::Client,
        base: &str,
        hash: &Hash32,
        deadline: Instant,
        ctl: &AttemptCtl,
    ) -> Attempt {
        let url = format!("{}/block/{}/raw", base, hash.to_hex());
        let unavailable = |msg: String| RawtxError::Unavailable(format!("{base}: {msg}"));
        let mut resp = client.get(&url).send().map_err(|e| {
            // No connection, or no response header in time: the endpoint
            // never answered (AUD-160).
            let blame = if e.is_connect() || e.is_timeout() {
                Blame::Unreachable
            } else {
                Blame::Transient
            };
            (unavailable(describe_reqwest(&e)), blame)
        })?;
        let status = resp.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS
            || status == reqwest::StatusCode::SERVICE_UNAVAILABLE
        {
            let wait = retry_after(resp.headers());
            return Err((
                unavailable(format!("HTTP {status}")),
                Blame::RateLimited(wait),
            ));
        }
        if status.is_server_error() {
            return Err((unavailable(format!("HTTP {status}")), Blame::Transient));
        }
        if !status.is_success() {
            return Err((unavailable(format!("HTTP {status}")), Blame::Hard));
        }
        let expected = resp.content_length();
        if expected.unwrap_or(0) > MAX_BLOCK_BYTES {
            return Err((unavailable("block too large".into()), Blame::Hard));
        }
        // Bounded, chunked read: without Content-Length (chunked) the body
        // must not be buffered past the limit (AUD-144), and a transfer that
        // keeps receiving bytes is not cut by a total timeout (AUD-155).
        let mut bytes = Vec::with_capacity(expected.unwrap_or(0).min(MAX_BLOCK_BYTES) as usize);
        let mut buf = vec![0u8; READ_CHUNK];
        loop {
            if ctl.aborted() {
                return Err((unavailable("aborted".into()), Blame::Transient));
            }
            if Instant::now() >= deadline {
                return Err((
                    unavailable(format!(
                        "timeout (attempt limit, {} KiB received)",
                        bytes.len() / 1024
                    )),
                    Blame::Transient,
                ));
            }
            match resp.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    ctl.add_bytes(n);
                    bytes.extend_from_slice(&buf[..n]);
                    if bytes.len() as u64 > MAX_BLOCK_BYTES {
                        return Err((unavailable("block too large".into()), Blame::Hard));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    return Err((
                        unavailable(format!(
                            "{} after {} KiB",
                            describe_io(&e),
                            bytes.len() / 1024
                        )),
                        Blame::Transient,
                    ))
                }
            }
        }
        if expected.is_some_and(|n| n != bytes.len() as u64) {
            return Err((unavailable("truncated body".into()), Blame::Transient));
        }
        Ok(bytes)
    }

    fn fail(&self, map: &HealthMap, base: &str, blame: Blame) {
        map.fail(base, blame, TRANSIENT_BASE, TRANSIENT_CAP, self.cooldown);
    }

    /// Record a failure of a Tor attempt. A 429/503 over Tor is about one
    /// exit: the provider is paused only when several distinct circuits were
    /// limited within a minute (AUD-160).
    fn fail_tor(&self, base: &str, blame: Blame) {
        match blame {
            Blame::RateLimited(_) => {
                if self.tor_health.circuit_limited(base) {
                    self.fail(&self.tor_health, base, blame);
                }
            }
            _ => self.fail(&self.tor_health, base, blame),
        }
    }

    /// Tor connect and stall limits for `base` (doubled for `.onion`).
    fn tor_limits(&self, base: &str) -> (Duration, Duration) {
        let factor = if is_onion(base) { 2 } else { 1 };
        (
            (self.tor_connect * factor).min(self.attempt_limit),
            (self.tor_stall * factor).min(self.attempt_limit),
        )
    }

    /// Endpoints usable now: each provider through Tor (tier 0) and, when
    /// clearnet is allowed, directly (tier 0 without Tor, tier 1 with it, so
    /// Tor is always tried first).
    fn usable_endpoints(&self) -> Vec<Endpoint> {
        let mut out = Vec::new();
        for base in &self.bases {
            if self.tor_proxy.is_some() && !self.tor_health.cooling(base) {
                out.push(Endpoint {
                    key: format!("tor {base}"),
                    tier: 0,
                    via_tor: true,
                });
            }
            if !is_onion(base) && self.direct_client.is_some() && !self.direct_health.cooling(base)
            {
                out.push(Endpoint {
                    key: format!("direct {base}"),
                    tier: if self.tor_proxy.is_some() { 1 } else { 0 },
                    via_tor: false,
                });
            }
        }
        out
    }

    fn attemptable(&self, base: &str) -> bool {
        let is_onion = base.to_ascii_lowercase().contains(".onion");
        let tor_ok = self.tor_proxy.is_some() && !self.tor_health.cooling(base);
        let direct_ok =
            !is_onion && self.direct_client.is_some() && !self.direct_health.cooling(base);
        tor_ok || direct_ok
    }

    /// When every provider is cooling down, release the one whose cooldown
    /// ends first instead of failing the wallet request without touching the
    /// network. Its failure streak is kept, so it backs off further if it
    /// fails again.
    fn force_one(&self, order: &[usize]) {
        let ends = |i: usize| {
            let base = &self.bases[i];
            let tor = self.tor_health.until(base);
            let direct = self.direct_health.until(base);
            tor.into_iter().chain(direct).min()
        };
        let Some(idx) = order.iter().copied().min_by_key(|&i| ends(i)) else {
            return;
        };
        let base = &self.bases[idx];
        self.tor_health.release(base);
        self.direct_health.release(base);
    }

    /// The deadline of the next attempt, or `None` once the call's deadline
    /// leaves too little time.
    fn attempt_deadline(&self, deadline: Instant) -> Option<Instant> {
        let now = Instant::now();
        let left = deadline.saturating_duration_since(now);
        (left >= crate::MIN_ATTEMPT.min(self.attempt_limit))
            .then(|| now + left.min(self.attempt_limit))
    }
}

fn is_onion(base: &str) -> bool {
    base.to_ascii_lowercase().contains(".onion")
}

impl BlockSource for EsploraSource {
    fn name(&self) -> &str {
        "esplora"
    }

    fn endpoints(&self, mode: FetchMode) -> Vec<Endpoint> {
        let mut out = self.usable_endpoints();
        if out.is_empty() && mode == FetchMode::Normal {
            // Every provider is cooling down: release the one whose pause
            // ends first rather than failing without touching the network.
            let order: Vec<usize> = (0..self.bases.len()).collect();
            self.force_one(&order);
            out = self.usable_endpoints();
        }
        out.shuffle(&mut thread_rng());
        out
    }

    fn fetch_endpoint(
        &self,
        endpoint: &Endpoint,
        hash: &Hash32,
        deadline: Instant,
        mode: FetchMode,
        ctl: &AttemptCtl,
    ) -> Result<Vec<u8>> {
        let quiet = mode == FetchMode::Quiet;
        let Some(attempt) = self.attempt_deadline(deadline) else {
            return Err(RawtxError::Unavailable("deadline reached".into()));
        };
        if let Some(base) = endpoint.key.strip_prefix("tor ") {
            let socks = self
                .tor_proxy
                .as_deref()
                .ok_or_else(|| RawtxError::Unavailable(format!("tor {base}: no proxy")))?;
            let (connect, stall) = self.tor_limits(base);
            let client = isolated_tor_client_with(socks, connect, stall).ok_or_else(|| {
                RawtxError::Unavailable(format!("tor {base}: client unavailable"))
            })?;
            return match Self::fetch_with(&client, base, hash, attempt, ctl) {
                Ok(bytes) => {
                    self.tor_health.succeed(base);
                    Ok(bytes)
                }
                Err((e, blame)) => {
                    // A decoy or a losing hedge never cools a provider down.
                    if !quiet && !ctl.aborted() {
                        self.fail_tor(base, blame);
                    }
                    Err(RawtxError::Unavailable(format!("tor {e}")))
                }
            };
        }
        if let Some(base) = endpoint.key.strip_prefix("direct ") {
            let client = self
                .direct_client
                .as_ref()
                .ok_or_else(|| RawtxError::Unavailable(format!("direct {base}: no client")))?;
            let via_fallback = self.fallback_clearnet && !self.allow_clearnet;
            return match Self::fetch_with(client, base, hash, attempt, ctl) {
                Ok(bytes) => {
                    self.direct_health.succeed(base);
                    if via_fallback && !self.fallback_warned.swap(true, Ordering::Relaxed) {
                        eprintln!(
                            "[e-lite] WARNING: Tor request failed; using clearnet for {base} (operator IP exposed)"
                        );
                    }
                    Ok(bytes)
                }
                Err((e, blame)) => {
                    if !quiet && !ctl.aborted() {
                        self.fail(&self.direct_health, base, blame);
                    }
                    Err(RawtxError::Unavailable(format!("direct {e}")))
                }
            };
        }
        Err(RawtxError::Unavailable("unknown esplora endpoint".into()))
    }

    fn fetch_block(&self, hash: &Hash32) -> Result<Vec<u8>> {
        self.fetch_block_until(
            hash,
            Instant::now() + crate::EXTERNAL_BUDGET,
            FetchMode::Normal,
        )
    }

    fn under_pressure(&self) -> bool {
        self.tor_health.any_limited() || self.direct_health.any_limited()
    }

    fn fetch_block_until(
        &self,
        hash: &Hash32,
        deadline: Instant,
        mode: FetchMode,
    ) -> Result<Vec<u8>> {
        let mut order: Vec<usize> = (0..self.bases.len()).collect();
        order.shuffle(&mut thread_rng());
        // Purely random order (AUD-135): preferring the provider that answered
        // last sent every block of a wallet to the same one. A dead provider
        // is not paid for on every block because a failure cools it down.
        if !order.iter().any(|&i| self.attemptable(&self.bases[i])) {
            if mode == FetchMode::Quiet {
                return Err(RawtxError::Unavailable(
                    "all providers are cooling down".into(),
                ));
            }
            self.force_one(&order);
        }
        let quiet = mode == FetchMode::Quiet;

        let mut errors = Vec::new();
        for idx in order {
            let base = &self.bases[idx];
            let is_onion = base.to_ascii_lowercase().contains(".onion");

            if let Some(socks) = self.tor_proxy.as_deref() {
                if self.tor_health.cooling(base) {
                    errors.push(format!("tor {base}: cooling down"));
                } else if let Some(attempt) = self.attempt_deadline(deadline) {
                    let (connect, stall) = self.tor_limits(base);
                    let Some(client) = isolated_tor_client_with(socks, connect, stall) else {
                        errors.push(format!("tor {base}: client unavailable"));
                        continue;
                    };
                    match Self::fetch_with(&client, base, hash, attempt, &AttemptCtl::new()) {
                        Ok(bytes) => {
                            self.tor_health.succeed(base);
                            return Ok(bytes);
                        }
                        Err((e, blame)) => {
                            // A decoy never cools a provider down (AUD-157).
                            if !quiet {
                                self.fail_tor(base, blame);
                            }
                            errors.push(format!("tor {e}"));
                        }
                    }
                } else {
                    errors.push("deadline reached".to_string());
                    break;
                }
            }

            if !is_onion {
                if let Some(client) = &self.direct_client {
                    if self.direct_health.cooling(base) {
                        errors.push(format!("direct {base}: cooling down"));
                        continue;
                    }
                    let Some(attempt) = self.attempt_deadline(deadline) else {
                        errors.push("deadline reached".to_string());
                        break;
                    };
                    let via_fallback = self.fallback_clearnet && !self.allow_clearnet;
                    match Self::fetch_with(client, base, hash, attempt, &AttemptCtl::new()) {
                        Ok(bytes) => {
                            self.direct_health.succeed(base);
                            if via_fallback && !self.fallback_warned.swap(true, Ordering::Relaxed) {
                                eprintln!(
                                    "[e-lite] WARNING: Tor request failed; using clearnet for {base} (operator IP exposed)"
                                );
                            }
                            return Ok(bytes);
                        }
                        Err((e, blame)) => {
                            if !quiet {
                                self.fail(&self.direct_health, base, blame);
                            }
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

    /// Serves `body` after the headers in `pieces` chunks, `gap` apart, then
    /// keeps the connection open for `hang`.
    fn serve_paced(body: Vec<u8>, pieces: usize, gap: Duration, hang: Duration) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            while let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf);
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes());
                let step = body.len().div_ceil(pieces);
                for chunk in body.chunks(step) {
                    std::thread::sleep(gap);
                    if sock.write_all(chunk).is_err() {
                        break;
                    }
                }
                std::thread::sleep(hang);
            }
        });
        format!("http://{addr}")
    }

    #[test]
    fn a_slow_but_live_body_is_not_cut_short() {
        // AUD-155: 2 s of steady transfer with a 1 s stall timeout.
        let body = vec![7u8; 64 * 1024];
        let base = serve_paced(body.clone(), 8, Duration::from_millis(250), Duration::ZERO);
        let src = EsploraSource::with_budget(
            vec![base],
            None,
            true,
            false,
            Duration::from_secs(1),
            Duration::from_secs(10),
            Duration::from_secs(60),
        )
        .unwrap();
        assert_eq!(src.fetch_block(&Hash32([0x61; 32])).unwrap(), body);
    }

    #[test]
    fn a_stalled_body_is_a_short_transient_failure() {
        let body = vec![7u8; 64 * 1024];
        // Half the body arrives, then nothing.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let half = body.len() / 2;
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf);
                let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
                let _ = sock.write_all(head.as_bytes());
                let _ = sock.write_all(&body[..half]);
                std::thread::sleep(Duration::from_secs(5));
            }
        });
        let base = format!("http://{addr}");
        let src = EsploraSource::with_budget(
            vec![base.clone()],
            None,
            true,
            false,
            Duration::from_secs(1),
            Duration::from_secs(10),
            Duration::from_secs(300),
        )
        .unwrap();
        let started = Instant::now();
        let err = src
            .fetch_block(&Hash32([0x62; 32]))
            .unwrap_err()
            .to_string();
        assert!(started.elapsed() < Duration::from_secs(4));
        assert!(err.contains("timeout") || err.contains("stalled"), "{err}");
        assert!(!err.contains("other error"), "{err}");
        // Transient: the backoff starts at 5 s, not at the 300 s cooldown.
        let until = src.direct_health.until(&base).unwrap();
        assert!(until <= Instant::now() + Duration::from_secs(6));
    }

    #[test]
    fn a_rate_limit_moves_on_and_honours_retry_after() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let limited_hits = Arc::new(AtomicUsize::new(0));
        let hits = Arc::clone(&limited_hits);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let limited = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            while let Ok((mut sock, _)) = listener.accept() {
                hits.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf);
                let _ = sock.write_all(
                    b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 30\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            }
        });
        let good = serve_ok(vec![1, 2, 3], Arc::new(AtomicUsize::new(0)));
        let src = EsploraSource::new(
            vec![limited.clone(), good],
            None,
            true,
            false,
            Duration::from_secs(2),
            Duration::from_secs(300),
        )
        .unwrap();
        for i in 0..20u8 {
            // The same request moves on to the healthy provider.
            assert_eq!(src.fetch_block(&Hash32([i; 32])).unwrap(), vec![1, 2, 3]);
            if limited_hits.load(Ordering::SeqCst) > 0 {
                break;
            }
        }
        assert_eq!(limited_hits.load(Ordering::SeqCst), 1);
        assert!(src.under_pressure());
        let until = src.direct_health.until(&limited).unwrap();
        let left = until.saturating_duration_since(Instant::now());
        assert!(
            left > Duration::from_secs(25) && left <= Duration::from_secs(30),
            "{left:?}"
        );
    }

    #[test]
    fn a_decoy_failure_does_not_cool_a_provider() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let requests = Arc::new(AtomicUsize::new(0));
        let bad = serve_500(Arc::clone(&requests));
        let src = EsploraSource::new(
            vec![bad.clone()],
            None,
            true,
            false,
            Duration::from_secs(2),
            Duration::from_secs(60),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        assert!(src
            .fetch_block_until(&Hash32([0x63; 32]), deadline, FetchMode::Quiet)
            .is_err());
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        assert!(src.direct_health.until(&bad).is_none());
    }

    #[test]
    fn a_rate_limit_over_tor_pauses_the_provider_only_after_several_circuits() {
        // AUD-160: over Tor a 429 is about one exit; the next request uses
        // another circuit, so one limited circuit must not stop the provider.
        let src = EsploraSource::new(
            vec!["https://provider.example/api".into()],
            Some("127.0.0.1:9".into()),
            false,
            false,
            Duration::from_secs(2),
            Duration::from_secs(60),
        )
        .unwrap();
        let base = "https://provider.example/api";
        let limited = Blame::RateLimited(Some(Duration::from_secs(30)));
        src.fail_tor(base, limited);
        src.fail_tor(base, limited);
        assert!(!src.tor_health.cooling(base));
        src.fail_tor(base, limited);
        assert!(src.tor_health.cooling(base));
    }

    #[test]
    fn onion_endpoints_get_longer_tor_limits_and_tor_comes_first() {
        let src = EsploraSource::with_budget(
            vec![
                "https://provider.example/api".into(),
                "http://abc.onion/api".into(),
            ],
            Some("127.0.0.1:9".into()),
            true,
            false,
            Duration::from_secs(2),
            Duration::from_secs(60),
            Duration::from_secs(60),
        )
        .unwrap();
        let (c, st) = src.tor_limits("https://provider.example/api");
        let (oc, ost) = src.tor_limits("http://abc.onion/api");
        assert_eq!((oc, ost), (c * 2, st * 2));
        let eps = src.endpoints(FetchMode::Normal);
        assert_eq!(eps.iter().filter(|e| e.tier == 0).count(), 2);
        // The clearnet endpoint exists only for the clearnet provider, as
        // the second tier.
        let direct: Vec<_> = eps.iter().filter(|e| !e.via_tor).collect();
        assert_eq!(direct.len(), 1);
        assert_eq!(direct[0].tier, 1);
    }
}
