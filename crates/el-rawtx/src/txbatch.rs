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
use crate::privacy::PrivacyParams;
use crate::txcache::{CachedTx, TxCache};
use crate::txsource::TxSource;
use crate::DecoySessions;
use crate::{AttemptCtl, RawtxError, RawtxStats, Result, Semaphore};
use el_model::sync::Mutex;
use el_model::{Hash32, ScriptHash};
use el_store::Store;
use rand::seq::SliceRandom;
use rand::thread_rng;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar};
use std::time::{Duration, Instant};

/// Servers a real transaction is tried on before the block fallback.
const MAX_TX_ATTEMPTS: u32 = 3;
/// Budget of one session (connection, version, batch, answer). Measured over
/// Tor: a 50-tx batch takes 0.3-1.2 s and a full session 1-2 s, so 3 s is a
/// firm switch threshold: a provider that misses it is replaced instead of
/// holding the wallet's transactions.
const SESSION_BUDGET: Duration = Duration::from_secs(3);
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
    attempts: u32,
    tried: Vec<String>,
}

#[derive(Clone)]
enum Item {
    Real(Req),
    Decoy {
        ctx: u64,
        wallet: usize,
        txid: Hash32,
    },
}

impl Item {
    fn txid(&self) -> Hash32 {
        match self {
            Item::Real(r) => r.t.txid,
            Item::Decoy { txid, .. } => *txid,
        }
    }
}

struct Wait {
    done: Mutex<Option<std::result::Result<String, RawtxError>>>,
    cv: Condvar,
}

