//! Raw transaction resolution for confirmed history.
//!
//! Order: local Core (zero leak) -> validated disk cache -> external sources
//! (P2P and/or Esplora) fetched as **whole blocks**, verified against the local
//! header chain, then cached. External fetches must go through Tor unless the
//! operator explicitly allows clearnet.

pub mod cache;
pub mod cachelayout;
pub mod core_source;
pub mod esplora;
pub(crate) mod health;
pub mod p2p;
pub mod txcache;
pub mod verify;

use el_model::{Chain, Hash32};
use el_store::Store;
use rand::seq::SliceRandom;
use rand::thread_rng;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// HTTP `User-Agent` for every external request. A generic value: the
/// project name and version would put the operator in a tiny anonymity set
/// for every provider (AUD-133).
pub const HTTP_USER_AGENT: &str = "Mozilla/5.0";

/// P2P `version` user agent. A common Core subversion instead of an
/// e-lite-specific one, so spy nodes cannot single out e-lite clients by
/// the cheapest filter (AUD-133). Behaviour (one getdata, then disconnect)
/// can still be fingerprinted.
pub const P2P_USER_AGENT: &[u8] = b"/Satoshi:29.0.0/";

pub use cache::BlockCache;
pub use cachelayout::{CacheInventory, CACHE_DIR_NAME};
pub use txcache::TxCache;

/// Verified external blocks kept in memory (a mainnet block decodes to a few
/// MiB; 8 cover a wallet's burst of requests without a re-download).
const RECENT_BLOCKS: usize = 8;

/// Where a verified block came from; decoys follow external fetches only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Origin {
    Recent,
    BlockCache,
    Local,
    External,
}

/// Split the `cache_mib` disk budget: `(block_mib, tx_mib)`. Whole blocks
/// get a quarter, at least 1 and at most 64 MiB; transactions the rest.
pub fn cache_split(cache_mib: u64) -> (u64, u64) {
    if cache_mib == 0 {
        return (0, 0);
    }
    let block = (cache_mib / 4).clamp(1, 64);
    (block, cache_mib - block)
}

/// Full verification of a block against the indexed record.
fn verify_full(bytes: &[u8], rec: &el_model::BlockRecord) -> Result<bitcoin::Block> {
    let block = verify_block(bytes, &rec.hash, &rec.header)?;
    verify_witness_commitment(&block)?;
    Ok(block)
}

/// A cached transaction must reproduce the indexed header's merkle root.
fn verify_cached_tx(
    entry: &txcache::CachedTx,
    rec: &el_model::BlockRecord,
    txid: Hash32,
) -> Result<()> {
    if entry.height != rec.height || entry.tx_pos >= rec.tx_count {
        return Err(RawtxError::Verify(
            "cached position does not match the index".into(),
        ));
    }
    let tx: bitcoin::Transaction = bitcoin::consensus::deserialize(&entry.raw)
        .map_err(|e| RawtxError::Verify(format!("cannot decode cached tx: {e}")))?;
    let computed = Hash32::from_hex(&tx.compute_txid().to_string())
        .map_err(|e| RawtxError::Verify(format!("bad txid: {e}")))?;
    if computed != txid {
        return Err(RawtxError::Verify("cached txid mismatch".into()));
    }
    let header: bitcoin::block::Header = bitcoin::consensus::deserialize(&rec.header)
        .map_err(|e| RawtxError::Verify(format!("bad indexed header: {e}")))?;
    let root = el_model::merkle_root_from_branch(txid, &entry.branch, entry.tx_pos as usize);
    if root.to_hex() != header.merkle_root.to_string() {
        return Err(RawtxError::Verify("cached merkle branch mismatch".into()));
    }
    Ok(())
}
pub use core_source::CoreBlockSource;
pub use esplora::{isolated_tor_client, EsploraSource};
pub use p2p::{P2pSource, PeerHint};
pub use verify::{extract_tx, verify_block, verify_block_by_hash, verify_witness_commitment};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawtxError {
    /// No source could even be asked: none is configured, or Tor is required
    /// and unavailable.
    NoSource(String),
    /// Every source that was asked failed.
    Unavailable(String),
    Verify(String),
    Config(String),
    Io(String),
}

impl fmt::Display for RawtxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RawtxError::NoSource(m) | RawtxError::Unavailable(m) => {
                write!(f, "rawtx unavailable: {m}")
            }
            RawtxError::Verify(m) => write!(f, "rawtx verification failed: {m}"),
            RawtxError::Config(m) => write!(f, "rawtx config: {m}"),
            RawtxError::Io(m) => write!(f, "rawtx io: {m}"),
        }
    }
}

impl std::error::Error for RawtxError {}

impl RawtxError {
    /// Error class without details. Details can carry a txid or the block
    /// hash of a wallet transaction, which must not reach the log (AUD-132);
    /// the wallet still receives the full message in its RPC error.
    pub fn kind(&self) -> &'static str {
        match self {
            RawtxError::NoSource(_) => "no source",
            RawtxError::Unavailable(_) => "unavailable",
            RawtxError::Verify(_) => "verification failed",
            RawtxError::Config(_) => "config",
            RawtxError::Io(_) => "io",
        }
    }
}

impl From<el_store::StoreError> for RawtxError {
    fn from(e: el_store::StoreError) -> Self {
        RawtxError::Io(e.to_string())
    }
}

