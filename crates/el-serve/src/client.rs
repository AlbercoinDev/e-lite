//! client module (split from lib.rs in alpha.29.17.0).

use super::*;

#[derive(Debug)]
pub struct RateLimit {
    tokens: f64,
    capacity: f64,
    refill_per_sec: f64,
    last: Instant,
}

impl RateLimit {
    pub(crate) fn new(capacity: u64, refill_per_sec: u64) -> Self {
        let capacity = capacity.max(1) as f64;
        Self {
            tokens: capacity,
            capacity,
            refill_per_sec: refill_per_sec as f64,
            last: Instant::now(),
        }
    }

    /// Consume one token. When the bucket is empty, wait for the next token
    /// instead of rejecting the request: an initial wallet scan must never be
    /// aborted by a burst. Returns `true` when the request had to wait.
    ///
    /// A zero refill rate disables the limiter.
    pub(crate) fn acquire(&mut self, now: Instant) -> bool {
        if self.refill_per_sec <= 0.0 {
            return false;
        }
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        self.last = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            return false;
        }
        let wait = (1.0 - self.tokens) / self.refill_per_sec;
        if wait > 0.0 {
            std::thread::sleep(Duration::from_secs_f64(wait));
        }
        // The sleep produced exactly the token consumed here.
        self.tokens = 0.0;
        self.last = Instant::now();
        true
    }
}

#[derive(Clone)]
pub struct OutpointSub {
    pub script: ScriptHash,
    pub status: Value,
}

pub struct ClientState {
    pub id: u64,
    pub writer: Mutex<Wire>,
    pub subs: Mutex<HashMap<ScriptHash, Option<String>>>,
    /// Scripts subscribed through `blockchain.scriptpubkey.subscribe`, with
    /// their last status. Kept separate from scripthash subscriptions so a
    /// client using both receives the correct method-specific notification.
    pub spk_subs: Mutex<HashMap<ScriptHash, Option<String>>>,
    /// Outpoint subscriptions from `blockchain.outpoint.subscribe`.
    pub outpoints: Mutex<HashMap<(Hash32, u32), OutpointSub>>,
    /// Per-connection request budget (rate limiting).
    pub budget: Mutex<RateLimit>,
    pub headers: AtomicBool,
    /// Lines of this connection running beside its reader.
    pub background: AtomicUsize,
    pub last_height: AtomicU64,
    pub alive: AtomicBool,
    /// The client is gone but lines of this connection are still finishing:
    /// its `max_clients` slot and registry entry were given back early.
    pub detached: AtomicBool,
    pub tls: bool,
    /// Protocol version negotiated by the first `server.version` call.
    pub protocol: Mutex<Option<ProtocolVersion>>,
    /// True once any message other than `server.version` has been seen. 1.6+
    /// requires version negotiation to be the first message; older traffic
    /// caps the connection at 1.4.2.
    pub saw_message: AtomicBool,
    /// Aggregate counters for the `debug:` wallet-session line. Counts and
    /// milliseconds only: never a hash, script or txid (AUD-132/133).
    pub stats: Mutex<ClientStats>,
    /// Hex characters of `server.ping` pongs this connection may still
    /// request. A batch of 1000 maximum-size pings would otherwise build
    /// gigabytes (AUD-167).
    pub pong_budget: AtomicU64,
    /// Notifications waiting for the connection lock. The notifier never
    /// waits for it: the client's own thread may hold it while it writes a
    /// large response (30 s write timeout) or polls the socket. Whoever holds
    /// the lock delivers the queue when it releases it.
    pub pending_notes: Mutex<VecDeque<Vec<u8>>>,
}

/// Hex characters of `server.ping` pongs one connection may request in total.
/// Small enough that a batch cannot allocate gigabytes, large enough for any
/// real client.
pub const MAX_PONG_BYTES: u64 = 4 * 1024 * 1024;

/// Notifications a client may have queued behind its own traffic before it
/// is considered stuck and disconnected.
pub(crate) const MAX_PENDING_NOTES: usize = 4096;

