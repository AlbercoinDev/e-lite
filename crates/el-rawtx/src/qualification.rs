//! Admission for external wallet traffic. Public, operation-specific samples
//! are the only way a new, expired or paused route can become eligible.

use crate::governor::{Availability, Lane, TorGovernor};
use crate::{
    verify_block_by_hash, AttemptCtl, BlockSource, Endpoint, FetchMode, TxSource, LOCAL_BUSY,
};
use el_model::sync::Mutex;
use el_model::{Chain, Hash32};
use el_store::Store;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const VALID_FOR: Duration = Duration::from_secs(180);
const RETRY_BASE: Duration = Duration::from_secs(5);
const RETRY_CAP: Duration = Duration::from_secs(60);
const TICK: Duration = Duration::from_millis(250);
pub(crate) const TX_FAST: Duration = Duration::from_millis(2500);
const TOR_BLOCK_FIRST: Duration = Duration::from_secs(6);
const DIRECT_BLOCK_FIRST: Duration = Duration::from_secs(2);
const TOR_BLOCK_BUDGET: Duration = Duration::from_secs(20);
const DIRECT_BLOCK_BUDGET: Duration = Duration::from_secs(8);

#[derive(Clone, Hash, PartialEq, Eq)]
pub(crate) struct Route {
    block: bool,
    operation: String,
    endpoint: String,
    tor: bool,
}

impl Route {
    pub(crate) fn tx(source: &dyn TxSource, key: &str) -> Self {
        Self {
            block: false,
            operation: source.name().to_owned(),
            endpoint: key.to_owned(),
            tor: source.via_tor(key),
        }
    }

    pub(crate) fn block(source: &dyn BlockSource, endpoint: &Endpoint) -> Self {
        Self {
            block: true,
            operation: source.name().to_owned(),
            endpoint: endpoint.key.clone(),
            tor: endpoint.via_tor,
        }
    }
}

#[derive(Default)]
struct Record {
    until: Option<Instant>,
    retry: Option<Instant>,
    generation: u64,
    failures: u32,
    strikes: u32,
    probe_ms: Option<f64>,
}

#[derive(Default)]
struct State {
    routes: HashMap<Route, Record>,
    totals: QualificationSnapshot,
}

#[derive(Default, Clone, Copy, Debug)]
pub struct QualificationSnapshot {
    pub ready: usize,
    pub txid_ready: usize,
    pub blocks_ready: usize,
    pub probes: u64,
    pub probe_passes: u64,
    pub probe_slow: u64,
    pub probe_failures: u64,
    pub probe_streams: u64,
    pub skipped: u64,
    pub paused: u64,
    pub failed_streams: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProbeResult {
    Fast,
    Slow,
    Failed,
    RateLimited(Duration),
    Local,
}

#[derive(Default)]
struct Schedule {
    cursor: usize,
    prefer_block: bool,
}

pub(crate) struct Qualification {
    state: Mutex<State>,
    event: Arc<Availability>,
}

impl Qualification {
    pub(crate) fn new(event: Arc<Availability>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::default(),
            event,
        })
    }

    pub(crate) fn admit(&self, route: &Route) -> Option<u64> {
        self.state
            .lock()
            .ok()?
            .routes
            .get(route)
            .filter(|r| r.until.is_some_and(|until| until > Instant::now()))
            .map(|r| r.generation)
    }

    pub(crate) fn skipped(&self) {
        if let Ok(mut st) = self.state.lock() {
            st.totals.skipped += 1;
        }
    }

    pub(crate) fn probe_ms(&self, route: &Route) -> Option<f64> {
        self.state.lock().ok()?.routes.get(route)?.probe_ms
    }

    pub(crate) fn snapshot(&self) -> QualificationSnapshot {
        let Ok(st) = self.state.lock() else {
            return QualificationSnapshot::default();
        };
        let now = Instant::now();
        let (txid_ready, blocks_ready) = st
            .routes
            .iter()
            .filter(|(_, r)| r.until > Some(now))
            .fold((0, 0), |(tx, blocks), (route, _)| {
                if route.block {
                    (tx, blocks + 1)
                } else {
                    (tx + 1, blocks)
                }
            });
        QualificationSnapshot {
            ready: txid_ready + blocks_ready,
            txid_ready,
            blocks_ready,
            ..st.totals
        }
    }

    /// Old workers cannot renew a route that has expired or been put in
    /// quarantine since they started. A canceled hedge is not a sample.
    pub(crate) fn observe(&self, route: &Route, generation: u64, sample: Sample) {
        let Ok(mut st) = self.state.lock() else {
            return;
        };
        if matches!(
            sample,
            Sample::Failed | Sample::Invalid | Sample::RateLimited(_)
        ) && route.tor
        {
            st.totals.failed_streams += 1;
        }
        let Some(r) = st.routes.get_mut(route) else {
            return;
        };
        let now = Instant::now();
        if r.generation != generation || r.until <= Some(now) {
            return;
        }
        match sample {
            Sample::Fast => {
                r.until = Some(now + VALID_FOR);
                r.strikes = 0;
            }
            Sample::Failed | Sample::Slow | Sample::Invalid => {
                r.strikes += 1;
                if r.strikes >= 2 || sample == Sample::Invalid {
                    delay(r, now);
                    st.totals.paused += 1;
                    drop(st);
                    self.event.notify();
                }
            }
            Sample::RateLimited(after) => {
                delay(r, now);
                r.retry = r.retry.max(Some(now + after));
                st.totals.paused += 1;
                drop(st);
                self.event.notify();
            }
        }
    }

    fn due(&self, route: &Route, now: Instant) -> bool {
        self.state.lock().is_ok_and(|st| {
            st.routes
                .get(route)
                .is_none_or(|r| r.until <= Some(now) && r.retry <= Some(now))
        })
    }

    fn complete(&self, route: Route, result: ProbeResult, elapsed: Duration) {
        let Ok(mut st) = self.state.lock() else {
            return;
        };
        let now = Instant::now();
        if result != ProbeResult::Local {
            st.totals.probes += 1;
            st.totals.probe_streams += u64::from(route.tor);
        }
        match result {
            ProbeResult::Fast => st.totals.probe_passes += 1,
            ProbeResult::Slow => st.totals.probe_slow += 1,
            ProbeResult::Failed | ProbeResult::RateLimited(_) => st.totals.probe_failures += 1,
            ProbeResult::Local => {}
        }
        let r = st.routes.entry(route).or_default();
        match result {
            ProbeResult::Fast => {
                *r = Record {
                    until: Some(now + VALID_FOR),
                    generation: r.generation.wrapping_add(1),
                    probe_ms: Some(elapsed.as_secs_f64() * 1000.0),
                    ..Record::default()
                };
            }
            ProbeResult::Slow | ProbeResult::Failed => delay(r, now),
            ProbeResult::RateLimited(after) => {
                delay(r, now);
                r.retry = r.retry.max(Some(now + after));
            }
            ProbeResult::Local => r.retry = Some(now + TICK),
        }
        drop(st);
        self.event.notify();
    }
}