impl From<el_model::CodecError> for RawtxError {
    fn from(e: el_model::CodecError) -> Self {
        RawtxError::Io(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, RawtxError>;

/// Replace every 64-hex-digit run (txid, block hash, script hash) with
/// `<hash>`, so an error that mentions one can be logged (AUD-132).
pub fn redact_hashes(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        let run = bytes[i..]
            .iter()
            .take_while(|b| b.is_ascii_hexdigit())
            .count();
        if run >= 64 {
            out.push_str("<hash>");
            i += run;
        } else if run > 0 {
            out.push_str(&text[i..i + run]);
            i += run;
        } else {
            let ch = text[i..].chars().next().unwrap_or(' ');
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// Total time one external block download may take, from the moment its
/// leader registers it (queue wait included) to the last source. Callers that
/// share the download wait `EXTERNAL_BUDGET + FOLLOWER_GRACE`, so a follower
/// never gives up before its leader (AUD-156).
pub const EXTERNAL_BUDGET: Duration = Duration::from_secs(60);
/// Extra time a follower waits beyond the leader's deadline.
pub const FOLLOWER_GRACE: Duration = Duration::from_secs(5);
/// A block whose download just failed is answered from that failure for this
/// long instead of asking every source again (AUD-158).
pub const FAILURE_MEMORY: Duration = Duration::from_secs(10);
/// An attempt with less time left than this is not started.
pub(crate) const MIN_ATTEMPT: Duration = Duration::from_secs(2);

/// Whether a download may change a source's view of its endpoints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FetchMode {
    /// A wallet or prefetch download: failures cool endpoints down.
    Normal,
    /// A decoy: its failures are not the wallet's problem and must not cool
    /// down a provider or peer that real requests need (AUD-157).
    Quiet,
}

/// A source that can return a **whole raw block** by hash.
pub trait BlockSource: Send + Sync {
    fn name(&self) -> &str;
    fn fetch_block(&self, hash: &Hash32) -> Result<Vec<u8>>;
    /// Like `fetch_block`, bounded by `deadline` and in `mode`. Sources that
    /// talk to the network override it; the default ignores both.
    fn fetch_block_until(
        &self,
        hash: &Hash32,
        deadline: Instant,
        mode: FetchMode,
    ) -> Result<Vec<u8>> {
        let _ = (deadline, mode);
        self.fetch_block(hash)
    }
    /// Whether the source is being rate limited right now (decoys wait).
    fn under_pressure(&self) -> bool {
        false
    }
    /// Local sources never leak anything to third parties.
    fn is_local(&self) -> bool {
        false
    }
}

#[derive(Default)]
pub struct RawtxStats {
    pub cache_hits: AtomicU64,
    pub cache_puts: AtomicU64,
    pub fetches: AtomicU64,
    pub bytes: AtomicU64,
    pub verify_failures: AtomicU64,
    pub decoys: AtomicU64,
    pub unavailable: AtomicU64,
    pub p2p_attempts: AtomicU64,
    pub p2p_failures: AtomicU64,
    pub p2p_skipped: AtomicU64,
    pub p2p_tor_failures: AtomicU64,
}

#[derive(Clone, Debug)]
pub struct RawtxConfig {
    /// When false, only the local Core block source and the disk cache are used.
    pub enabled: bool,
    pub cache_mib: u64,
    pub decoys: u32,
    pub tor_socks: Option<String>,
    pub allow_clearnet: bool,
    /// If Tor is configured but a request fails, retry the clearnet endpoint
    /// directly. This exposes the operator's IP to the provider.
    pub fallback_clearnet: bool,
    /// Timeout for a single direct (clearnet) Esplora block request.
    pub request_timeout_seconds: u64,
    /// Absolute limit of one Esplora attempt (Tor or direct); a transfer
    /// that keeps receiving bytes may use all of it.
    pub tor_request_timeout_seconds: u64,
    /// Circuit + connection time allowed for an Esplora request through Tor.
    pub tor_connect_timeout_seconds: u64,
    /// No response header or body byte for this long through Tor: stalled.
    pub tor_stall_timeout_seconds: u64,
    /// Skip a provider (Tor or direct) after a failed attempt.
    pub provider_cooldown_seconds: u64,
    /// Maximum concurrent external block downloads.
    pub max_parallel_fetches: usize,
    /// Cache directory override. `None` uses `<data-dir>/rawtx-cache`.
    pub cache_dir: Option<std::path::PathBuf>,
    pub use_p2p: bool,
    pub use_esplora: bool,
    pub p2p_v2: bool,
    pub esplora_urls: Vec<String>,
    pub p2p_peers: Vec<String>,
    pub p2p_timeout: Duration,
    /// Per-attempt limit of a P2P connection made through Tor.
    pub p2p_tor_timeout: Duration,
    pub chain: Chain,
}

/// A block download shared by concurrent callers asking for the same height.
struct InflightBlock {
    done: Mutex<bool>,
    result: Mutex<Option<Result<Vec<u8>>>>,
    cv: Condvar,
    /// The leader's deadline, fixed when it registered (before waiting for
    /// a download slot). Followers wait until `deadline + FOLLOWER_GRACE`.
    deadline: Instant,
}

impl InflightBlock {
    fn new(deadline: Instant) -> Self {
        Self {
            done: Mutex::new(false),
            result: Mutex::new(None),
            cv: Condvar::new(),
            deadline,
        }
    }
}

/// Who asks for an external block. A wallet request may take the last free
/// download slot; the background prefetch always leaves one for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Priority {
    Wallet,
    Prefetch,
}

/// Decoys owed for real external downloads, at most this many.
const MAX_DECOY_DEBT: usize = 32;

/// Decoy downloads owed for real external downloads (wallet or prefetch),
/// served one at a time by a background thread outside the wallet's
/// download slots. A decoy never changes a provider's or peer's health and
/// waits while a provider rate-limits (AUD-157).
struct DecoyLane {
    queue: Mutex<VecDeque<Hash32>>,
    cv: Condvar,
    closed: std::sync::atomic::AtomicBool,
    started: std::sync::Once,
}

impl DecoyLane {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            queue: Mutex::new(VecDeque::new()),
            cv: Condvar::new(),
            closed: std::sync::atomic::AtomicBool::new(false),
            started: std::sync::Once::new(),
        })
    }