/// Per-connection aggregates for the wallet reconstruction diagnostics. No
/// identifiers are stored: a wallet scan shows up as counts and latencies.
#[derive(Debug)]
pub struct ClientStats {
    pub opened: Instant,
    pub last_summary: Instant,
    /// Wallet requests (`get_history`, `transaction.get`) since the last
    /// summary line. Pings and subscriptions do not count: an idle wallet
    /// prints nothing.
    pub window_requests: u32,
    /// Lifetime counters, printed every 60 s of activity and on close.
    pub history_requests: u64,
    pub history_us_total: u64,
    pub history_us_max: u64,
    pub tx_gets: u64,
    pub tx_us_total: u64,
    pub tx_us_max: u64,
    /// JSON-RPC batches served on this connection (lines and items).
    pub batch_lines: u64,
    pub batch_items: u64,
    pub batch_max_items: u64,
    /// Cache and failure counters when the connection opened, for the
    /// session deltas. Every 60 s summary also prints the window delta.
    pub session_base: CacheBaseline,
    pub window_base: CacheBaseline,
    /// Milliseconds between the connection's first `get_history` and its
    /// first `transaction.get`: how long the wallet scanned before it started
    /// loading transactions.
    pub first_tx_after_history: Option<u64>,
    pub first_history_at: Option<Instant>,
}

impl ClientStats {
    fn empty() -> Self {
        let now = Instant::now();
        ClientStats {
            opened: now,
            last_summary: now,
            window_requests: 0,
            history_requests: 0,
            history_us_total: 0,
            history_us_max: 0,
            tx_gets: 0,
            tx_us_total: 0,
            tx_us_max: 0,
            batch_lines: 0,
            batch_items: 0,
            batch_max_items: 0,
            session_base: CacheBaseline::default(),
            window_base: CacheBaseline::default(),
            first_tx_after_history: None,
            first_history_at: None,
        }
    }
}

/// `512 B`, `3.4 KiB`, `12.1 MiB`, `1.2 GiB`.
pub fn display_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Latency for the wallet-session line: microseconds below 1 ms, so a fast
/// server does not read as `0 ms` (AUD-149).
pub(crate) fn fmt_latency(us: u64) -> String {
    if us < 1000 {
        format!("{us} us")
    } else {
        format!("{} ms", us / 1000)
    }
}

impl ClientState {
    /// Snapshot the process-wide cache/failure counters when the connection
    /// opens, so the session lines can report deltas instead of totals.
    pub(crate) fn set_baseline(&self, shared: &Shared) {
        if !debug_logs() {
            return;
        }
        let Ok(mut s) = self.stats.lock() else {
            return;
        };
        let base = CacheBaseline::capture(shared);
        s.session_base = base;
        s.window_base = base;
    }

    /// Record one dispatched request for the wallet-session diagnostics and
    /// print a summary every 60 s of activity. Counts, latencies and cache
    /// deltas only: never a hash, script or txid (AUD-132).
    pub(crate) fn record_request(&self, shared: &Shared, method: &str, elapsed: Duration) {
        if !debug_logs() {
            return;
        }
        let Ok(mut s) = self.stats.lock() else {
            return;
        };
        let us = elapsed.as_micros() as u64;
        match method {
            "blockchain.scripthash.get_history" | "blockchain.scriptpubkey.get_history" => {
                s.window_requests = s.window_requests.saturating_add(1);
                s.history_requests += 1;
                s.history_us_total += us;
                s.history_us_max = s.history_us_max.max(us);
                if s.first_history_at.is_none() {
                    s.first_history_at = Some(Instant::now());
                }
            }
            "blockchain.transaction.get" => {
                s.window_requests = s.window_requests.saturating_add(1);
                if s.first_tx_after_history.is_none() {
                    if let Some(at) = s.first_history_at {
                        let ms = at.elapsed().as_millis() as u64;
                        s.first_tx_after_history = Some(ms);
                        eprintln!(
                            "[e-lite] debug: wallet: first tx.get {ms} ms after the first \
                             get_history (history scan -> loading transactions)"
                        );
                    }
                }
                s.tx_gets += 1;
                s.tx_us_total += us;
                s.tx_us_max = s.tx_us_max.max(us);
            }
            _ => return,
        }
        if s.last_summary.elapsed() >= Duration::from_secs(60) && s.window_requests >= 3 {
            let avg = |total: u64, n: u64| total.checked_div(n).unwrap_or(0);
            let delta = s.window_base.since(shared);
            eprintln!(
                "[e-lite] debug: wallet session: {}s, history {} ({} avg, {} max), \
                 tx.get {} ({} avg, {} max), batches {} ({} items, max {}), cache hits {} \
                 puts {} fetch {}, tx.get failed {}",
                s.opened.elapsed().as_secs(),
                s.history_requests,
                fmt_latency(avg(s.history_us_total, s.history_requests)),
                fmt_latency(s.history_us_max),
                s.tx_gets,
                fmt_latency(avg(s.tx_us_total, s.tx_gets)),
                fmt_latency(s.tx_us_max),
                s.batch_lines,
                s.batch_items,
                s.batch_max_items,
                delta.hits,
                delta.puts,
                delta.fetches,
                delta.failures_text(),
            );
            s.last_summary = Instant::now();
            s.window_requests = 0;
            s.window_base = CacheBaseline::capture(shared);
        }
    }