struct Shared {
    store: Arc<Store>,
    txcache: Option<Arc<TxCache>>,
    sources: Vec<Arc<dyn TxSource>>,
    params: PrivacyParams,
    picker: DecoyPicker,
    books: Mutex<(HashMap<u64, DecoyBook>, VecDeque<u64>)>,
    queue: Mutex<Vec<Req>>,
    queue_cv: Condvar,
    waiters: Mutex<HashMap<Hash32, Arc<Wait>>>,
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
        self.shared.queue_cv.notify_all();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl TxMode {
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
    ) -> Option<Self> {
        if sources.is_empty() {
            return None;
        }
        let shared = Arc::new(Shared {
            picker: DecoyPicker::new(Arc::clone(&store), secret, params.clone()),
            store,
            txcache,
            sources,
            sessions: Semaphore::new(sessions_total.max(1)),
            decoy_sessions: Semaphore::new((sessions_total / 4).max(1)),
            decoy_mode,
            jitter_ms: jitter_ms.min(250),
            provider_quota: if provider_quota <= 0.0 {
                0.0
            } else {
                provider_quota.clamp(1.0, 4.0)
            },
            provider_counts: Mutex::new(HashMap::new()),
            provider_total: AtomicU64::new(0),
            batch_caps: Mutex::new(HashMap::new()),
            params,
            books: Mutex::new((HashMap::new(), VecDeque::new())),
            queue: Mutex::new(Vec::new()),
            queue_cv: Condvar::new(),
            waiters: Mutex::new(HashMap::new()),
            per_server: Mutex::new(HashMap::new()),
            latency: Mutex::new(HashMap::new()),
            spread: Mutex::new(HashMap::new()),
            stats,
            closed: AtomicBool::new(false),
            active: std::sync::atomic::AtomicUsize::new(0),
            sessions_active: std::sync::atomic::AtomicUsize::new(0),
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

    /// The index served the history of `script` to connection `ctx`: queue
    /// every transaction not cached yet, and give the connection's decoy
    /// wallets a look-alike of the address. Returns how many were queued.
    pub(crate) fn want_history(&self, ctx: u64, script: ScriptHash, hist: Vec<TxRef>) -> usize {
        let missing: Vec<TxRef> = hist
            .iter()
            .copied()
            .filter(|t| !self.shared.cached(t.txid))
            .collect();
        if missing.is_empty() {
            return 0;
        }
        self.shared
            .with_book(ctx, |book, picker| book.add_real(picker, script, &hist));
        let n = missing.len();
        for t in missing {
            self.shared.submit(ctx, t, false);
        }
        n
    }

    /// Fetch one transaction (waiting up to `deadline`). `Err` means the
    /// caller should use the whole-block path.
    pub(crate) fn fetch(&self, ctx: u64, t: TxRef, deadline: Instant) -> Result<String> {
        let wait = self.shared.submit(ctx, t, true);
        let mut done = wait
            .done
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if let Some(result) = done.as_ref() {
                return result.clone();
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(RawtxError::Unavailable("tx mode: deadline reached".into()));
            }
            done = wait
                .cv
                .wait_timeout(done, left.min(Duration::from_secs(1)))
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
    }

    /// True when no batch, session or queued request remains. Tests wait on it
    /// before dropping the store; `/status` can report it too.
    pub(crate) fn idle(&self) -> bool {
        let queue_empty = self
            .shared
            .queue
            .lock()
            .map(|q| q.is_empty())
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
    }

    /// The connection closed: forget its decoy wallets.
    pub(crate) fn forget(&self, ctx: u64) {
        if let Ok(mut books) = self.shared.books.lock() {
            books.0.remove(&ctx);
            books.1.retain(|c| *c != ctx);
        }
        if let Ok(mut spread) = self.shared.spread.lock() {
            spread.retain(|(c, _), _| *c != ctx);
        }
    }
}

impl Shared {
    fn cached(&self, txid: Hash32) -> bool {
        self.txcache
            .as_ref()
            .is_some_and(|c| c.get(&txid).is_some())
    }

    fn with_book<T>(&self, ctx: u64, f: impl FnOnce(&mut DecoyBook, &DecoyPicker) -> T) -> T {
        let mut books = self.books.lock().unwrap_or_else(|p| p.into_inner());
        if !books.0.contains_key(&ctx) {
            if books.1.len() >= MAX_BOOKS {
                if let Some(old) = books.1.pop_front() {
                    books.0.remove(&old);
                    if let Ok(mut spread) = self.spread.lock() {
                        spread.retain(|(c, _), _| *c != old);
                    }
                }
            }
            books
                .0
                .insert(ctx, DecoyBook::new(self.params.decoy_wallets));
            books.1.push_back(ctx);
        }
        let book = books.0.get_mut(&ctx).expect("inserted above");
        f(book, &self.picker)
    }

    /// Queue `t` once (a second request shares the first one's wait).
    fn submit(&self, ctx: u64, t: TxRef, from_tx_get: bool) -> Arc<Wait> {
        let mut waiters = self.waiters.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(w) = waiters.get(&t.txid) {
            return Arc::clone(w);
        }
        let wait = Arc::new(Wait {
            done: Mutex::new(None),
            cv: Condvar::new(),
        });
        waiters.insert(t.txid, Arc::clone(&wait));
        drop(waiters);
        if from_tx_get {
            // A bare transaction.get: no address known, decoys of its shape.
            self.with_book(ctx, |book, picker| book.add_unknown(picker, &t));
        }
        let mut q = self.queue.lock().unwrap_or_else(|p| p.into_inner());
        q.push(Req {
            ctx,
            t,
            attempts: 0,
            tried: Vec::new(),
        });
        self.queue_cv.notify_all();
        wait
    }

    fn finish(&self, txid: Hash32, result: std::result::Result<String, RawtxError>) {
        let wait = self
            .waiters
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&txid);
        if let Some(w) = wait {
            *w.done.lock().unwrap_or_else(|p| p.into_inner()) = Some(result);
            w.cv.notify_all();
        }
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
    fn expected_ms(&self, key: &str) -> f64 {
        self.latency
            .lock()
            .ok()
            .and_then(|m| m.get(key).copied())
            .unwrap_or(5_000.0)
    }

    fn requeue(&self, req: Req) {
        let mut q = self.queue.lock().unwrap_or_else(|p| p.into_inner());
        q.push(req);
        self.queue_cv.notify_all();
    }

    /// Check a loose transaction against the index and cache it (AUD-163).
    fn verify(&self, t: &TxRef, raw: &[u8]) -> Result<String> {
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
            .ok_or_else(|| RawtxError::Verify("block not indexed".into()))?;
        let txids = self.store.block_txids(&rec)?;
        if txids.get(t.pos as usize) != Some(&t.txid) {
            return Err(RawtxError::Verify(
                "position does not match the index".into(),
            ));
        }
        let (_, branch) = el_model::merkle_branch(&txids, t.pos as usize)
            .ok_or_else(|| RawtxError::Verify("no merkle branch".into()))?;
        let header: bitcoin::block::Header = bitcoin::consensus::deserialize(&rec.header)
            .map_err(|e| RawtxError::Verify(format!("bad indexed header: {e}")))?;
        let root = el_model::merkle_root_from_branch(t.txid, &branch, t.pos as usize);
        if root.to_hex() != header.merkle_root.to_string() {
            return Err(RawtxError::Verify("merkle branch mismatch".into()));
        }
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

fn batch_loop(shared: Arc<Shared>) {
    loop {
        let reqs = {
            let mut q = shared.queue.lock().unwrap_or_else(|p| p.into_inner());
            while q.is_empty() {
                if shared.closed.load(Ordering::Relaxed) {
                    return;
                }
                q = shared
                    .queue_cv
                    .wait_timeout(q, Duration::from_secs(1))
                    .unwrap_or_else(|p| p.into_inner())
                    .0;
            }
            drop(q);
            // Gather what arrives within the window, then take it all.
            std::thread::sleep(Duration::from_millis(shared.params.batch_window_ms));
            std::mem::take(&mut *shared.queue.lock().unwrap_or_else(|p| p.into_inner()))
        };
        shared.active.fetch_add(1, Ordering::SeqCst);
        dispatch(&shared, reqs);
        shared.active.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Mix, spread and launch the sessions for one gathered batch.
fn dispatch(shared: &Arc<Shared>, reqs: Vec<Req>) {
    // Usable servers over every source: (source index, key, onion).
    let mut servers: Vec<(usize, String, bool)> = Vec::new();
    for (i, src) in shared.sources.iter().enumerate() {
        for key in src.servers() {
            let onion = key.to_ascii_lowercase().contains(".onion");
            servers.push((i, key, onion));
        }
    }
    if servers.is_empty() {
        // Everything is cooling down: use every server rather than failing.
        for (i, src) in shared.sources.iter().enumerate() {
            for key in src.all_servers() {
                let onion = key.to_ascii_lowercase().contains(".onion");
                servers.push((i, key, onion));
            }
        }
    }
    if servers.is_empty() {
        for r in reqs {
            shared.finish(
                r.t.txid,
                Err(RawtxError::Unavailable("tx mode: no server".into())),
            );
        }
        return;
    }
    let p = &shared.params;
    let spread = ((1.0 / p.max_share).ceil() as usize)
        .max(p.min_servers)
        .clamp(1, servers.len());

    // Groups: per connection, the real requests and each decoy wallet.
    let mut by_ctx: HashMap<u64, Vec<Req>> = HashMap::new();
    for r in reqs {
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
        let decoys = shared.with_book(ctx, |book, _| {
            let mut taken = book.take(n);
            if background {
                taken.retain(|(wallet, _)| *wallet == 0);
            }
            taken
        });
        let k = shared.with_book(ctx, |book, _| book.wallets());
        groups.push(((ctx, 0), real.into_iter().map(Item::Real).collect()));
        let mut per_wallet: Vec<Vec<Item>> = (0..k).map(|_| Vec::new()).collect();
        for (wallet, txid) in decoys {
            if let Some(g) = per_wallet.get_mut(wallet) {
                g.push(Item::Decoy { ctx, wallet, txid });
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

    // Spread each group over `spread` servers (onion first, then random),
    // round robin, skipping servers a real request already failed on.
    let mut per_server: HashMap<usize, Vec<Item>> = HashMap::new();
    let mut rng = thread_rng();
    // Faster servers first, with jitter so the choice is not deterministic;
    // the spread above still puts every group on several servers.
    let rank: Vec<f64> = servers
        .iter()
        .map(|(_, key, onion)| {
            let jitter = 0.7 + rand::random::<f64>() * 0.6;
            shared.expected_ms(key) * jitter * if *onion { 0.8 } else { 1.0 }
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
                    let ca = counts.get(&servers[a].1).copied().unwrap_or(0);
                    let cb = counts.get(&servers[b].1).copied().unwrap_or(0);
                    ca.cmp(&cb).then(rank[a].total_cmp(&rank[b]))
                })
                .or_else(|| (0..servers.len()).find(|i| !tried.contains(&servers[*i].1)))
                .unwrap_or(candidates[0]);
            *counts.entry(servers[pick].1.clone()).or_insert(0) += 1;
            per_server.entry(pick).or_default().push(item);
            let operator = server_operator(&servers[pick].1);
            if let Ok(mut totals) = shared.provider_counts.lock() {
                *totals.entry(operator).or_insert(0) += 1;
            }
            shared.provider_total.fetch_add(1, Ordering::Relaxed);
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
        let cap = shared.batch_cap(&key, p.session_size.max(1));
        for chunk in items.chunks(cap) {
            let src = Arc::clone(&shared.sources[src_idx]);
            let chunk = chunk.to_vec();
            let shared = Arc::clone(shared);
            let key = key.clone();
            let spawned = std::thread::Builder::new()
                .name("el-txsession".into())
                .spawn(move || run_session(&shared, src, &key, chunk));
            if let Err(e) = spawned {
                eprintln!("[e-lite] tx session did not start: {e}");
            }
        }
    }
}

/// Decrements the session counter when `run_session` returns or unwinds.
struct SessionGuard<'a>(&'a std::sync::atomic::AtomicUsize);

impl Drop for SessionGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn run_session(shared: &Arc<Shared>, src: Arc<dyn TxSource>, key: &str, items: Vec<Item>) {
    shared.sessions_active.fetch_add(1, Ordering::SeqCst);
    let _guard = SessionGuard(&shared.sessions_active);
    let has_real = items.iter().any(|item| matches!(item, Item::Real(_)));
    if has_real && shared.jitter_ms > 0 {
        // Small random delay: it breaks the timing correlation between the
        // wallet's connection and its first provider request at a negligible
        // cost (at most `jitter_ms`).
        let delay = rand::random::<u64>() % shared.jitter_ms.max(1);
        std::thread::sleep(Duration::from_millis(delay));
    }
    // Decoy sessions use a small separate pool so three decoy wallets never
    // take the session slots a real transaction is waiting for. Dropping a
    // decoy when the pool is full is harmless: it is background noise.
    let _decoy_permit = if has_real {
        None
    } else {
        match shared
            .decoy_sessions
            .acquire_until(Instant::now() + crate::EXTERNAL_QUEUE_MAX, 0)
        {
            Some(permit) => Some(permit),
            None => return,
        }
    };
    // The server's slot first: a session waiting for a busy server must not
    // hold one of the global slots.
    let slot = shared.server_slot(key);
    let Some(_server_permit) = slot.acquire_until(Instant::now() + crate::EXTERNAL_QUEUE_MAX, 0)
    else {
        for item in items {
            if let Item::Real(r) = item {
                retry_or_fail(shared, r, key, "server busy");
            }
        }
        return;
    };
    let Some(_permit) = shared
        .sessions
        .acquire_until(Instant::now() + crate::EXTERNAL_QUEUE_MAX, 0)
    else {
        for item in items {
            if let Item::Real(r) = item {
                retry_or_fail(shared, r, key, "no session slot");
            }
        }
        return;
    };
    let txids: Vec<Hash32> = items.iter().map(Item::txid).collect();
    let real = items.iter().filter(|i| matches!(i, Item::Real(_))).count() as u64;
    shared.stats.tx_sessions.fetch_add(1, Ordering::Relaxed);
    shared.stats.tx_real.fetch_add(real, Ordering::Relaxed);
    shared
        .stats
        .tx_decoys
        .fetch_add(txids.len() as u64 - real, Ordering::Relaxed);
    let ctl = AttemptCtl::new();
    let started = Instant::now();
    let result = src.fetch_from(key, &txids, started + SESSION_BUDGET, &ctl);
    // Real time, success or failure: a fast rejection must not look like a
    // 40 s stall (the old code used the whole session budget on failure).
    let ms = started.elapsed().as_millis() as f64;
    if result.is_err() {
        // A provider that closes above its batch limit is remembered with a
        // smaller batch, so the next session succeeds instead of failing.
        shared.note_batch_failure(key, items.len());
    }
    shared.note_latency(key, ms);
    let batch = result.unwrap_or_default();
    let mut bad = false;
    for item in items {
        match item {
            Item::Real(r) => match batch.get(&r.t.txid) {
                Some(raw) => match shared.verify(&r.t, raw) {
                    Ok(hex) => {
                        shared.stats.tx_verified.fetch_add(1, Ordering::Relaxed);
                        shared.finish(r.t.txid, Ok(hex));
                    }
                    Err(e) => {
                        bad = true;
                        shared
                            .stats
                            .tx_verify_failed
                            .fetch_add(1, Ordering::Relaxed);
                        shared.stats.verify_failures.fetch_add(1, Ordering::Relaxed);
                        retry_or_fail(shared, r, key, e.kind());
                    }
                },
                None => retry_or_fail(shared, r, key, "not returned"),
            },
            Item::Decoy { ctx, wallet, txid } => {
                if let Some(raw) = batch.get(&txid) {
                    if shared.params.decoy_growth {
                        shared.with_book(ctx, |book, picker| book.grow(picker, wallet, txid, raw));
                    }
                }
            }
        }
    }
    if bad {
        src.penalize(key);
    }
}

fn retry_or_fail(shared: &Arc<Shared>, mut r: Req, key: &str, why: &str) {
    r.attempts += 1;
    r.tried.push(key.to_string());
    if r.attempts < MAX_TX_ATTEMPTS && !shared.closed.load(Ordering::Relaxed) {
        shared.requeue(r);
    } else {
        shared.stats.tx_fallback.fetch_add(1, Ordering::Relaxed);
        shared.finish(
            r.t.txid,
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
