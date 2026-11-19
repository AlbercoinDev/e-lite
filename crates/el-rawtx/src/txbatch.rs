//! Per-txid fetch mode (AUD-163): gather, mix with decoy wallets, spread over
//! servers, fetch in short sessions, verify against the index, cache.
//!
//! Requests come from the server as soon as the index knows them (the
//! history of a subscribed address) and from `transaction.get`. One thread
//! gathers them for `batch_window_ms`, adds the same number of transactions
//! from each decoy wallet of the connection, and spreads every group (the
//! real one and each decoy wallet) over at least `min_servers` servers, so no
//! server receives more than `max_share` of any group. Each server's share is
//! cut into sessions of `session_size` txids, run on separate Tor circuits,
//! `parallel_sessions` at a time. A real transaction is cached only after
//! its txid, its position in the indexed block and its merkle branch up to
//! the indexed header all check out; a server that fails that is penalised
//! and the transaction goes to another one. After three servers, the caller
//! falls back to the whole-block path.

use crate::decoy_wallets::{DecoyBook, DecoyPicker, TxRef};
use crate::governor::{Availability, GovernorPermit, Lane, TorGovernor};
use crate::privacy::PrivacyParams;
use crate::qualification::{Qualification, Route, Sample, TX_FAST};
use crate::txcache::{CachedTx, TxCache};
use crate::txsource::TxSource;
use crate::DecoySessions;
use crate::{
    AttemptCtl, Permit, RawtxError, RawtxStats, Result, Semaphore, MAX_HISTORY_TXID_PREFETCH,
};
use el_model::sync::Mutex;
use el_model::{Hash32, ScriptHash};
use el_store::Store;
use rand::seq::SliceRandom;
use rand::thread_rng;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Servers a real transaction is tried on before the block fallback.
const MAX_TX_ATTEMPTS: u32 = 3;
/// Budget of one session (connection, version, batch, answer). Measured over
/// Tor: a 50-tx batch takes 0.3-1.2 s and a full session 1-2 s, so 3 s is a
/// firm switch threshold: a provider that misses it is replaced instead of
/// holding the wallet's transactions.
const SESSION_BUDGET: Duration = Duration::from_secs(3);
const MAX_JOBS: usize = 4096;
const MAX_PENDING_HISTORIES: usize = 128;
const PREFETCH_TTL: Duration = Duration::from_secs(45);
/// Only for a source returning an unstarted attempt after admission. The
/// scheduler itself waits for resource events rather than retrying on a timer.
const SOURCE_BUSY_BACKOFF: Duration = Duration::from_millis(50);
/// Smallest batch tried after a provider rejects larger ones. Measured:
/// `electrum.blockstream.info` accepts 20 txids and closes the connection
/// above that. The cap halves per failure down to this value.
const MIN_BATCH: usize = 2;
/// Connections whose decoy book is kept (oldest dropped first).
const MAX_BOOKS: usize = 64;
/// Sessions open at once to one server. Measured over Tor on 2026-11-14:
/// 32 parallel sessions over 11 servers lost 87 of 480 transactions to
/// refused or dropped connections; 16 lost 2.
const MAX_SESSIONS_PER_SERVER: usize = 2;
/// Weight of a new sample in a server's latency average.
const LATENCY_ALPHA: f64 = 0.3;

#[derive(Clone)]
struct Req {
    ctx: u64,
    t: TxRef,
    block: Hash32,
    wait: Arc<Wait>,
    attempts: u32,
    tried: Vec<String>,
    ready: Instant,
    cover_needed: bool,
}

#[derive(Clone)]
enum Item {
    Real(Req),
    Decoy { ctx: u64, wallet: usize, t: TxRef },
}

impl Item {
    fn txid(&self) -> Hash32 {
        match self {
            Item::Real(r) => r.t.txid,
            Item::Decoy { t, .. } => t.txid,
        }
    }
}

#[derive(Default)]
struct Interest {
    active: usize,
    prefetch: Option<Instant>,
}

#[derive(Default)]
struct Progress {
    done: Option<Result<String>>,
    owners: HashMap<u64, Interest>,
}

struct Wait {
    state: Mutex<Progress>,
}

impl Wait {
    fn still_relevant_to(&self, ctx: u64, now: Instant) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .owners
            .get(&ctx)
            .is_some_and(|i| i.active > 0 || i.prefetch.is_some_and(|until| until > now))
    }

    fn interest(&self, now: Instant) -> Option<(u64, Lane)> {
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if st.done.is_some() {
            return None;
        }
        st.owners
            .retain(|_, i| i.active > 0 || i.prefetch.is_some_and(|end| end > now));
        st.owners
            .iter()
            .min_by_key(|(ctx, i)| (i.active == 0, **ctx))
            .map(|(ctx, i)| {
                (
                    *ctx,
                    if i.active > 0 {
                        Lane::Wallet
                    } else {
                        Lane::Prefetch
                    },
                )
            })
    }

    fn result(&self) -> Option<Result<String>> {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .done
            .clone()
    }
}

/// One wallet's interest in shared work, kept across the block fallback.
pub(crate) struct Ticket {
    wait: Arc<Wait>,
    event: Arc<Availability>,
    ctx: u64,
}

impl Ticket {
    pub(crate) fn result(&self) -> Option<Result<String>> {
        self.wait.result()
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let mut st = self.wait.state.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(interest) = st.owners.get_mut(&self.ctx) {
            interest.active -= 1;
            if interest.active == 0 {
                // Do not keep speculative work for a request the wallet has
                // completed or abandoned.
                st.owners.remove(&self.ctx);
            }
        }
        drop(st);
        self.event.notify();
    }
}

#[derive(Default)]
struct Incoming {
    reqs: Vec<Req>,
    histories: VecDeque<(u64, ScriptHash, Vec<TxRef>)>,
    unsent: Vec<Session>,
}

#[derive(Default)]
struct Books {
    open: HashMap<u64, Arc<Mutex<DecoyBook>>>,
    order: VecDeque<u64>,
    closed: VecDeque<u64>,
}