    /// One line at close with the whole session: counts, batches and the
    /// cache delta since the connection opened (cold vs warm evidence).
    pub(crate) fn log_close(&self, shared: &Shared) {
        if !debug_logs() {
            return;
        }
        let Ok(s) = self.stats.lock() else {
            return;
        };
        let delta = s.session_base.since(shared);
        eprintln!(
            "[e-lite] debug: wallet: closed after {}s (history {}, tx.get {}; batches {} \
             ({} items, max {}); cache hits {} puts {} fetch {}; tx.get failed {})",
            s.opened.elapsed().as_secs(),
            s.history_requests,
            s.tx_gets,
            s.batch_lines,
            s.batch_items,
            s.batch_max_items,
            delta.hits,
            delta.puts,
            delta.fetches,
            delta.failures_text(),
        );
    }

    /// Push a notification. It is queued and written by whoever holds the
    /// connection lock, so the notifier thread (shared by every client)
    /// never waits behind one client's response or socket poll. The write
    /// itself gets a short timeout: a client that does not drain its socket
    /// is disconnected and the wallet reconnects and resubscribes (AUD-146).
    /// `true` means accepted for delivery.
    pub fn notify(&self, value: &Value) -> bool {
        if !self.alive.load(Ordering::Relaxed) {
            return false;
        }
        let mut bytes = match serde_json::to_vec(value) {
            Ok(b) => b,
            Err(_) => return false,
        };
        bytes.push(b'\n');
        {
            let Ok(mut pending) = self.pending_notes.lock() else {
                return false;
            };
            if pending.len() >= MAX_PENDING_NOTES {
                drop(pending);
                self.kill();
                return false;
            }
            pending.push_back(bytes);
        }
        self.flush_pending();
        self.alive.load(Ordering::Relaxed)
    }

    /// Deliver queued notifications if the connection lock is free. Every
    /// holder of the lock calls this after releasing it, so a notification
    /// queued while the lock was busy is never stranded.
    pub(crate) fn flush_pending(&self) {
        loop {
            let has_pending = self
                .pending_notes
                .lock()
                .map(|p| !p.is_empty())
                .unwrap_or(false);
            if !has_pending || !self.alive.load(Ordering::Relaxed) {
                return;
            }
            let Ok(mut writer) = self.writer.try_lock() else {
                // The holder flushes after it releases the lock.
                return;
            };
            let batch: Vec<Vec<u8>> = match self.pending_notes.lock() {
                Ok(mut p) => p.drain(..).collect(),
                Err(_) => return,
            };
            let _ = writer
                .socket()
                .set_write_timeout(Some(NOTIFY_WRITE_TIMEOUT));
            let ok = batch
                .iter()
                .try_for_each(|bytes| writer.write_all(bytes))
                .and_then(|_| writer.flush())
                .is_ok();
            let _ = writer
                .socket()
                .set_write_timeout(Some(CLIENT_WRITE_TIMEOUT));
            if !ok {
                // A partial frame may be on the wire: the stream is unusable.
                self.alive.store(false, Ordering::SeqCst);
                let _ = writer.socket().shutdown(std::net::Shutdown::Both);
                return;
            }
        }
    }