    fn pending(&self) -> usize {
        self.queue.lock().map(|q| q.len()).unwrap_or(0)
    }

    /// Next owed decoy, or `None` once the resolver is gone.
    fn next(&self) -> Option<Hash32> {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if self.closed.load(Ordering::Relaxed) {
                return None;
            }
            if let Some(hash) = queue.front().copied() {
                return Some(hash);
            }
            queue = self
                .cv
                .wait_timeout(queue, Duration::from_secs(1))
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
    }

    fn done(&self) {
        if let Ok(mut queue) = self.queue.lock() {
            queue.pop_front();
        }
    }

    /// Sleep up to `d`, returning early (false) once the resolver is gone.
    fn pause(&self, d: Duration) -> bool {
        let end = Instant::now() + d;
        while Instant::now() < end {
            if self.closed.load(Ordering::Relaxed) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50).min(end - Instant::now()));
        }
        !self.closed.load(Ordering::Relaxed)
    }
}

/// Small counting semaphore: bounds total external downloads without pulling
/// in an async runtime.
struct Semaphore {
    permits: Mutex<usize>,
    total: usize,
    cv: Condvar,
}

impl Semaphore {
    fn new(permits: usize) -> Arc<Self> {
        let total = permits.max(1);
        Arc::new(Self {
            permits: Mutex::new(total),
            total,
            cv: Condvar::new(),
        })
    }

    /// Take a permit while more than `keep_free` are free, waiting until
    /// `deadline` at most. `keep_free` never blocks a single-permit pool.
    fn acquire_until(self: &Arc<Self>, deadline: Instant, keep_free: usize) -> Option<Permit> {
        let keep_free = keep_free.min(self.total - 1);
        let mut permits = self
            .permits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while *permits <= keep_free {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            permits = self
                .cv
                .wait_timeout(permits, left)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
        *permits -= 1;
        Some(Permit {
            sem: Arc::clone(self),
        })
    }
}

struct Permit {
    sem: Arc<Semaphore>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut permits = self
            .sem
            .permits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *permits += 1;
        // Every waiter re-checks: a prefetch may not take what a wallet
        // request is waiting for.
        self.sem.cv.notify_all();
    }
}

pub struct Resolver {
    store: Arc<Store>,
    /// Local source, tried first and never cached.
    local: Option<Arc<dyn BlockSource>>,
    /// External sources other than P2P, tried in random order.
    external: Vec<Arc<dyn BlockSource>>,
    /// P2P source, kept apart so its peer list can be refreshed (AUD-137).
    /// It counts as a source only while it has peers.
    p2p: Option<Arc<P2pSource>>,
    /// Whole blocks: prune-gap repair, plus entries from older releases.
    cache: Option<BlockCache>,
    /// Verified transactions for wallet requests (AUD-139).
    txcache: Option<TxCache>,
    /// Verified external blocks kept in memory for a burst of requests.
    recent: Mutex<VecDeque<(Hash32, Arc<bitcoin::Block>)>>,
    cache_dir: std::path::PathBuf,
    decoys: u32,
    stats: Arc<RawtxStats>,
    chain: Chain,
    inflight: Mutex<HashMap<Hash32, Arc<InflightBlock>>>,
    /// Blocks whose download failed recently, with the error (AUD-158).
    failed: Mutex<HashMap<Hash32, (Instant, RawtxError)>>,
    external_slots: Arc<Semaphore>,
    decoy_lane: Arc<DecoyLane>,
}

impl Drop for Resolver {
    fn drop(&mut self) {
        self.decoy_lane.closed.store(true, Ordering::Relaxed);
        self.decoy_lane.cv.notify_all();
    }
}