fn delay(r: &mut Record, now: Instant) {
    r.until = None;
    r.strikes = 0;
    r.generation = r.generation.wrapping_add(1);
    r.failures = r.failures.saturating_add(1);
    r.retry = Some(now + crate::health::backoff(RETRY_BASE, r.failures, RETRY_CAP));
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sample {
    Fast,
    Slow,
    Failed,
    Invalid,
    RateLimited(Duration),
}

pub(crate) fn block_fast(ctl: &AttemptCtl, start: Instant, end: Instant, tor: bool) -> bool {
    let (first_limit, total) = if tor {
        (TOR_BLOCK_FIRST, TOR_BLOCK_BUDGET)
    } else {
        (DIRECT_BLOCK_FIRST, DIRECT_BLOCK_BUDGET)
    };
    ctl.first_byte()
        .is_some_and(|first| first.saturating_duration_since(start) <= first_limit)
        && end.saturating_duration_since(start) <= total
}

enum Target {
    Tx(Arc<dyn TxSource>, String),
    Block(Arc<dyn BlockSource>, Endpoint),
}

impl Target {
    fn route(&self) -> Route {
        match self {
            Self::Tx(source, key) => Route::tx(source.as_ref(), key),
            Self::Block(source, endpoint) => Route::block(source.as_ref(), endpoint),
        }
    }

    fn probe(&self, ctl: &AttemptCtl, txids: &[Hash32], block: Option<Hash32>) -> ProbeResult {
        let start = Instant::now();
        match self {
            Self::Tx(source, key) => {
                let sample = &txids[..txids.len().min(source.max_batch_size())];
                if sample.is_empty() {
                    return ProbeResult::Local;
                }
                match source.fetch_from(key, sample, start + TX_FAST, ctl) {
                    Ok(batch)
                        if sample
                            .iter()
                            .all(|id| batch.get(id).is_some_and(|raw| valid_txid(raw, id))) =>
                    {
                        if start.elapsed() <= TX_FAST {
                            ProbeResult::Fast
                        } else {
                            ProbeResult::Slow
                        }
                    }
                    Err(e) if ctl.local_failure() || e.to_string().contains(LOCAL_BUSY) => {
                        ProbeResult::Local
                    }
                    Err(_) if ctl.retry_after().is_some() => {
                        ProbeResult::RateLimited(ctl.retry_after().unwrap())
                    }
                    _ if start.elapsed() >= TX_FAST => ProbeResult::Slow,
                    _ => ProbeResult::Failed,
                }
            }
            Self::Block(source, endpoint) => {
                let Some(hash) = block else {
                    return ProbeResult::Local;
                };
                let budget = if endpoint.via_tor {
                    TOR_BLOCK_BUDGET
                } else {
                    DIRECT_BLOCK_BUDGET
                };
                match source.fetch_endpoint(endpoint, &hash, start + budget, FetchMode::Quiet, ctl)
                {
                    Ok(bytes)
                        if verify_block_by_hash(&bytes, &hash)
                            .and_then(|block| crate::verify_witness_commitment(&block))
                            .is_ok() =>
                    {
                        if block_fast(ctl, start, Instant::now(), endpoint.via_tor) {
                            ProbeResult::Fast
                        } else {
                            ProbeResult::Slow
                        }
                    }
                    Err(e) if ctl.local_failure() || e.to_string().contains(LOCAL_BUSY) => {
                        ProbeResult::Local
                    }
                    Err(_) if ctl.retry_after().is_some() => {
                        ProbeResult::RateLimited(ctl.retry_after().unwrap())
                    }
                    _ if start.elapsed() >= budget => ProbeResult::Slow,
                    _ => ProbeResult::Failed,
                }
            }
        }
    }
}

pub(crate) fn valid_txid(raw: &[u8], id: &Hash32) -> bool {
    bitcoin::consensus::deserialize::<bitcoin::Transaction>(raw)
        .is_ok_and(|tx| tx.compute_txid().to_string() == id.to_hex())
}

/// Fixed old heights, never chosen from wallet activity. Non-genesis blocks
/// exercise archive service; the txid of the genesis coinbase is not served
/// by many providers. Until these heights are indexed, txid is ineligible.
fn samples(store: &Store, chain: Chain, tx: bool) -> (Vec<Hash32>, Option<Hash32>) {
    let first = match chain {
        Chain::Mainnet => 481_824, // first SegWit block: exercise historical witness delivery
        Chain::Signet | Chain::Testnet4 => 1_000,
        Chain::Regtest => 1,
    };
    let pinned = store.get_block(first).ok().flatten().map(|rec| rec.hash);
    let block = pinned.or_else(|| {
        store
            .committed_tip()
            .ok()
            .flatten()
            .and_then(|tip| {
                store
                    .get_block((tip.height / 2).clamp(1, first))
                    .ok()
                    .flatten()
            })
            .map(|rec| rec.hash)
    });
    let mut ids = Vec::new();
    if tx && pinned.is_some() {
        for height in first..first + 16 {
            let Some(rec) = store.get_block(height).ok().flatten() else {
                break;
            };
            if let Ok(txs) = store.block_txids(&rec) {
                ids.extend(txs.into_iter().take(4 - ids.len()));
            }
            if ids.len() == 4 {
                break;
            }
        }
    }
    (ids, block)
}

pub(crate) struct Worker {
    stop: Arc<AtomicBool>,
    event: Arc<Availability>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.event.notify();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Worker {
    pub(crate) fn start(
        gate: Arc<Qualification>,
        governor: Arc<TorGovernor>,
        store: Arc<Store>,
        chain: Chain,
        tx: Vec<Arc<dyn TxSource>>,
        blocks: Vec<Arc<dyn BlockSource>>,
    ) -> Option<Self> {
        if tx.is_empty() && blocks.is_empty() {
            return None;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let event = Arc::clone(&gate.event);
        let thread = std::thread::Builder::new()
            .name("el-provider-probe".into())
            .spawn(move || {
                let mut schedule = Schedule::default();
                let mut public = (Vec::new(), None);
                let mut resample = Instant::now();
                while !flag.load(Ordering::Relaxed) {
                    let seq = gate.event.sequence();
                    let now = Instant::now();
                    if now >= resample {
                        public = samples(&store, chain, !tx.is_empty());
                        resample = now
                            + if public.1.is_some() && (tx.is_empty() || public.0.len() == 4) {
                                Duration::from_secs(30)
                            } else {
                                Duration::from_secs(2)
                            };
                    }
                    let mut targets = Vec::new();
                    if !public.0.is_empty() {
                        for source in &tx {
                            for key in source.all_servers() {
                                targets.push(Target::Tx(Arc::clone(source), key));
                            }
                        }
                    }
                    if public.1.is_some() {
                        for source in &blocks {
                            for endpoint in source.probe_endpoints() {
                                targets.push(Target::Block(Arc::clone(source), endpoint));
                            }
                        }
                    }
                    // Stable ordering and a rotating cursor prevent an
                    // unreachable route from hiding the rest. Give blocks
                    // a turn before the first large txid burst.
                    targets.sort_by_cached_key(|target| {
                        let r = target.route();
                        (!r.tor, r.block, r.operation, r.endpoint)
                    });
                    let len = targets.len();
                    let due = |i: usize| {
                        let target = &targets[i];
                        // Explicit clearnet fallback is still a fallback.
                        // Don't send probe traffic directly while any Tor
                        // block route is qualified.
                        let fallback =
                            matches!(target, Target::Block(_, endpoint) if endpoint.tier > 0);
                        gate.due(&target.route(), now)
                            && (!fallback
                                || !targets.iter().any(|other| {
                                    let route = other.route();
                                    route.tor && route.block && gate.admit(&route).is_some()
                                }))
                    };
                    let next = || (0..len).map(|i| (i + schedule.cursor) % len);
                    if let Some(i) = next()
                        .filter(|&i| due(i))
                        .find(|&i| targets[i].route().block == schedule.prefer_block)
                        .or_else(|| next().find(|&i| due(i)))
                    {
                        let target = &targets[i];
                        let route = target.route();
                        let stream = if route.tor {
                            // At most one probe globally; leave a stream free
                            // and never jump ahead of a waiting wallet.
                            (governor.in_use() + 1 < governor.total())
                                .then(|| governor.try_acquire(Lane::Decoy))
                                .flatten()
                        } else {
                            None
                        };
                        if !route.tor || stream.is_some() {
                            schedule.cursor = i + 1;
                            schedule.prefer_block = !route.block;
                            let ctl = match target {
                                Target::Block(_, endpoint) => AttemptCtl::for_endpoint(endpoint),
                                Target::Tx(..) => AttemptCtl::new(),
                            }
                            .with_health(false);
                            let result = match target {
                                Target::Tx(..) => target.probe(
                                    &ctl.with_streams(Arc::clone(&governor), Lane::Decoy, stream),
                                    &public.0,
                                    public.1,
                                ),
                                Target::Block(..) => {
                                    let result = target.probe(&ctl, &[], public.1);
                                    ctl.close_sockets();
                                    drop(stream);
                                    result
                                }
                            };
                            if crate::debug_logs() && result != ProbeResult::Local {
                                el_model::debug_event!(
                                    "provider probe src={} ep={} tor={} {:?} {} ms",
                                    route.operation,
                                    crate::redact_hashes(&route.endpoint),
                                    route.tor,
                                    result,
                                    now.elapsed().as_millis(),
                                );
                            }
                            gate.complete(route, result, now.elapsed());
                            continue;
                        }
                    }
                    gate.event.wait(seq, Some(now + TICK));
                }
            })
            .ok()?;
        Some(Self {
            stop,
            event,
            thread: Some(thread),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(endpoint: &str, block: bool, tor: bool) -> Route {
        Route {
            block,
            operation: "esplora".into(),
            endpoint: endpoint.into(),
            tor,
        }
    }

    #[test]
    fn fresh_fast_proof_is_required_per_transport_and_operation() {
        let q = Qualification::new(Arc::new(Availability::default()));
        let https = route("https://example.test", false, true);
        let onion = route("http://example.onion", false, true);
        let blocks = route("http://example.onion", true, true);
        let direct = route("https://example.test", false, false);
        assert!(q.admit(&https).is_none());
        q.complete(https.clone(), ProbeResult::Slow, Duration::from_secs(3));
        q.complete(onion.clone(), ProbeResult::Fast, Duration::from_secs(1));
        assert!(q.admit(&https).is_none());
        assert!(q.admit(&onion).is_some());
        assert!(q.admit(&blocks).is_none());
        assert!(q.admit(&direct).is_none());
        let mut st = q.state.lock().unwrap();
        st.routes.get_mut(&onion).unwrap().until = Some(Instant::now());
        drop(st);
        assert!(q.admit(&onion).is_none());
        assert_eq!(q.snapshot().probe_slow, 1);
    }

    #[test]
    fn failures_pause_and_a_stale_worker_cannot_reopen_a_route() {
        let q = Qualification::new(Arc::new(Availability::default()));
        let endpoint = route("https://example.test", false, true);
        q.complete(endpoint.clone(), ProbeResult::Fast, Duration::from_secs(1));
        let generation = q.admit(&endpoint).unwrap();
        q.observe(&endpoint, generation, Sample::Failed);
        assert_eq!(q.snapshot().paused, 0);
        q.observe(&endpoint, generation, Sample::Failed);
        assert!(q.admit(&endpoint).is_none());
        q.observe(&endpoint, generation, Sample::Fast);
        assert!(!q.due(&endpoint, Instant::now()));
        assert_eq!(q.snapshot().failed_streams, 2);
        q.complete(endpoint.clone(), ProbeResult::Fast, Duration::from_secs(1));
        assert_ne!(q.admit(&endpoint), Some(generation));
        assert_eq!(q.snapshot().ready, 1);
    }
}