    fn kill(&self) {
        self.alive.store(false, Ordering::SeqCst);
        if let Ok(writer) = self.writer.try_lock() {
            let _ = writer.socket().shutdown(std::net::Shutdown::Both);
        }
    }

    pub fn send(&self, value: &Value) -> bool {
        if !self.alive.load(Ordering::Relaxed) {
            return false;
        }
        let mut bytes = match serde_json::to_vec(value) {
            Ok(b) => b,
            Err(_) => return false,
        };
        bytes.push(b'\n');
        let mut writer = match self.writer.lock() {
            Ok(w) => w,
            Err(_) => return false,
        };
        if writer
            .write_all(&bytes)
            .and_then(|_| writer.flush())
            .is_err()
        {
            // A partial frame may be on the wire: the stream is unusable.
            // Close it so the peer and this connection's reader see it now.
            self.alive.store(false, Ordering::SeqCst);
            let _ = writer.socket().shutdown(std::net::Shutdown::Both);
            return false;
        }
        drop(writer);
        self.flush_pending();
        true
    }
}

#[derive(Default)]
pub struct ClientRegistry {
    next: AtomicU64,
    clients: Mutex<HashMap<u64, Arc<ClientState>>>,
    rate_limit_rps: AtomicU64,
    rate_burst: AtomicU64,
}

impl ClientRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Configure the per-connection budget. A zero rate disables it.
    pub fn set_rate_limit(&self, requests_per_sec: u64, burst: u64) {
        self.rate_limit_rps
            .store(requests_per_sec, Ordering::Relaxed);
        self.rate_burst.store(burst.max(1), Ordering::Relaxed);
    }

    pub fn rate_limit(&self) -> (u64, u64) {
        (
            self.rate_limit_rps.load(Ordering::Relaxed),
            self.rate_burst.load(Ordering::Relaxed),
        )
    }

    pub fn register(&self, wire: Wire) -> Arc<ClientState> {
        let id = self.next.fetch_add(1, Ordering::SeqCst) + 1;
        let tls = wire.is_tls();
        let state = Arc::new(ClientState {
            id,
            writer: Mutex::new(wire),
            subs: Mutex::new(HashMap::new()),
            spk_subs: Mutex::new(HashMap::new()),
            outpoints: Mutex::new(HashMap::new()),
            budget: Mutex::new(RateLimit::new(
                self.rate_burst.load(Ordering::Relaxed),
                self.rate_limit_rps.load(Ordering::Relaxed),
            )),
            headers: AtomicBool::new(false),
            background: AtomicUsize::new(0),
            last_height: AtomicU64::new(0),
            alive: AtomicBool::new(true),
            detached: AtomicBool::new(false),
            tls,
            protocol: Mutex::new(None),
            saw_message: AtomicBool::new(false),
            pong_budget: AtomicU64::new(MAX_PONG_BYTES),
            stats: Mutex::new(ClientStats::empty()),
            pending_notes: Mutex::new(VecDeque::new()),
        });
        if let Ok(mut c) = self.clients.lock() {
            c.insert(id, Arc::clone(&state));
        }
        state
    }

    /// Drop the registry entry without marking the connection dead: lines
    /// still running may answer a client that only half-closed.
    pub fn detach(&self, id: u64) {
        if let Ok(mut c) = self.clients.lock() {
            c.remove(&id);
        }
    }

    pub fn unregister(&self, id: u64) {
        if let Ok(mut c) = self.clients.lock() {
            if let Some(state) = c.remove(&id) {
                state.alive.store(false, Ordering::SeqCst);
            }
        }
    }

    pub fn snapshot(&self) -> Vec<Arc<ClientState>> {
        self.clients
            .lock()
            .map(|c| c.values().cloned().collect())
            .unwrap_or_default()
    }

    pub fn len(&self) -> usize {
        self.clients.lock().map(|c| c.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