struct Shared {
    store: Arc<Store>,
    txcache: Option<Arc<TxCache>>,
    sources: Vec<Arc<dyn TxSource>>,
    qualification: Arc<Qualification>,
    params: PrivacyParams,
    picker: DecoyPicker,
    books: Mutex<Books>,
    queue: Mutex<Incoming>,
    event: Arc<Availability>,
    waiters: Mutex<HashMap<(TxRef, Hash32), Arc<Wait>>>,
    prune_height: Arc<AtomicU32>,
    sessions: Arc<Semaphore>,
    /// Sessions open to a server (at most `MAX_SESSIONS_PER_SERVER`).
    per_server: Mutex<HashMap<String, Arc<Semaphore>>>,
    /// Decoy sessions get their own small pool so three decoy wallets never
    /// take the slots the real wallet is waiting for.
    decoy_sessions: Arc<Semaphore>,
    /// Same round as the real wallet, or mostly background.
    decoy_mode: DecoySessions,
    /// Random delay before a session, milliseconds.
    jitter_ms: u64,
    /// Shared Tor stream governor: a session holds one stream.
    governor: Arc<TorGovernor>,
    /// Long-term per-provider quota (multiple of the fair share; 0 = off).
    provider_quota: f64,
    /// Items sent per operator since start and the total, for the quota.
    provider_counts: Mutex<HashMap<String, u64>>,
    provider_total: AtomicU64,
    /// Longest batch a server accepted, learned after a session failure: a
    /// provider that closes connections above 20 items (measured) is not asked
    /// for 50 again in the same process.
    batch_caps: Mutex<HashMap<String, usize>>,
    /// Average session time per server, ms (failures count as slow).
    latency: Mutex<HashMap<String, f64>>,
    /// Items of each group sent to each server during a connection's load,
    /// keyed by (connection, group): keeps `max_share` across batches.
    spread: Mutex<HashMap<(u64, usize), HashMap<String, usize>>>,
    stats: Arc<RawtxStats>,
    closed: AtomicBool,
    /// Batches being dispatched and sessions running right now (tests).
    active: std::sync::atomic::AtomicUsize,
    sessions_active: std::sync::atomic::AtomicUsize,
    pending: std::sync::atomic::AtomicUsize,
}

/// The per-txid fetch mode of a resolver.
pub(crate) struct TxMode {
    shared: Arc<Shared>,
    /// The batcher thread, joined on drop: a thread still inside the index or
    /// RocksDB when the process exits aborts it (`pthread lock: Invalid
    /// argument`). The loop sees `closed` and returns, so the join is short.
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Drop for TxMode {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::Relaxed);
        self.shared.event.notify();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl TxMode {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        store: Arc<Store>,
        txcache: Option<Arc<TxCache>>,
        sources: Vec<Arc<dyn TxSource>>,
        params: PrivacyParams,
        secret: [u8; 32],
        stats: Arc<RawtxStats>,
        sessions_total: usize,
        decoy_mode: DecoySessions,
        jitter_ms: u64,
        provider_quota: f64,
        governor: Arc<TorGovernor>,
        prune_height: Arc<AtomicU32>,
        event: Arc<Availability>,
        qualification: Arc<Qualification>,
    ) -> Option<Self> {
        if sources.is_empty() {
            return None;
        }
        let shared = Arc::new(Shared {
            picker: DecoyPicker::new(
                Arc::clone(&store),
                secret,
                params.clone(),
                Arc::clone(&prune_height),
            ),
            store,
            txcache,
            sources,
            qualification,
            sessions: Semaphore::new(sessions_total.min(params.parallel_sessions).max(1)),
            decoy_sessions: Semaphore::new(
                (sessions_total.min(params.parallel_sessions) / 4).max(1),
            ),
            decoy_mode,
            jitter_ms: jitter_ms.min(250),
            provider_quota: if provider_quota <= 0.0 {
                0.0
            } else {
                provider_quota.clamp(1.0, 4.0)
            },
            provider_counts: Mutex::new(HashMap::new()),
            provider_total: AtomicU64::new(0),
            governor,
            batch_caps: Mutex::new(HashMap::new()),
            params,
            books: Mutex::new(Books::default()),
            queue: Mutex::new(Incoming::default()),
            event,
            waiters: Mutex::new(HashMap::new()),
            prune_height,
            per_server: Mutex::new(HashMap::new()),
            latency: Mutex::new(HashMap::new()),
            spread: Mutex::new(HashMap::new()),
            stats,
            closed: AtomicBool::new(false),
            active: std::sync::atomic::AtomicUsize::new(0),
            sessions_active: std::sync::atomic::AtomicUsize::new(0),
            pending: std::sync::atomic::AtomicUsize::new(0),
        });
        let worker = Arc::clone(&shared);
        let handle = match std::thread::Builder::new()
            .name("el-txbatch".into())
            .spawn(move || batch_loop(worker))
        {
            Ok(handle) => handle,
            Err(e) => {
                eprintln!("[e-lite] tx batcher did not start: {e}");
                return None;
            }
        };
        Some(TxMode {
            shared,
            worker: Some(handle),
        })
    }

    /// Servers usable now, over every source.
    pub(crate) fn servers(&self) -> usize {
        self.shared.sources.iter().map(|s| s.servers().len()).sum()
    }

    pub(crate) fn probe_sources(&self) -> Vec<Arc<dyn TxSource>> {
        self.shared
            .sources
            .iter()
            .filter(|s| s.requires_qualification())
            .cloned()
            .collect()
    }

    pub(crate) fn routes_ready(&self) -> bool {
        self.shared
            .sufficient_diversity(&self.shared.eligible_servers())
    }

    pub(crate) fn route_operator_counts(&self) -> (usize, usize) {
        (
            operator_count(&self.shared.eligible_servers()),
            self.shared.operator_minimum(),
        )
    }

    pub(crate) fn provider_counts(&self) -> HashMap<String, u64> {
        self.shared
            .provider_counts
            .lock()
            .map(|counts| counts.clone())
            .unwrap_or_default()
    }

    pub(crate) fn wake(&self) {
        self.shared.event.notify();
    }

    /// The index served the history of `script` to connection `ctx`: queue
    /// every transaction not cached yet, and give the connection's decoy
    /// wallets a look-alike of the address. Returns how many were queued.
    pub(crate) fn want_history(&self, ctx: u64, script: ScriptHash, hist: &[TxRef]) -> usize {
        let mut q = self.shared.queue.lock().unwrap_or_else(|p| p.into_inner());
        if hist.is_empty()
            || hist.len() > MAX_HISTORY_TXID_PREFETCH
            || q.histories.len() >= MAX_PENDING_HISTORIES
        {
            return 0;
        }
        let n = hist.len();
        q.histories.push_back((ctx, script, hist.to_vec()));
        drop(q);
        self.shared.event.notify();
        n
    }

    pub(crate) fn request(&self, ctx: u64, t: TxRef, block: Hash32) -> Option<Ticket> {
        let wait = self.shared.submit(ctx, t, block, true)?;
        Some(Ticket {
            wait,
            event: Arc::clone(&self.shared.event),
            ctx,
        })
    }

