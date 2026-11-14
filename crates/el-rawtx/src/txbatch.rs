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
use crate::{AttemptCtl, RawtxError, RawtxStats, Result, Semaphore};
use el_model::{Hash32, ScriptHash};
use el_store::Store;
use rand::seq::SliceRandom;
use rand::thread_rng;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Servers a real transaction is tried on before the block fallback.
const MAX_TX_ATTEMPTS: u32 = 3;
/// Budget of one session (connection, version, batch, answer).
const SESSION_BUDGET: Duration = Duration::from_secs(40);
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
    /// Sessions open per server (at most `MAX_SESSIONS_PER_SERVER`).
    per_server: Mutex<HashMap<String, Arc<Semaphore>>>,
    /// Average session time per server, ms (failures count as slow).
    latency: Mutex<HashMap<String, f64>>,
    /// Items of each group sent to each server during a connection's load,
    /// keyed by (connection, group): keeps `max_share` across batches.
    spread: Mutex<HashMap<(u64, usize), HashMap<String, usize>>>,
    stats: Arc<RawtxStats>,
    closed: AtomicBool,
}

/// The per-txid fetch mode of a resolver.
pub(crate) struct TxMode {
    shared: Arc<Shared>,
}

impl Drop for TxMode {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::Relaxed);
        self.shared.queue_cv.notify_all();
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
    ) -> Option<Self> {
        if sources.is_empty() {
            return None;
        }
        let shared = Arc::new(Shared {
            picker: DecoyPicker::new(Arc::clone(&store), secret, params.clone()),
            store,
            txcache,
            sources,
            sessions: Semaphore::new(params.parallel_sessions.max(1)),
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
        });
        let worker = Arc::clone(&shared);
        if let Err(e) = std::thread::Builder::new()
            .name("el-txbatch".into())
            .spawn(move || batch_loop(worker))
        {
            eprintln!("[e-lite] tx batcher did not start: {e}");
            return None;
        }
        Some(TxMode { shared })
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
        dispatch(&shared, reqs);
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
        let decoys = shared.with_book(ctx, |book, _| book.take(n));
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
    let mut order: Vec<usize> = (0..servers.len()).collect();
    order.shuffle(&mut rng);
    order.sort_by(|&a, &b| rank[a].total_cmp(&rank[b]));
    let candidates: Vec<usize> = order
        .into_iter()
        .take(spread.max(servers.len().div_ceil(2)))
        .collect();
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
        }
    }
    drop(spread_counts);

    for (idx, mut items) in per_server {
        items.shuffle(&mut rng);
        for chunk in items.chunks(p.session_size.max(1)) {
            let (src_idx, key, _) = servers[idx].clone();
            let src = Arc::clone(&shared.sources[src_idx]);
            let chunk = chunk.to_vec();
            let shared = Arc::clone(shared);
            let spawned = std::thread::Builder::new()
                .name("el-txsession".into())
                .spawn(move || run_session(&shared, src, &key, chunk));
            if let Err(e) = spawned {
                eprintln!("[e-lite] tx session did not start: {e}");
            }
        }
    }
}

fn run_session(shared: &Arc<Shared>, src: Arc<dyn TxSource>, key: &str, items: Vec<Item>) {
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
    // A failed session counts as a slow one, so the server drops in rank.
    let ms = if result.is_ok() {
        started.elapsed().as_millis() as f64
    } else {
        SESSION_BUDGET.as_millis() as f64
    };
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