impl Resolver {
    pub fn new(store: Arc<Store>, cfg: &RawtxConfig) -> Result<Self> {
        let mut external: Vec<Arc<dyn BlockSource>> = Vec::new();
        let mut p2p = None;
        let tor = cfg.tor_socks.clone();
        let stats = Arc::new(RawtxStats::default());

        if cfg.enabled {
            // Created even with no peers yet: the list is refreshed from Core
            // at runtime, and an empty source is not counted as a source.
            if cfg.use_p2p {
                if tor.is_none() && !cfg.allow_clearnet && !cfg.fallback_clearnet {
                    // fail closed: no external P2P without Tor and no fallback
                } else {
                    p2p = Some(Arc::new(
                        P2pSource::new(
                            cfg.p2p_peers.clone(),
                            tor.clone(),
                            cfg.p2p_timeout,
                            cfg.chain,
                            cfg.p2p_v2,
                            cfg.fallback_clearnet,
                            Duration::from_secs(cfg.provider_cooldown_seconds.max(1)),
                        )
                        .with_tor_timeout(cfg.p2p_tor_timeout)
                        .with_stats(Arc::clone(&stats)),
                    ));
                }
            }
            if cfg.use_esplora && !cfg.esplora_urls.is_empty() {
                match EsploraSource::with_budget(
                    cfg.esplora_urls.clone(),
                    tor.clone(),
                    cfg.allow_clearnet,
                    cfg.fallback_clearnet,
                    Duration::from_secs(cfg.request_timeout_seconds.max(1)),
                    Duration::from_secs(cfg.tor_request_timeout_seconds.max(1)),
                    Duration::from_secs(cfg.provider_cooldown_seconds.max(1)),
                ) {
                    Ok(s) => external.push(Arc::new(s.with_tor_timeouts(
                        Duration::from_secs(cfg.tor_connect_timeout_seconds.max(1)),
                        Duration::from_secs(cfg.tor_stall_timeout_seconds.max(1)),
                    ))),
                    Err(e) => eprintln!("[e-lite] esplora disabled: {e}"),
                }
            }
        }
        if let Some(p2p) = &p2p {
            p2p.set_force_when_cooling(external.is_empty());
        }

        let cache_dir = cfg
            .cache_dir
            .clone()
            .unwrap_or_else(|| store.path().join("rawtx-cache"));
        // `cache_mib` is the total disk budget: a quarter (1..64 MiB) for
        // whole blocks (repair, legacy entries), the rest for transactions.
        let (block_mib, tx_mib) = cache_split(cfg.cache_mib);
        let cache = if block_mib > 0 {
            Some(BlockCache::open(
                &cache_dir,
                block_mib.saturating_mul(1024 * 1024),
            )?)
        } else {
            None
        };
        let txcache = if tx_mib > 0 {
            Some(TxCache::open(
                &cache_dir.join("tx"),
                tx_mib.saturating_mul(1024 * 1024),
            )?)
        } else {
            None
        };

        Ok(Resolver {
            store,
            local: None,
            external,
            p2p,
            cache,
            txcache,
            recent: Mutex::new(VecDeque::new()),
            cache_dir,
            decoys: cfg.decoys,
            stats,
            chain: cfg.chain,
            inflight: Mutex::new(HashMap::new()),
            failed: Mutex::new(HashMap::new()),
            external_slots: Semaphore::new(cfg.max_parallel_fetches),
            decoy_lane: DecoyLane::new(),
        })
    }

    pub fn with_local(mut self, source: Arc<dyn BlockSource>) -> Self {
        self.local = Some(source);
        self
    }

    /// Inject an additional external source (used by tests and custom setups).
    pub fn with_external(mut self, source: Arc<dyn BlockSource>) -> Self {
        self.external.push(source);
        if let Some(p2p) = &self.p2p {
            p2p.set_force_when_cooling(false);
        }
        self
    }

    pub fn stats(&self) -> Arc<RawtxStats> {
        Arc::clone(&self.stats)
    }

    pub fn external_sources(&self) -> usize {
        self.sources().len()
    }

    /// External sources usable right now: the configured ones plus P2P when
    /// it has at least one peer.
    fn sources(&self) -> Vec<Arc<dyn BlockSource>> {
        let mut out = self.external.clone();
        if let Some(p2p) = &self.p2p {
            if p2p.peer_count() > 0 {
                out.push(Arc::clone(p2p) as Arc<dyn BlockSource>);
            }
        }
        out
    }

    /// Replace the P2P peer list (discovered from Core). Returns the new
    /// count, or `None` when P2P is disabled.
    pub fn refresh_p2p_peers(&self, peers: Vec<PeerHint>) -> Option<usize> {
        self.p2p.as_ref().map(|p2p| p2p.set_peers(peers))
    }

    /// Current P2P peer count, `None` when P2P is disabled.
    pub fn p2p_peers(&self) -> Option<usize> {
        self.p2p.as_ref().map(|p2p| p2p.peer_count())
    }

    /// Directory used by the validated rawtx cache.
    pub fn cache_dir(&self) -> &std::path::Path {
        &self.cache_dir
    }

    /// Bytes stored in the rawtx caches (blocks + transactions).
    pub fn cache_size_bytes(&self) -> u64 {
        self.cache.as_ref().map(|c| c.size()).unwrap_or(0)
            + self.txcache.as_ref().map(|c| c.size()).unwrap_or(0)
    }

    /// Transactions in the transaction cache.
    pub fn cached_txs(&self) -> usize {
        self.txcache.as_ref().map(|c| c.len()).unwrap_or(0)
    }

    /// Whole blocks in the block cache.
    pub fn cached_blocks(&self) -> usize {
        self.cache.as_ref().map(|c| c.len()).unwrap_or(0)
    }