    pub(crate) fn publish(&self, t: TxRef, block: Hash32, raw: &str) {
        let key = (t, block);
        let wait = self
            .shared
            .waiters
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&key)
            .cloned();
        if let Some(wait) = wait {
            self.shared.finish(key, &wait, Ok(raw.to_owned()));
        }
    }

    /// True when no batch, session or queued request remains. Tests wait on it
    /// before dropping the store; `/status` can report it too.
    pub(crate) fn idle(&self) -> bool {
        let queue_empty = self
            .shared
            .queue
            .lock()
            .map(|q| q.reqs.is_empty() && q.histories.is_empty() && q.unsent.is_empty())
            .unwrap_or(false);
        let waiters_empty = self
            .shared
            .waiters
            .lock()
            .map(|w| w.is_empty())
            .unwrap_or(false);
        queue_empty
            && waiters_empty
            && self.shared.active.load(Ordering::SeqCst) == 0
            && self.shared.sessions_active.load(Ordering::SeqCst) == 0
            && self.shared.pending.load(Ordering::SeqCst) == 0
    }

    /// The connection closed: forget its decoy wallets.
    pub(crate) fn forget(&self, ctx: u64) {
        self.shared
            .queue
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .histories
            .retain(|(id, _, _)| *id != ctx);
        let waiters = self
            .shared
            .waiters
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        {
            let mut books = self.shared.books.lock().unwrap_or_else(|p| p.into_inner());
            books.open.remove(&ctx);
            books.order.retain(|c| *c != ctx);
            books.closed.push_back(ctx);
            if books.closed.len() > MAX_BOOKS + MAX_PENDING_HISTORIES {
                books.closed.pop_front();
            }
        }
        for wait in waiters.values() {
            wait.state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .owners
                .remove(&ctx);
        }
        drop(waiters);
        if let Ok(mut spread) = self.shared.spread.lock() {
            spread.retain(|(c, _), _| *c != ctx);
        }
        self.shared.event.notify();
    }
}

impl Shared {
    fn cached(&self, t: TxRef, expected: Option<Hash32>) -> Option<String> {
        let entry = self.txcache.as_ref()?.get(&t.txid)?;
        let rec = self.store.get_block(t.height).ok()??;
        if expected.is_some_and(|hash| hash != rec.hash) {
            return None;
        }
        crate::verify_cached_tx(&entry, &rec, t.txid).ok()?;
        Some(hex::encode(entry.raw))
    }

    fn has_book(&self, ctx: u64) -> bool {
        self.books
            .lock()
            .is_ok_and(|books| books.open.contains_key(&ctx))
    }

    fn defer_decoys(&self, source: usize, key: &str, queued: Instant, items: Vec<Item>) {
        if items.is_empty() || queued.elapsed() >= PREFETCH_TTL {
            return;
        }
        let mut incoming = self.queue.lock().unwrap_or_else(|p| p.into_inner());
        incoming.unsent.push(Session {
            source,
            key: key.to_owned(),
            items,
            queued,
            ready: Instant::now() + SOURCE_BUSY_BACKOFF,
        });
        drop(incoming);
        self.event.notify();
    }

    fn with_book<T>(
        &self,
        ctx: u64,
        f: impl FnOnce(&mut DecoyBook, &DecoyPicker) -> T,
    ) -> Option<T> {
        let book = {
            let mut books = self.books.lock().unwrap_or_else(|p| p.into_inner());
            if books.closed.contains(&ctx) {
                return None;
            }
            if let Some(book) = books.open.get(&ctx) {
                Arc::clone(book)
            } else {
                if books.order.len() >= MAX_BOOKS {
                    if let Some(old) = books.order.pop_front() {
                        books.open.remove(&old);
                        if let Ok(mut spread) = self.spread.lock() {
                            spread.retain(|(c, _), _| *c != old);
                        }
                    }
                }
                let book = Arc::new(Mutex::new(DecoyBook::new(self.params.decoy_wallets)));
                books.open.insert(ctx, Arc::clone(&book));
                books.order.push_back(ctx);
                book
            }
        };
        let mut book = book.lock().unwrap_or_else(|p| p.into_inner());
        Some(f(&mut book, &self.picker))
    }

    /// Queue `t` once (a second request shares the first one's wait).
    fn submit(&self, ctx: u64, t: TxRef, block: Hash32, from_tx_get: bool) -> Option<Arc<Wait>> {
        let now = Instant::now();
        if t.height >= self.prune_height.load(Ordering::Relaxed) {
            return None;
        }
        let mut waiters = self.waiters.lock().unwrap_or_else(|p| p.into_inner());
        if self
            .books
            .lock()
            .is_ok_and(|books| books.closed.contains(&ctx))
        {
            return None;
        }
        let key = (t, block);
        let (wait, new) = if let Some(wait) = waiters.get(&key) {
            (Arc::clone(wait), false)
        } else {
            let max = if from_tx_get {
                MAX_JOBS
            } else {
                MAX_JOBS - MAX_HISTORY_TXID_PREFETCH
            };
            if waiters.len() >= max {
                return None;
            }
            let wait = Arc::new(Wait {
                state: Mutex::new(Progress::default()),
            });
            waiters.insert(key, Arc::clone(&wait));
            (wait, true)
        };
        {
            let mut st = wait.state.lock().unwrap_or_else(|p| p.into_inner());
            let interest = st.owners.entry(ctx).or_default();
            if from_tx_get {
                interest.active += 1;
            } else {
                interest.prefetch = Some(now + PREFETCH_TTL);
            }
        }
        drop(waiters);
        if new {
            self.requeue(Req {
                ctx,
                t,
                block,
                wait: Arc::clone(&wait),
                attempts: 0,
                tried: Vec::new(),
                ready: now,
                cover_needed: from_tx_get,
            });
        } else {
            self.event.notify();
        }
        Some(wait)
    }

    fn finish(&self, key: (TxRef, Hash32), wait: &Arc<Wait>, result: Result<String>) {
        let mut map = self.waiters.lock().unwrap_or_else(|p| p.into_inner());
        if map.get(&key).is_some_and(|w| Arc::ptr_eq(w, wait)) {
            wait.state.lock().unwrap_or_else(|p| p.into_inner()).done = Some(result);
            map.remove(&key);
            drop(map);
            self.event.notify();
        }
    }

    fn wanted(&self, req: &Req, now: Instant) -> Option<(u64, Lane)> {
        if let Some(i) = req.wait.interest(now) {
            if req.t.height < self.prune_height.load(Ordering::Relaxed) {
                return Some(i);
            }
        }
        // Recheck interest under the map lock: a new wallet might have joined
        // since the first check.
        let mut map = self.waiters.lock().unwrap_or_else(|p| p.into_inner());
        let key = (req.t, req.block);
        if map.get(&key).is_some_and(|w| Arc::ptr_eq(w, &req.wait)) {
            if req.t.height < self.prune_height.load(Ordering::Relaxed) {
                if let Some(i) = req.wait.interest(now) {
                    return Some(i);
                }
            }
            req.wait
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .done = Some(Err(RawtxError::Unavailable(
                "transaction work no longer needed".into(),
            )));
            map.remove(&key);
            self.event.notify();
        }
        None
    }

    fn server_slot(&self, key: &str) -> Arc<Semaphore> {
        let mut m = self.per_server.lock().unwrap_or_else(|p| p.into_inner());
        Arc::clone(
            m.entry(key.to_string())
                .or_insert_with(|| Semaphore::new(MAX_SESSIONS_PER_SERVER)),
        )
    }

    fn note_latency(&self, key: &str, ms: f64) {
        let mut m = self.latency.lock().unwrap_or_else(|p| p.into_inner());
        let e = m.entry(key.to_string()).or_insert(ms);
        *e = *e * (1.0 - LATENCY_ALPHA) + ms * LATENCY_ALPHA;
    }

    /// Longest batch to ask this server for. Starts at `default`; a failed
    /// session halves it down to `MIN_BATCH` and the value is kept for the
    /// rest of the process (a provider that closes above 20 items is not
    /// retried with 50 on the next load).
    fn batch_cap(&self, key: &str, default: usize) -> usize {
        self.batch_caps
            .lock()
            .map(|m| m.get(key).copied().unwrap_or(default))
            .unwrap_or(default)
    }

    /// A session with `items` transactions failed: remember a smaller batch.
    fn note_batch_failure(&self, key: &str, items: usize) {
        if items <= MIN_BATCH {
            return;
        }
        let smaller = (items / 2).max(MIN_BATCH);
        if let Ok(mut m) = self.batch_caps.lock() {
            let entry = m.entry(key.to_string()).or_insert(items);
            if smaller < *entry {
                *entry = smaller;
            }
        }
    }

    /// Expected session time of a server; unknown servers rank as average.
    fn expected_ms(&self, source: usize, key: &str) -> f64 {
        self.latency
            .lock()
            .ok()
            .and_then(|m| m.get(key).copied())
            .or_else(|| {
                self.qualification
                    .probe_ms(&Route::tx(self.sources[source].as_ref(), key))
            })
            .unwrap_or(5_000.0)
    }

    fn requeue(&self, req: Req) {
        let mut q = self.queue.lock().unwrap_or_else(|p| p.into_inner());
        q.reqs.push(req);
        drop(q);
        self.event.notify();
    }

    /// Check a loose transaction against the index and cache it (AUD-163).
    fn validate(&self, t: &TxRef, expected: Option<Hash32>, raw: &[u8]) -> Result<Vec<Hash32>> {
        let tx: bitcoin::Transaction = bitcoin::consensus::deserialize(raw)
            .map_err(|e| RawtxError::Verify(format!("cannot decode: {e}")))?;
        let computed = Hash32::from_hex(&tx.compute_txid().to_string())
            .map_err(|e| RawtxError::Verify(format!("bad txid: {e}")))?;
        if computed != t.txid {
            return Err(RawtxError::Verify("txid mismatch".into()));
        }
        let rec = self
            .store
            .get_block(t.height)?
            .ok_or_else(|| RawtxError::Unavailable("indexed location changed".into()))?;
        if expected.is_some_and(|hash| rec.hash != hash) {
            return Err(RawtxError::Unavailable("indexed location changed".into()));
        }
        let txids = self.store.block_txids(&rec)?;
        if txids.get(t.pos as usize) != Some(&t.txid) {
            return Err(RawtxError::Unavailable(
                "position does not match the index".into(),
            ));
        }
        let (_, branch) = el_model::merkle_branch(&txids, t.pos as usize)
            .ok_or_else(|| RawtxError::Verify("no merkle branch".into()))?;
        let header: bitcoin::block::Header = bitcoin::consensus::deserialize(&rec.header)
            .map_err(|e| RawtxError::Verify(format!("bad indexed header: {e}")))?;
        let root = el_model::merkle_root_from_branch(t.txid, &branch, t.pos as usize);
        if root.to_hex() != header.merkle_root.to_string() {
            return Err(RawtxError::Unavailable("index Merkle root changed".into()));
        }
        Ok(branch)
    }