    /// Cache inventory from the in-memory state each cache rebuilt at open.
    /// O(1): never walks the directory (that is `cachelayout::inventory`).
    pub fn cache_inventory(&self) -> CacheInventory {
        CacheInventory {
            block_files: self.cached_blocks() as u64,
            block_bytes: self.cache.as_ref().map(|c| c.size()).unwrap_or(0),
            tx_files: self.cached_txs() as u64,
            tx_bytes: self.txcache.as_ref().map(|c| c.size()).unwrap_or(0),
            skipped: 0,
        }
    }

    /// Whether a pruned-index repair has any chance: a validated block cache
    /// or at least one external whole-block source.
    pub fn has_repair_sources(&self) -> bool {
        !self.sources().is_empty() || self.cache.as_ref().map(|c| !c.is_empty()).unwrap_or(false)
    }

    /// Fetch a whole block by hash for index repair, verifying its hash and
    /// merkle root. This is only used for heights Core has already pruned; the
    /// normal index path keeps using the local bulk source/RPC and never sends
    /// block hashes to external sources.
    pub fn fetch_block_by_hash(&self, hash: &Hash32) -> Result<Vec<u8>> {
        if let Some(cache) = &self.cache {
            if let Some(bytes) = cache.get(hash) {
                match verify_block_by_hash(&bytes, hash) {
                    Ok(_) => {
                        self.stats.cache_hits.fetch_add(1, Ordering::Relaxed);
                        return Ok(bytes);
                    }
                    Err(e) => {
                        self.stats.verify_failures.fetch_add(1, Ordering::Relaxed);
                        eprintln!(
                            "[e-lite] cached repair block {} rejected: {e}",
                            hash.to_hex()
                        );
                        let _ = cache.remove(hash);
                    }
                }
            }
        }

        let mut errors = Vec::new();
        if let Some(local) = &self.local {
            match local.fetch_block(hash) {
                Ok(bytes) => match verify_block_by_hash(&bytes, hash) {
                    Ok(_) => {
                        self.cache_store(hash, &bytes);
                        return Ok(bytes);
                    }
                    Err(e) => errors.push(format!("{}: {e}", local.name())),
                },
                Err(e) => errors.push(format!("{}: {e}", local.name())),
            }
        }

        let sources = self.sources();
        let mut order: Vec<usize> = (0..sources.len()).collect();
        order.shuffle(&mut thread_rng());
        for idx in order {
            let source = &sources[idx];
            match source.fetch_block(hash) {
                Ok(bytes) => {
                    self.stats.fetches.fetch_add(1, Ordering::Relaxed);
                    self.stats
                        .bytes
                        .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                    match verify_block_by_hash(&bytes, hash) {
                        Ok(_) => {
                            self.cache_store(hash, &bytes);
                            return Ok(bytes);
                        }
                        Err(e) => {
                            self.stats.verify_failures.fetch_add(1, Ordering::Relaxed);
                            eprintln!(
                                "[e-lite] {} returned an invalid repair block: {e}",
                                source.name()
                            );
                            errors.push(format!("{}: {e}", source.name()));
                        }
                    }
                }
                Err(e) => errors.push(format!("{}: {e}", source.name())),
            }
        }
        self.stats.unavailable.fetch_add(1, Ordering::Relaxed);
        Err(RawtxError::Unavailable(if errors.is_empty() {
            "no repair source configured (cache, P2P or Esplora)".to_string()
        } else {
            errors.join("; ")
        }))
    }

    pub fn chain(&self) -> Chain {
        self.chain
    }

    /// Resolve a confirmed transaction's raw hex at `height`.
    pub fn raw_tx(&self, txid: Hash32, height: u32) -> Result<String> {
        self.raw_tx_inner(txid, height, Priority::Wallet)
    }

    /// Background prefetch path: same lookup and decoys, lower priority.
    pub fn raw_tx_prefetch(&self, txid: Hash32, height: u32) -> Result<String> {
        self.raw_tx_inner(txid, height, Priority::Prefetch)
    }

    /// Prefetch every listed transaction of one block with a single block
    /// fetch, caching each one. An external download owes decoys like a
    /// wallet request (AUD-159) and never takes the last free download
    /// slot. Returns how many were fetched (0 when all were already cached).
    pub fn prefetch_block_txs(&self, height: u32, txids: &[Hash32]) -> Result<usize> {
        let rec = self.block_record(height)?;
        let missing: Vec<Hash32> = txids
            .iter()
            .copied()
            .filter(|txid| self.cached_tx(&rec, *txid).is_none())
            .collect();
        if missing.is_empty() {
            return Ok(0);
        }
        let (block, _) = self.verified_block(&rec, Priority::Prefetch)?;
        self.cache_txs(&rec, &block, &missing)?;
        Ok(missing.len())
    }

    fn block_record(&self, height: u32) -> Result<el_model::BlockRecord> {
        self.store
            .get_block(height)?
            .ok_or_else(|| RawtxError::Unavailable(format!("block {height} not indexed")))
    }