    fn verify(&self, t: &TxRef, block: Hash32, raw: &[u8]) -> Result<String> {
        let branch = self.validate(t, Some(block), raw)?;
        if let Some(cache) = &self.txcache {
            let entry = CachedTx {
                height: t.height,
                tx_pos: t.pos,
                branch,
                raw: raw.to_vec(),
            };
            if cache.put(&t.txid, &entry).is_ok() {
                self.stats.cache_puts.fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(hex::encode(raw))
    }
}

struct Session {
    source: usize,
    key: String,
    items: Vec<Item>,
    queued: Instant,
    ready: Instant,
}

impl Session {
    fn lane(&self, now: Instant) -> Lane {
        self.items
            .iter()
            .filter_map(|item| match item {
                Item::Real(r) => r.wait.interest(now).map(|(_, lane)| lane),
                Item::Decoy { .. } => None,
            })
            .min()
            .unwrap_or(Lane::Decoy)
    }
}

fn batch_loop(shared: Arc<Shared>) {
    let mut staged = VecDeque::<Session>::new();
    let mut preparing = Vec::<Req>::new();
    let mut workers = Vec::<std::thread::JoinHandle<()>>::new();
    let mut gather_until = None;
    let mut reassign = Vec::new();
    while !shared.closed.load(Ordering::Relaxed) {
        let seq = shared.event.sequence();
        shared.active.fetch_add(1, Ordering::SeqCst);
        let (requests, unsent, history, more_history) = {
            let mut incoming = shared.queue.lock().unwrap_or_else(|p| p.into_inner());
            (
                std::mem::take(&mut incoming.reqs),
                std::mem::take(&mut incoming.unsent),
                incoming.histories.pop_front(),
                !incoming.histories.is_empty(),
            )
        };
        staged.extend(unsent);
        if let Some((ctx, script, hist)) = history {
            let missing: Vec<_> = hist
                .iter()
                .copied()
                .filter(|&t| shared.cached(t, None).is_none())
                .collect();
            if !missing.is_empty() {
                shared.with_book(ctx, |book, picker| book.add_real(picker, script, &hist));
                for t in missing {
                    if let Ok(Some(rec)) = shared.store.get_block(t.height) {
                        shared.submit(ctx, t, rec.hash, false);
                    }
                }
            }
        }
        preparing.extend(requests);
        let now = Instant::now();
        let mut next = None;
        for session in std::mem::take(&mut reassign) {
            reassign_session(&shared, session, &mut staged);
        }
        let routes_ready = shared.sufficient_diversity(&shared.eligible_servers());
        if !routes_ready && shared.sources.iter().any(|s| s.requires_qualification()) {
            preparing.retain(|r| shared.wanted(r, now).is_some());
            gather_until = None;
        } else if !preparing.is_empty() && staged.len() < MAX_JOBS {
            let gather = *gather_until
                .get_or_insert(now + Duration::from_millis(shared.params.batch_window_ms));
            if now >= gather {
                let mut ready = Vec::new();
                preparing.retain_mut(|r| {
                    if shared.wanted(r, now).is_none() {
                        return false;
                    }
                    if r.ready > now {
                        next = Some(next.map_or(r.ready, |until: Instant| until.min(r.ready)));
                        return true;
                    }
                    if let Some(raw) = shared.cached(r.t, Some(r.block)) {
                        shared.finish((r.t, r.block), &r.wait, Ok(raw));
                    } else {
                        ready.push(r.clone());
                    }
                    false
                });
                if !ready.is_empty() {
                    dispatch(&shared, ready, &mut staged);
                }
                if preparing.is_empty() {
                    gather_until = None;
                }
            } else {
                next = Some(gather);
            }
        }
        // Recompute lane when a wallet joins its history prefetch. Never
        // create a worker to wait for a server, another session or Tor.
        staged.make_contiguous().sort_by_cached_key(|s| s.lane(now));
        let n = staged.len();
        for _ in 0..n {
            let Some(mut session) = staged.pop_front() else {
                break;
            };
            if shared.closed.load(Ordering::Relaxed) {
                break;
            }
            session.items.retain(|item| match item {
                Item::Real(r) => shared.wanted(r, now).is_some(),
                Item::Decoy { ctx, .. } => {
                    shared.has_book(*ctx) && now < session.queued + PREFETCH_TTL
                }
            });
            if session.items.is_empty() {
                continue;
            }
            if session.ready > now {
                next = Some(next.map_or(session.ready, |until| until.min(session.ready)));
                staged.push_back(session);
                continue;
            }
            if !routes_ready && shared.sources[session.source].requires_qualification() {
                staged.push_back(session);
                continue;
            }
            if shared.session_generation(&session).is_none() {
                shared.qualification.skipped();
                reassign.push(session);
                continue;
            }
            let lane = session.lane(now);
            match admit(&shared, &session, lane) {
                Some(permits) => {
                    let scheduler = Arc::clone(&shared);
                    let backup = session.items.clone();
                    let guard = SessionGuard::new(&shared);
                    match std::thread::Builder::new()
                        .name(el_model::sync::THREAD_FETCH.into())
                        .spawn(move || {
                            let _guard = guard;
                            run_session(&scheduler, session, lane, permits);
                        }) {
                        Ok(worker) => workers.push(worker),
                        Err(e) => {
                            eprintln!("[e-lite] tx session did not start: {e}");
                            for item in backup {
                                if let Item::Real(r) = item {
                                    shared.finish(
                                        (r.t, r.block),
                                        &r.wait,
                                        Err(RawtxError::Unavailable(
                                            "could not start transaction session".into(),
                                        )),
                                    );
                                }
                            }
                        }
                    }
                }
                None => staged.push_back(session),
            }
        }
        let mut i = 0;
        while i < workers.len() {
            if workers[i].is_finished() {
                let _ = workers.swap_remove(i).join();
            } else {
                i += 1;
            }
        }
        if !staged.is_empty() {
            if let Some(until) = shared.governor.paused_until() {
                next = Some(next.map_or(until, |other| other.min(until)));
            }
            if let Some(until) = staged
                .iter()
                .map(|s| s.queued + PREFETCH_TTL)
                .filter(|&t| t > now)
                .min()
            {
                next = Some(next.map_or(until, |other| other.min(until)));
            }
        }
        shared.pending.store(
            staged.len() + preparing.len() + reassign.len(),
            Ordering::SeqCst,
        );
        shared.active.fetch_sub(1, Ordering::SeqCst);
        if !more_history && reassign.is_empty() {
            shared.event.wait(seq, next);
        }
    }
    for worker in workers {
        let _ = worker.join();
    }
}

/// Mix, spread and launch the sessions for one gathered batch.
fn dispatch(shared: &Shared, reqs: Vec<Req>, staged: &mut VecDeque<Session>) {
    let servers = shared.eligible_servers();
    if !shared.sufficient_diversity(&servers) {
        for r in reqs {
            shared.finish(
                (r.t, r.block),
                &r.wait,
                Err(RawtxError::Unavailable(
                    "tx mode: not enough qualified operators".into(),
                )),
            );
        }
        return;
    }
    // Groups: per connection, the real requests and each decoy wallet.
    let mut by_ctx: HashMap<u64, Vec<Req>> = HashMap::new();
    for mut r in reqs {
        let Some((ctx, _)) = shared.wanted(&r, Instant::now()) else {
            continue;
        };
        if r.cover_needed || r.ctx != ctx {
            shared.with_book(ctx, |book, picker| book.add_unknown(picker, &r.t));
            r.cover_needed = false;
            r.ctx = ctx;
        }
        by_ctx.entry(r.ctx).or_default().push(r);
    }
    // Groups keyed by (connection, 0 = real / 1 + decoy wallet).
    let mut groups: Vec<((u64, usize), Vec<Item>)> = Vec::new();
    for (ctx, real) in by_ctx {
        let n = real.len();
        // Background decoys keep only the first wallet while real requests
        // are waiting; the same round (default) keeps all of them, which is
        // what hides the real group among look-alikes.
        let background = matches!(shared.decoy_mode, DecoySessions::Background);
        let (decoys, k) = shared
            .with_book(ctx, |book, _| {
                let mut taken = book.take(n);
                if background {
                    taken.retain(|(wallet, _)| *wallet == 0);
                }
                (taken, book.wallets())
            })
            .unwrap_or_default();
        groups.push(((ctx, 0), real.into_iter().map(Item::Real).collect()));
        let mut per_wallet: Vec<Vec<Item>> = (0..k).map(|_| Vec::new()).collect();
        for (wallet, t) in decoys {
            if let Some(g) = per_wallet.get_mut(wallet) {
                g.push(Item::Decoy { ctx, wallet, t });
            }
        }
        groups.extend(
            per_wallet
                .into_iter()
                .enumerate()
                .filter(|(_, g)| !g.is_empty())
                .map(|(i, g)| ((ctx, i + 1), g)),
        );
    }

    assign(shared, servers, groups, Instant::now(), staged);
}

type Servers = Vec<(usize, String, bool)>;
type Groups = Vec<((u64, usize), Vec<Item>)>;

fn operator_count(servers: &Servers) -> usize {
    servers
        .iter()
        .map(|(_, key, _)| server_operator(key))
        .collect::<HashSet<_>>()
        .len()
}

impl Shared {
    fn session_generation(&self, session: &Session) -> Option<u64> {
        let src = self.sources[session.source].as_ref();
        if src.requires_qualification() {
            self.qualification.admit(&Route::tx(src, &session.key))
        } else {
            Some(0)
        }
    }

    fn eligible_servers(&self) -> Servers {
        let mut out = Vec::new();
        for (i, source) in self.sources.iter().enumerate() {
            let keys = if source.requires_qualification() {
                source.all_servers()
            } else {
                source.servers()
            };
            for key in keys {
                if !source.requires_qualification()
                    || self
                        .qualification
                        .admit(&Route::tx(source.as_ref(), &key))
                        .is_some()
                {
                    let onion = key.to_ascii_lowercase().contains(".onion");
                    out.push((i, key, onion));
                }
            }
        }
        out
    }

    fn sufficient_diversity(&self, servers: &Servers) -> bool {
        if servers.is_empty() {
            return false;
        }
        !self.sources.iter().any(|s| s.requires_qualification())
            || operator_count(servers) >= self.operator_minimum()
    }

    fn operator_minimum(&self) -> usize {
        self.params
            .min_servers
            .max((1.0 / self.params.max_share).ceil() as usize)
    }
}

fn reassign_session(shared: &Shared, session: Session, staged: &mut VecDeque<Session>) {
    let servers = shared.eligible_servers();
    let enough = shared.sufficient_diversity(&servers);
    let mut groups: HashMap<_, Vec<_>> = HashMap::new();
    let mut counts = shared.spread.lock().unwrap_or_else(|p| p.into_inner());
    let op = server_operator(&session.key);
    for item in session.items {
        let group = match &item {
            Item::Real(r) => (r.ctx, 0),
            Item::Decoy { ctx, wallet, .. } => (*ctx, *wallet + 1),
        };
        if let Some(n) = counts.get_mut(&group).and_then(|by_op| by_op.get_mut(&op)) {
            *n = n.saturating_sub(1);
        }
        if enough {
            groups.entry(group).or_default().push(item);
        } else if let Item::Real(r) = item {
            shared.finish(
                (r.t, r.block),
                &r.wait,
                Err(RawtxError::Unavailable(
                    "tx mode: not enough qualified operators".into(),
                )),
            );
        }
    }
    drop(counts);
    if enough {
        assign(
            shared,
            servers,
            groups.into_iter().collect(),
            session.queued,
            staged,
        );
    }
}

fn assign(
    shared: &Shared,
    servers: Servers,
    groups: Groups,
    queued: Instant,
    staged: &mut VecDeque<Session>,
) {
    let p = &shared.params;
    let spread = ((1.0 / p.max_share).ceil() as usize)
        .max(p.min_servers)
        .clamp(1, servers.len());
    // Spread each group over `spread` servers (onion first, then random),
    // round robin, skipping servers a real request already failed on.
    let mut per_server: HashMap<usize, Vec<Item>> = HashMap::new();
    let mut rng = thread_rng();
    // Faster servers first, with jitter so the choice is not deterministic;
    // the spread above still puts every group on several servers.
    let burst = groups
        .iter()
        .filter(|((_, wallet), _)| *wallet == 0)
        .map(|(_, items)| items.len())
        .sum::<usize>()
        >= 8;
    let rank: Vec<f64> = servers
        .iter()
        .map(|(source, key, onion)| {
            let jitter = 0.7 + rand::random::<f64>() * 0.6;
            let singles = if shared.sources[*source].max_batch_size() == 1 && burst {
                2.0
            } else {
                1.0
            };
            shared.expected_ms(*source, key) * jitter * singles * if *onion { 0.8 } else { 1.0 }
        })
        .collect();
    // Candidates: the faster half of the servers, never fewer than `spread`.
    // One endpoint per operator first (anti-collusion: mempool.space API,
    // onion and Electrum are the same operator), then the rest to fill.
    let mut order: Vec<usize> = (0..servers.len()).collect();
    order.shuffle(&mut rng);
    order.sort_by(|&a, &b| rank[a].total_cmp(&rank[b]));
    // Long-term quota: an operator that already received more than its share
    // of the traffic moves behind the others, so the load rotates instead of
    // concentrating on the two fastest providers.
    let operators: Vec<String> = servers
        .iter()
        .map(|(_, key, _)| server_operator(key))
        .collect();
    let (counts, total) = {
        let counts = shared
            .provider_counts
            .lock()
            .map(|m| m.clone())
            .unwrap_or_default();
        (counts, shared.provider_total.load(Ordering::Relaxed))
    };
    let quota_share = if shared.provider_quota > 0.0 && spread > 0 {
        shared.provider_quota / spread as f64
    } else {
        0.0
    };
    let order = quota_order(&order, &operators, &counts, total, quota_share);
    let target = spread.max(servers.len().div_ceil(2));
    let mut seen_operators: HashSet<String> = HashSet::new();
    let mut candidates: Vec<usize> = Vec::new();
    for &index in &order {
        if candidates.len() >= target {
            break;
        }
        if seen_operators.insert(server_operator(&servers[index].1)) {
            candidates.push(index);
        }
    }
    for &index in &order {
        if candidates.len() >= target {
            break;
        }
        if !candidates.contains(&index) {
            candidates.push(index);
        }
    }
    let mut spread_counts = shared.spread.lock().unwrap_or_else(|p| p.into_inner());
    for (group_key, group) in groups {
        // Each item goes to the candidate that has received the fewest items
        // of this group during the whole load (not just this batch), so the
        // share of any server stays near 1/spread across batches.
        let counts = spread_counts.entry(group_key).or_default();
        for item in group {
            let tried: &[String] = match &item {
                Item::Real(r) => &r.tried,
                Item::Decoy { .. } => &[],
            };
            let pick = candidates
                .iter()
                .copied()
                .filter(|&i| !tried.contains(&servers[i].1))
                .min_by(|&a, &b| {
                    let ca = counts.get(&operators[a]).copied().unwrap_or(0);
                    let cb = counts.get(&operators[b]).copied().unwrap_or(0);
                    ca.cmp(&cb).then(rank[a].total_cmp(&rank[b]))
                })
                .or_else(|| (0..servers.len()).find(|i| !tried.contains(&servers[*i].1)))
                .unwrap_or(candidates[0]);
            *counts.entry(operators[pick].clone()).or_insert(0) += 1;
            per_server.entry(pick).or_default().push(item);
        }
    }
    drop(spread_counts);

    let mut pending: Vec<(usize, Vec<Item>)> = per_server.into_iter().collect();
    // Real items first: their sessions must not queue behind decoy sessions
    // for the global session slots (three decoy wallets, one real).
    pending.sort_by_key(|(_, items)| !items.iter().any(|i| matches!(i, Item::Real(_))));
    for (idx, mut items) in pending {
        items.shuffle(&mut rng);
        let (src_idx, key, _) = servers[idx].clone();
        let cap = shared
            .batch_cap(&key, p.session_size.max(1))
            .min(shared.sources[src_idx].max_batch_size().max(1));
        for chunk in items.chunks(cap) {
            let now = Instant::now();
            let jitter = if shared.jitter_ms == 0 {
                0
            } else {
                rand::random::<u64>() % shared.jitter_ms
            };
            staged.push_back(Session {
                source: src_idx,
                key: key.clone(),
                items: chunk.to_vec(),
                queued,
                ready: now + Duration::from_millis(jitter),
            });
        }
    }
}

/// Decrements the session counter when `run_session` returns or unwinds.
struct SessionGuard(Arc<Shared>);

impl SessionGuard {
    fn new(shared: &Arc<Shared>) -> Self {
        shared.sessions_active.fetch_add(1, Ordering::SeqCst);
        Self(Arc::clone(shared))
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.0.sessions_active.fetch_sub(1, Ordering::SeqCst);
        self.0.event.notify();
    }
}

struct Admission {
    _session: Permit,
    _server: Permit,
    _decoy: Option<Permit>,
    stream: Option<GovernorPermit>,
}

fn admit(shared: &Shared, session: &Session, lane: Lane) -> Option<Admission> {
    let server = shared.server_slot(&session.key).try_acquire(0)?;
    let slot = shared
        .sessions
        .try_acquire(usize::from(lane == Lane::Decoy))?;
    let decoy = if lane == Lane::Decoy {
        Some(shared.decoy_sessions.try_acquire(0)?)
    } else {
        None
    };
    let stream = if shared.sources[session.source].via_tor(&session.key) {
        Some(shared.governor.try_acquire(lane)?)
    } else {
        None
    };
    Some(Admission {
        _session: slot,
        _server: server,
        _decoy: decoy,
        stream,
    })
}

fn run_session(shared: &Shared, session: Session, lane: Lane, mut permits: Admission) {
    // The route may have expired while the scheduler assigned the thread.
    if shared.session_generation(&session).is_none()
        || (shared.sources[session.source].requires_qualification()
            && !shared.sufficient_diversity(&shared.eligible_servers()))
    {
        shared.qualification.skipped();
        let mut queue = shared.queue.lock().unwrap_or_else(|p| p.into_inner());
        queue.unsent.push(session);
        return;
    }
    let Session {
        source,
        key,
        mut items,
        queued,
        ..
    } = session;
    let key = key.as_str();
    let src = &shared.sources[source];
    items.retain(|item| match item {
        Item::Real(r) => {
            if shared.wanted(r, Instant::now()).is_none() {
                false
            } else if let Some(raw) = shared.cached(r.t, Some(r.block)) {
                shared.finish((r.t, r.block), &r.wait, Ok(raw));
                false
            } else if shared
                .store
                .get_block(r.t.height)
                .ok()
                .flatten()
                .is_none_or(|rec| rec.hash != r.block)
            {
                shared.finish(
                    (r.t, r.block),
                    &r.wait,
                    Err(RawtxError::Unavailable("indexed location changed".into())),
                );
                false
            } else {
                true
            }
        }
        Item::Decoy { ctx, t, .. } => {
            shared.has_book(*ctx) && t.height < shared.prune_height.load(Ordering::Relaxed)
        }
    });
    if items.is_empty() {
        return;
    }
    // The original connection can leave while a second one is waiting for
    // the same transaction. Replan before sending if the original cover is
    // no longer associated with a live interest.
    items.retain(|item| match item {
        Item::Real(r) if !r.wait.still_relevant_to(r.ctx, Instant::now()) => {
            let mut r = r.clone();
            r.cover_needed = true;
            shared.requeue(r);
            false
        }
        _ => true,
    });
    if items.is_empty() {
        return;
    }
    let txids: Vec<Hash32> = items.iter().map(Item::txid).collect();
    let real = items.iter().filter(|i| matches!(i, Item::Real(_))).count() as u64;
    let route = Route::tx(src.as_ref(), key);
    let generation = if src.requires_qualification() {
        let Some(generation) = shared.qualification.admit(&route) else {
            shared.qualification.skipped();
            let mut queue = shared.queue.lock().unwrap_or_else(|p| p.into_inner());
            queue.unsent.push(Session {
                source,
                key: key.to_owned(),
                items,
                queued,
                ready: queued,
            });
            return;
        };
        generation
    } else {
        0
    };
    shared.stats.tx_sessions.fetch_add(1, Ordering::Relaxed);
    let ctl = AttemptCtl::new()
        .with_streams(Arc::clone(&shared.governor), lane, permits.stream.take())
        .with_health(lane == Lane::Wallet && !src.requires_qualification());
    let waited = queued.elapsed();
    // The session's budget starts once it holds everything it waited for.
    let started = Instant::now();
    let result = src.fetch_from(key, &txids, started + SESSION_BUDGET, &ctl);
    let network_time = started.elapsed();
    ctl.close_sockets();
    let ms = started.elapsed().as_millis() as f64;
    let skipped: HashSet<Hash32> = ctl.skipped().into_iter().collect();
    let local = matches!(&result, Err(RawtxError::Unavailable(m)) if m.contains(crate::LOCAL_BUSY));
    if crate::debug_logs() {
        el_model::debug_event!(
            "stream session src={} ep={key} lane={} items {} ({real} real), wait {} ms, {} ms, {}: {}",
            src.name(),
            lane.name(),
            txids.len(),
            waited.as_millis(),
            ms as u64,
            if ctl.connected() { "connected" } else { "no connection" },
            match &result {
                Ok(batch) => format!("ok, {} returned, {} not sent", batch.len(), skipped.len()),
                Err(e) => crate::redact_hashes(&e.to_string()),
            }
        );
    }
    if local {
        // Nothing was sent: not the server's failure, not a slow server.
        let mut decoys = Vec::new();
        for item in items {
            match item {
                Item::Real(r) => local_requeue(shared, r),
                cover => decoys.push(cover),
            }
        }
        shared.defer_decoys(source, key, queued, decoys);
        return;
    }
    if result.is_ok() && lane == Lane::Wallet && src.via_tor(key) {
        shared.governor.healthy();
    }
    if result.is_err() && ctl.connected() && lane != Lane::Decoy {
        // A provider that closes above its batch limit is remembered with a
        // smaller batch, so the next session succeeds instead of failing. A
        // session that never connected says nothing about the batch size
        // (29.25.2: a slow circuit used to halve it for the whole process).
        shared.note_batch_failure(key, items.len());
    }
    // Real time, success or failure: a fast rejection must not look like a
    // 40 s stall (the old code used the whole session budget on failure).
    if lane == Lane::Wallet && !src.requires_qualification() {
        shared.note_latency(key, ms);
    }
    let failed = result.is_err();
    let batch = result.unwrap_or_default();
    let mut bad = false;
    let mut unsent = Vec::new();
    let mut sent = 0;
    let mut useful = 0;
    for item in items {
        if !skipped.contains(&item.txid()) || batch.contains_key(&item.txid()) {
            sent += 1;
            let count = if matches!(item, Item::Real(_)) {
                &shared.stats.tx_real
            } else {
                &shared.stats.tx_decoys
            };
            count.fetch_add(1, Ordering::Relaxed);
        }
        match item {
            Item::Real(r) if skipped.contains(&r.t.txid) && !batch.contains_key(&r.t.txid) => {
                local_requeue(shared, r);
            }
            Item::Real(r) if shared.wanted(&r, Instant::now()).is_some() => {
                match batch.get(&r.t.txid) {
                    Some(raw) => match shared.verify(&r.t, r.block, raw) {
                        Ok(hex) => {
                            useful += 1;
                            shared.stats.tx_verified.fetch_add(1, Ordering::Relaxed);
                            shared.finish((r.t, r.block), &r.wait, Ok(hex));
                        }
                        Err(e) if matches!(e, RawtxError::Verify(_)) => {
                            bad = true;
                            shared
                                .stats
                                .tx_verify_failed
                                .fetch_add(1, Ordering::Relaxed);
                            shared.stats.verify_failures.fetch_add(1, Ordering::Relaxed);
                            retry_or_fail(shared, r, key, e.kind());
                        }
                        Err(e) => shared.finish((r.t, r.block), &r.wait, Err(e)),
                    },
                    None => retry_or_fail(shared, r, key, "not returned"),
                }
            }
            Item::Real(_) => {}
            Item::Decoy { ctx, wallet, t } => {
                if let Some(raw) = batch.get(&t.txid) {
                    if shared.params.decoy_growth
                        && shared.has_book(ctx)
                        && shared.validate(&t, None, raw).is_ok()
                    {
                        shared
                            .with_book(ctx, |book, picker| book.grow(picker, wallet, t.txid, raw));
                    }
                } else if skipped.contains(&t.txid) {
                    unsent.push(Item::Decoy { ctx, wallet, t });
                }
            }
        }
    }
    if sent > 0 {
        let operator = server_operator(key);
        if let Ok(mut totals) = shared.provider_counts.lock() {
            *totals.entry(operator).or_insert(0) += sent;
        }
        shared.provider_total.fetch_add(sent, Ordering::Relaxed);
    }
    shared.defer_decoys(source, key, queued, unsent);
    if src.requires_qualification() && lane != Lane::Decoy && !ctl.aborted() {
        let sample = if bad {
            Some(Sample::Invalid)
        } else if let Some(after) = ctl.retry_after() {
            Some(Sample::RateLimited(after))
        } else if failed && !ctl.non_health_failure() {
            Some(Sample::Failed)
        } else if useful > 0 {
            shared.note_latency(key, ms);
            Some(if network_time <= TX_FAST {
                Sample::Fast
            } else {
                Sample::Slow
            })
        } else {
            None
        };
        if let Some(sample) = sample {
            shared.qualification.observe(&route, generation, sample);
        }
    } else if bad && lane == Lane::Wallet {
        src.penalize(key);
    }
}

/// A request which never went on the wire spends no provider attempt.
fn local_requeue(shared: &Shared, mut r: Req) {
    shared
        .stats
        .tx_local_requeues
        .fetch_add(1, Ordering::Relaxed);
    if shared.closed.load(Ordering::Relaxed) {
        shared.finish(
            (r.t, r.block),
            &r.wait,
            Err(RawtxError::Unavailable("tx mode: closed".into())),
        );
    } else {
        r.ready = Instant::now() + SOURCE_BUSY_BACKOFF;
        shared.requeue(r);
    }
}

fn retry_or_fail(shared: &Shared, mut r: Req, key: &str, why: &str) {
    r.attempts += 1;
    r.tried.push(key.to_string());
    if r.attempts < MAX_TX_ATTEMPTS && !shared.closed.load(Ordering::Relaxed) {
        shared.requeue(r);
    } else {
        shared.stats.tx_fallback.fetch_add(1, Ordering::Relaxed);
        shared.finish(
            (r.t, r.block),
            &r.wait,
            Err(RawtxError::Unavailable(format!(
                "tx mode: {MAX_TX_ATTEMPTS} servers failed ({why})"
            ))),
        );
    }
}

/// Move operators that are over their long-term quota behind the others.
/// `quota_share <= 0` disables the rotation; with no traffic yet the order is
/// unchanged.
fn quota_order(
    order: &[usize],
    operators: &[String],
    counts: &HashMap<String, u64>,
    total: u64,
    quota_share: f64,
) -> Vec<usize> {
    if quota_share <= 0.0 || total == 0 {
        return order.to_vec();
    }
    let limit = quota_share * total as f64;
    let mut under = Vec::with_capacity(order.len());
    let mut over = Vec::new();
    for &index in order {
        let sent = counts
            .get(operators.get(index).map(String::as_str).unwrap_or(""))
            .copied()
            .unwrap_or(0) as f64;
        if sent < limit {
            under.push(index);
        } else {
            over.push(index);
        }
    }
    under.extend(over);
    under
}

/// Registrable-suffix approximation used to keep two endpoints of the same
/// operator out of the preferred candidate set (`mempool.space` API, onion
/// and Electrum count as one).
fn server_operator(key: &str) -> String {
    let host = key
        .split("//")
        .last()
        .unwrap_or(key)
        .split('/')
        .next()
        .unwrap_or(key)
        .split(':')
        .next()
        .unwrap_or(key)
        .trim_start_matches("www.")
        .to_ascii_lowercase();
    // An operator's onion alias does not count as another recipient. This
    // is identity metadata only: it never adds a configured route.
    if host == "mempoolhqx4isw62xs7abwphsq7ldayuidyx2v2oethdhhj6mlo2r6ad.onion" {
        return "mempool.space".into();
    }
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() >= 2 {
        format!("{}.{}", parts[parts.len() - 2], parts[parts.len() - 1])
    } else {
        host
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mempool_transports_share_the_same_operator() {
        let operator = server_operator("https://mempool.space/api");
        assert_eq!(operator, server_operator("electrum.mempool.space:50002:s"));
        assert_eq!(
            operator,
            server_operator(
                "http://mempoolhqx4isw62xs7abwphsq7ldayuidyx2v2oethdhhj6mlo2r6ad.onion/api"
            )
        );
    }

    #[test]
    fn over_quota_operators_move_to_the_back() {
        let operators = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let mut counts = HashMap::new();
        counts.insert("a".to_string(), 90);
        counts.insert("b".to_string(), 5);
        counts.insert("c".to_string(), 5);
        let order = vec![0, 1, 2];
        // Fair share 1/3, quota 1.5x: `a` is far above it.
        let rotated = quota_order(&order, &operators, &counts, 100, 1.5 / 3.0);
        assert_eq!(rotated, vec![1, 2, 0]);
        // A disabled quota keeps the latency order.
        assert_eq!(quota_order(&order, &operators, &counts, 100, 0.0), order);
        // No traffic yet: unchanged.
        assert_eq!(quota_order(&order, &operators, &counts, 0, 0.5), order);
    }

    #[test]
    fn traffic_rotates_between_operators() {
        let operators = vec!["a".to_string(), "b".to_string()];
        let mut counts: HashMap<String, u64> = HashMap::new();
        let mut first = 0usize;
        for _ in 0..20 {
            let out = quota_order(&[0, 1], &operators, &counts, 10, 0.75);
            first += usize::from(out[0] == 1);
            let chosen = operators[out[0]].clone();
            *counts.entry(chosen).or_insert(0) += 5;
        }
        // Neither operator is always first: the quota rotates the lead.
        assert!(first > 0, "the second operator must lead sometimes");
        assert!(first < 20, "the first operator must lead sometimes");
    }
}