    fn raw_tx_inner(&self, txid: Hash32, height: u32, priority: Priority) -> Result<String> {
        let rec = self.block_record(height)?;
        // 1) Transaction cache: one small file per transaction (AUD-139).
        if let Some(hex) = self.cached_tx(&rec, txid) {
            self.stats.cache_hits.fetch_add(1, Ordering::Relaxed);
            return Ok(hex);
        }
        // 2) The block: recent blocks in memory, legacy block cache, local
        //    Core, then external sources (shared across concurrent callers).
        let (block, _) = self.verified_block(&rec, priority)?;
        let hex = self
            .cache_txs(&rec, &block, &[txid])?
            .pop()
            .ok_or_else(|| RawtxError::Verify("transaction is not in the block".into()))?;
        Ok(hex)
    }

    /// A cached transaction authenticated against the indexed header: txid,
    /// position and merkle branch must reproduce the header's merkle root.
    /// A mismatching entry is removed.
    fn cached_tx(&self, rec: &el_model::BlockRecord, txid: Hash32) -> Option<String> {
        let cache = self.txcache.as_ref()?;
        let entry = cache.get(&txid)?;
        match verify_cached_tx(&entry, rec, txid) {
            Ok(()) => Some(hex::encode(&entry.raw)),
            Err(e) => {
                self.stats.verify_failures.fetch_add(1, Ordering::Relaxed);
                eprintln!(
                    "[e-lite] cached transaction at height {} rejected ({})",
                    rec.height,
                    e.kind()
                );
                cache.remove(&txid);
                None
            }
        }
    }

    /// Store each requested transaction of a verified block in the
    /// transaction cache and return their hex, in request order.
    fn cache_txs(
        &self,
        rec: &el_model::BlockRecord,
        block: &bitcoin::Block,
        wanted: &[Hash32],
    ) -> Result<Vec<String>> {
        let txids = block
            .txdata
            .iter()
            .map(|tx| {
                Hash32::from_hex(&tx.compute_txid().to_string())
                    .map_err(|e| RawtxError::Verify(format!("bad txid: {e}")))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut out = Vec::with_capacity(wanted.len());
        for txid in wanted {
            let pos = txids
                .iter()
                .position(|t| t == txid)
                .ok_or_else(|| RawtxError::Verify("transaction is not in the block".into()))?;
            let raw = bitcoin::consensus::serialize(&block.txdata[pos]);
            if let Some(cache) = &self.txcache {
                if let Some((_, branch)) = el_model::merkle_branch(&txids, pos) {
                    let entry = txcache::CachedTx {
                        height: rec.height,
                        tx_pos: pos as u32,
                        branch,
                        raw: raw.clone(),
                    };
                    match cache.put(txid, &entry) {
                        Ok(()) => {
                            self.stats.cache_puts.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) => eprintln!("[e-lite] tx cache put failed: {}", e.kind()),
                    }
                }
            }
            out.push(hex::encode(raw));
        }
        Ok(out)
    }

    /// A block fully verified against the indexed header (hash, header,
    /// merkle root, BIP141 commitment), from the cheapest source that has it.
    fn verified_block(
        &self,
        rec: &el_model::BlockRecord,
        priority: Priority,
    ) -> Result<(Arc<bitcoin::Block>, Origin)> {
        let height = rec.height;
        if let Some(block) = self.recent_block(&rec.hash) {
            self.stats.cache_hits.fetch_add(1, Ordering::Relaxed);
            return Ok((block, Origin::Recent));
        }
        // Legacy whole-block cache (and repair blocks).
        if let Some(cache) = &self.cache {
            if let Some(bytes) = cache.get(&rec.hash) {
                match verify_full(&bytes, rec) {
                    Ok(block) => {
                        self.stats.cache_hits.fetch_add(1, Ordering::Relaxed);
                        return Ok((self.remember(rec.hash, block), Origin::BlockCache));
                    }
                    Err(e) => {
                        self.stats.verify_failures.fetch_add(1, Ordering::Relaxed);
                        eprintln!(
                            "[e-lite] cached block at height {height} rejected ({})",
                            e.kind()
                        );
                        let _ = cache.remove(&rec.hash);
                    }
                }
            }
        }
        // Local Core: zero external requests.
        if let Some(local) = &self.local {
            if let Ok(bytes) = local.fetch_block(&rec.hash) {
                match verify_full(&bytes, rec) {
                    Ok(block) => return Ok((Arc::new(block), Origin::Local)),
                    Err(e) => {
                        self.stats.verify_failures.fetch_add(1, Ordering::Relaxed);
                        eprintln!("[e-lite] local rawtx rejected ({})", e.kind());
                    }
                }
            }
        }
        if self.sources().is_empty() {
            self.stats.unavailable.fetch_add(1, Ordering::Relaxed);
            return Err(RawtxError::NoSource(
                "no external source configured (or Tor is required and unavailable)".to_string(),
            ));
        }
        // The same block failed a moment ago (a wallet retrying it): answer
        // with that failure instead of asking every source again (AUD-158).
        if let Some(e) = self.recent_failure(&rec.hash) {
            return Err(e);
        }
        let bytes = match self.fetch_external_block(&rec.hash, priority) {
            Ok(bytes) => bytes,
            Err(e) => {
                eprintln!(
                    "[e-lite] rawtx at height {height} failed ({}); sources: {}",
                    e.kind(),
                    redact_hashes(&e.to_string())
                );
                return Err(e);
            }
        };
        match verify_full(&bytes, rec) {
            Ok(block) => {
                self.owe_decoys(rec.hash);
                Ok((self.remember(rec.hash, block), Origin::External))
            }
            Err(e) => {
                self.stats.verify_failures.fetch_add(1, Ordering::Relaxed);
                eprintln!(
                    "[e-lite] external block at height {height} rejected ({})",
                    e.kind()
                );
                Err(e)
            }
        }
    }

    /// Recently fetched external blocks stay in memory so a wallet asking for
    /// several transactions of one block does not download it again.
    fn recent_block(&self, hash: &Hash32) -> Option<Arc<bitcoin::Block>> {
        let recent = self.recent.lock().ok()?;
        recent
            .iter()
            .find(|(h, _)| h == hash)
            .map(|(_, b)| Arc::clone(b))
    }

    fn remember(&self, hash: Hash32, block: bitcoin::Block) -> Arc<bitcoin::Block> {
        let block = Arc::new(block);
        if let Ok(mut recent) = self.recent.lock() {
            recent.retain(|(h, _)| *h != hash);
            recent.push_back((hash, Arc::clone(&block)));
            while recent.len() > RECENT_BLOCKS {
                recent.pop_front();
            }
        }
        block
    }

    /// Fetch one block from the external sources, deduplicating concurrent
    /// requests for the same hash. The first caller downloads; the rest wait
    /// for its result instead of hammering the same provider.
    fn fetch_external_block(&self, hash: &Hash32, priority: Priority) -> Result<Vec<u8>> {
        if let Some(cache) = &self.cache {
            if let Some(bytes) = cache.get(hash) {
                return Ok(bytes);
            }
        }
        let (entry, leader) = {
            let mut map = self
                .inflight
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match map.get(hash) {
                Some(existing) => (Arc::clone(existing), false),
                None => {
                    let entry = Arc::new(InflightBlock::new(Instant::now() + EXTERNAL_BUDGET));
                    map.insert(*hash, Arc::clone(&entry));
                    (entry, true)
                }
            }
        };

        if leader {
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                self.fetch_from_external_sources(hash, entry.deadline, priority)
            }))
            .unwrap_or_else(|_| {
                Err(RawtxError::Unavailable(
                    "external block fetch panicked".into(),
                ))
            });
            if let Err(e) = &result {
                self.remember_failure(hash, e);
            }
            *entry
                .result
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(result.clone());
            *entry
                .done
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
            entry.cv.notify_all();
            self.inflight
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(hash);
            return result;
        }

        // The leader gives up at its own deadline; a follower waits a little
        // longer so it always receives the leader's result (AUD-156).
        let deadline = entry.deadline + FOLLOWER_GRACE;
        let mut done = entry
            .done
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while !*done {
            let now = Instant::now();
            if now >= deadline {
                return Err(RawtxError::Unavailable(
                    "inflight block download timed out".into(),
                ));
            }
            let (guard, _) = entry
                .cv
                .wait_timeout(done, deadline.saturating_duration_since(now))
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            done = guard;
        }
        drop(done);
        let result = entry
            .result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        result.unwrap_or_else(|| Err(RawtxError::Unavailable("inflight result lost".into())))
    }

    fn fetch_from_external_sources(
        &self,
        hash: &Hash32,
        deadline: Instant,
        priority: Priority,
    ) -> Result<Vec<u8>> {
        // A wallet request may take the last free slot; the prefetch leaves
        // it free. Neither waits past the point where no attempt would fit.
        let keep_free = match priority {
            Priority::Wallet => 0,
            Priority::Prefetch => 1,
        };
        let queue_deadline = deadline.checked_sub(MIN_ATTEMPT).unwrap_or(deadline);
        let Some(_permit) = self.external_slots.acquire_until(queue_deadline, keep_free) else {
            self.stats.unavailable.fetch_add(1, Ordering::Relaxed);
            return Err(RawtxError::Unavailable(
                "every download slot stayed busy until the deadline".into(),
            ));
        };
        let sources = self.sources();
        let mut order: Vec<usize> = (0..sources.len()).collect();
        order.shuffle(&mut thread_rng());
        let mut errors = Vec::new();
        let count = order.len();
        for (pos, idx) in order.into_iter().enumerate() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left < MIN_ATTEMPT {
                errors.push("external fetch deadline reached".to_string());
                break;
            }
            // One deadline for the whole download (AUD-156): every source
            // but the last leaves a third of what is left to the others.
            let source_deadline = if pos + 1 == count {
                deadline
            } else {
                Instant::now() + left * 2 / 3
            };
            let source = &sources[idx];
            match source.fetch_block_until(hash, source_deadline, FetchMode::Normal) {
                Ok(bytes) => {
                    self.stats.fetches.fetch_add(1, Ordering::Relaxed);
                    self.stats
                        .bytes
                        .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                    match verify_block_by_hash(&bytes, hash).and_then(|block| {
                        verify_witness_commitment(&block)?;
                        Ok(block)
                    }) {
                        // Not written to the block cache: the caller keeps the
                        // transactions it needs (AUD-139).
                        Ok(_) => return Ok(bytes),
                        Err(e) => {
                            self.stats.verify_failures.fetch_add(1, Ordering::Relaxed);
                            eprintln!(
                                "[e-lite] {} returned an invalid block ({})",
                                source.name(),
                                e.kind()
                            );
                            errors.push(format!("{}: {e}", source.name()));
                        }
                    }
                }
                Err(e) => errors.push(format!("{}: {e}", source.name())),
            }
        }
        self.stats.unavailable.fetch_add(1, Ordering::Relaxed);
        Err(RawtxError::Unavailable(errors.join("; ")))
    }

    /// A failure of `hash` younger than `FAILURE_MEMORY`, if any.
    fn recent_failure(&self, hash: &Hash32) -> Option<RawtxError> {
        let mut failed = self.failed.lock().ok()?;
        let now = Instant::now();
        failed.retain(|_, (at, _)| now.duration_since(*at) < FAILURE_MEMORY);
        failed.get(hash).map(|(_, e)| e.clone())
    }

    /// Remember that every source failed for `hash`. "No source" is not
    /// remembered: it is a configuration state, not a download failure.
    fn remember_failure(&self, hash: &Hash32, e: &RawtxError) {
        if !matches!(e, RawtxError::Unavailable(_)) {
            return;
        }
        if let Ok(mut failed) = self.failed.lock() {
            let now = Instant::now();
            failed.retain(|_, (at, _)| now.duration_since(*at) < FAILURE_MEMORY);
            failed.insert(*hash, (now, e.clone()));
        }
    }

    /// Decoys owed and not yet downloaded.
    pub fn decoys_pending(&self) -> usize {
        self.decoy_lane.pending()
    }

    fn cache_store(&self, hash: &Hash32, bytes: &[u8]) {
        if let Some(cache) = &self.cache {
            match cache.put(hash, bytes) {
                Ok(()) => {
                    self.stats.cache_puts.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => eprintln!("[e-lite] cache put failed: {e}"),
            }
        }
    }

    /// Owe `decoys` downloads of random indexed blocks for one real external
    /// download (wallet or prefetch), and start the decoy thread on first use.
    fn owe_decoys(&self, exclude: Hash32) {
        if self.decoys == 0 || self.sources().is_empty() {
            return;
        }
        let tip = match self.store.committed_tip() {
            Ok(Some(t)) => t.height,
            _ => return,
        };
        if tip == 0 {
            return;
        }
        let mut owed = Vec::new();
        for _ in 0..self.decoys {
            // Retry the random pick so decoys are distinct from the real block
            // and always exist in the index.
            for _ in 0..8 {
                let height = rand::random::<u32>() % (tip + 1);
                if let Ok(Some(r)) = self.store.get_block(height) {
                    if r.hash != exclude {
                        owed.push(r.hash);
                        break;
                    }
                }
            }
        }
        if owed.is_empty() {
            return;
        }
        if let Ok(mut queue) = self.decoy_lane.queue.lock() {
            for hash in owed {
                if queue.len() >= MAX_DECOY_DEBT {
                    break;
                }
                queue.push_back(hash);
            }
        }
        self.decoy_lane.cv.notify_all();
        self.start_decoy_thread();
    }

    /// One thread serves every owed decoy, one at a time, outside the
    /// wallet's download slots. It holds the sources but not the store, so a
    /// dropped resolver releases its database at once.
    fn start_decoy_thread(&self) {
        let lane = Arc::clone(&self.decoy_lane);
        let external = self.external.clone();
        let p2p = self.p2p.clone();
        let stats = Arc::clone(&self.stats);
        self.decoy_lane.started.call_once(move || {
            let spawned = std::thread::Builder::new()
                .name("el-decoys".to_string())
                .spawn(move || {
                    while let Some(hash) = lane.next() {
                        // Spread decoys over time so they do not form a burst
                        // next to the real download.
                        let jitter = Duration::from_millis(250 + rand::random::<u64>() % 1250);
                        if !lane.pause(jitter) {
                            return;
                        }
                        let mut sources = external.clone();
                        if let Some(p2p) = &p2p {
                            if p2p.peer_count() > 0 {
                                sources.push(Arc::clone(p2p) as Arc<dyn BlockSource>);
                            }
                        }
                        if sources.iter().any(|s| s.under_pressure()) {
                            // A provider asked us to slow down: keep the debt.
                            if !lane.pause(Duration::from_secs(5)) {
                                return;
                            }
                            continue;
                        }
                        sources.shuffle(&mut thread_rng());
                        let deadline = Instant::now() + EXTERNAL_BUDGET;
                        for source in &sources {
                            if source
                                .fetch_block_until(&hash, deadline, FetchMode::Quiet)
                                .is_ok()
                            {
                                stats.decoys.fetch_add(1, Ordering::Relaxed);
                                break;
                            }
                        }
                        lane.done();
                    }
                });
            if let Err(e) = spawned {
                eprintln!("[e-lite] decoy thread did not start: {e}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prefetch_leaves_a_download_slot_for_wallets() {
        let sem = Semaphore::new(2);
        let soon = || Instant::now() + Duration::from_millis(50);
        let wallet = sem.acquire_until(soon(), 0).expect("first slot");
        // One slot left: the prefetch may not take it, a wallet may.
        assert!(sem.acquire_until(soon(), 1).is_none());
        let second = sem.acquire_until(soon(), 0).expect("last slot");
        drop((wallet, second));
        assert!(sem.acquire_until(soon(), 1).is_some());
        // A single-slot pool never blocks the prefetch forever.
        let single = Semaphore::new(1);
        assert!(single.acquire_until(soon(), 1).is_some());
    }
}
