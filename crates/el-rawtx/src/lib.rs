//! Raw transaction resolution for confirmed history.
//!
//! Order: local Core (zero leak) -> validated disk cache -> external sources
//! (P2P and/or Esplora) fetched as **whole blocks**, verified against the local
//! header chain, then cached. External fetches must go through Tor unless the
//! operator explicitly allows clearnet.

pub mod cache;
pub mod core_source;
pub mod esplora;
pub mod p2p;
pub mod verify;

use el_model::{Chain, Hash32};
use el_store::Store;
use rand::seq::SliceRandom;
use rand::thread_rng;
use std::collections::HashMap;
use std::fmt;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
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
pub use core_source::CoreBlockSource;
pub use esplora::EsploraSource;
pub use p2p::P2pSource;
pub use verify::{extract_tx, verify_block, verify_block_by_hash, verify_witness_commitment};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawtxError {
    Unavailable(String),
    Verify(String),
    Config(String),
    Io(String),
}

impl fmt::Display for RawtxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RawtxError::Unavailable(m) => write!(f, "rawtx unavailable: {m}"),
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

/// A source that can return a **whole raw block** by hash.
pub trait BlockSource: Send + Sync {
    fn name(&self) -> &str;
    fn fetch_block(&self, hash: &Hash32) -> Result<Vec<u8>>;
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
    /// Timeout for a single Esplora block request.
    pub request_timeout_seconds: u64,
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
    pub chain: Chain,
}

/// A block download shared by concurrent callers asking for the same height.
struct InflightBlock {
    done: Mutex<bool>,
    result: Mutex<Option<Result<Vec<u8>>>>,
    cv: Condvar,
}

impl InflightBlock {
    fn new() -> Self {
        Self {
            done: Mutex::new(false),
            result: Mutex::new(None),
            cv: Condvar::new(),
        }
    }
}

/// Small counting semaphore: bounds total external downloads without pulling
/// in an async runtime.
struct Semaphore {
    permits: Mutex<usize>,
    cv: Condvar,
}

impl Semaphore {
    fn new(permits: usize) -> Arc<Self> {
        Arc::new(Self {
            permits: Mutex::new(permits.max(1)),
            cv: Condvar::new(),
        })
    }

    fn acquire(self: &Arc<Self>) -> Permit {
        let mut permits = self
            .permits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while *permits == 0 {
            permits = self
                .cv
                .wait(permits)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        *permits -= 1;
        Permit {
            sem: Arc::clone(self),
        }
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
        self.sem.cv.notify_one();
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
    cache: Option<BlockCache>,
    cache_dir: std::path::PathBuf,
    decoys: u32,
    stats: Arc<RawtxStats>,
    chain: Chain,
    inflight: Mutex<HashMap<Hash32, Arc<InflightBlock>>>,
    external_slots: Arc<Semaphore>,
    decoys_inflight: Arc<AtomicUsize>,
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
                        .with_stats(Arc::clone(&stats)),
                    ));
                }
            }
            if cfg.use_esplora && !cfg.esplora_urls.is_empty() {
                match EsploraSource::new(
                    cfg.esplora_urls.clone(),
                    tor.clone(),
                    cfg.allow_clearnet,
                    cfg.fallback_clearnet,
                    Duration::from_secs(cfg.request_timeout_seconds.max(1)),
                    Duration::from_secs(cfg.provider_cooldown_seconds.max(1)),
                ) {
                    Ok(s) => external.push(Arc::new(s)),
                    Err(e) => eprintln!("[e-lite] esplora disabled: {e}"),
                }
            }
        }

        let cache_dir = cfg
            .cache_dir
            .clone()
            .unwrap_or_else(|| store.path().join("rawtx-cache"));
        let cache = if cfg.cache_mib > 0 {
            Some(BlockCache::open(
                &cache_dir,
                cfg.cache_mib.saturating_mul(1024 * 1024),
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
            cache_dir,
            decoys: cfg.decoys,
            stats,
            chain: cfg.chain,
            inflight: Mutex::new(HashMap::new()),
            external_slots: Semaphore::new(cfg.max_parallel_fetches),
            decoys_inflight: Arc::new(AtomicUsize::new(0)),
        })
    }

    pub fn with_local(mut self, source: Arc<dyn BlockSource>) -> Self {
        self.local = Some(source);
        self
    }

    /// Inject an additional external source (used by tests and custom setups).
    pub fn with_external(mut self, source: Arc<dyn BlockSource>) -> Self {
        self.external.push(source);
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
    pub fn refresh_p2p_peers(&self, peers: Vec<String>) -> Option<usize> {
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

    /// Logical bytes stored in the rawtx cache.
    pub fn cache_size_bytes(&self) -> u64 {
        self.cache.as_ref().map(|c| c.size()).unwrap_or(0)
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
        self.raw_tx_inner(txid, height, true)
    }

    /// Background prefetch path: same lookup, but no privacy decoys.
    pub fn raw_tx_prefetch(&self, txid: Hash32, height: u32) -> Result<String> {
        self.raw_tx_inner(txid, height, false)
    }

    fn raw_tx_inner(&self, txid: Hash32, height: u32, allow_decoys: bool) -> Result<String> {
        let rec = self
            .store
            .get_block(height)?
            .ok_or_else(|| RawtxError::Unavailable(format!("block {height} not indexed")))?;

        // 1) Validated cache first: repeated requests for the same block are free.
        if let Some(cache) = &self.cache {
            if let Some(bytes) = cache.get(&rec.hash) {
                match self.verified_tx(&bytes, &rec, txid) {
                    Ok(hex) => {
                        self.stats.cache_hits.fetch_add(1, Ordering::Relaxed);
                        return Ok(hex);
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

        // 2) Local Core: zero external requests.
        if let Some(local) = &self.local {
            if let Ok(bytes) = local.fetch_block(&rec.hash) {
                match self.verified_tx(&bytes, &rec, txid) {
                    Ok(hex) => {
                        self.cache_store(&rec.hash, &bytes);
                        return Ok(hex);
                    }
                    Err(e) => {
                        self.stats.verify_failures.fetch_add(1, Ordering::Relaxed);
                        eprintln!("[e-lite] local rawtx rejected ({})", e.kind());
                    }
                }
            }
        }

        // 3) External sources, shared across concurrent callers.
        if self.sources().is_empty() {
            self.stats.unavailable.fetch_add(1, Ordering::Relaxed);
            return Err(RawtxError::Unavailable(
                "no external source configured (or Tor is required and unavailable)".to_string(),
            ));
        }
        let bytes = match self.fetch_external_block(&rec.hash) {
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
        match self.verified_tx(&bytes, &rec, txid) {
            Ok(hex) => {
                if allow_decoys {
                    self.spawn_decoys(rec.hash);
                }
                Ok(hex)
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

    /// Fetch one block from the external sources, deduplicating concurrent
    /// requests for the same hash. The first caller downloads; the rest wait
    /// for its result instead of hammering the same provider.
    fn fetch_external_block(&self, hash: &Hash32) -> Result<Vec<u8>> {
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
                    let entry = Arc::new(InflightBlock::new());
                    map.insert(*hash, Arc::clone(&entry));
                    (entry, true)
                }
            }
        };

        if leader {
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                self.fetch_from_external_sources(hash)
            }))
            .unwrap_or_else(|_| {
                Err(RawtxError::Unavailable(
                    "external block fetch panicked".into(),
                ))
            });
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

        let deadline = Instant::now() + Duration::from_secs(45);
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

    fn fetch_from_external_sources(&self, hash: &Hash32) -> Result<Vec<u8>> {
        let _permit = self.external_slots.acquire();
        let sources = self.sources();
        let mut order: Vec<usize> = (0..sources.len()).collect();
        order.shuffle(&mut thread_rng());
        let started = Instant::now();
        let mut errors = Vec::new();
        for idx in order {
            if started.elapsed() >= Duration::from_secs(30) {
                errors.push("external fetch deadline exceeded".to_string());
                break;
            }
            let source = &sources[idx];
            match source.fetch_block(hash) {
                Ok(bytes) => {
                    self.stats.fetches.fetch_add(1, Ordering::Relaxed);
                    self.stats
                        .bytes
                        .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                    match verify_block_by_hash(&bytes, hash).and_then(|block| {
                        verify_witness_commitment(&block)?;
                        Ok(block)
                    }) {
                        Ok(_) => {
                            self.cache_store(hash, &bytes);
                            return Ok(bytes);
                        }
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

    /// Number of decoy downloads currently in flight.
    pub fn decoys_inflight(&self) -> usize {
        self.decoys_inflight.load(Ordering::Relaxed)
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

    fn verified_tx(
        &self,
        bytes: &[u8],
        rec: &el_model::BlockRecord,
        txid: Hash32,
    ) -> Result<String> {
        let block = verify_block(bytes, &rec.hash, &rec.header)?;
        verify_witness_commitment(&block)?;
        let tx = extract_tx(&block, txid)?;
        Ok(hex::encode(bitcoin::consensus::serialize(&tx)))
    }

    /// Fetch decoys in the background so the caller is not delayed.
    fn spawn_decoys(&self, exclude: Hash32) {
        const MAX_DECOYS_INFLIGHT: usize = 2;
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
        for _ in 0..self.decoys {
            if self.decoys_inflight.fetch_add(1, Ordering::Relaxed) >= MAX_DECOYS_INFLIGHT {
                self.decoys_inflight.fetch_sub(1, Ordering::Relaxed);
                break;
            }
            // Retry the random pick so decoys are always distinct from the real
            // block and always exist in the index.
            let mut chosen = None;
            for _ in 0..8 {
                let height = rand::random::<u32>() % (tip + 1);
                if let Ok(Some(r)) = self.store.get_block(height) {
                    if r.hash != exclude {
                        chosen = Some(r.hash);
                        break;
                    }
                }
            }
            let Some(hash) = chosen else {
                self.decoys_inflight.fetch_sub(1, Ordering::Relaxed);
                continue;
            };
            let sources = self.sources();
            let stats = Arc::clone(&self.stats);
            let inflight = Arc::clone(&self.decoys_inflight);
            let slots = Arc::clone(&self.external_slots);
            std::thread::spawn(move || {
                let _permit = slots.acquire();
                let mut order: Vec<usize> = (0..sources.len()).collect();
                order.shuffle(&mut thread_rng());
                for idx in order {
                    if sources[idx].fetch_block(&hash).is_ok() {
                        stats.decoys.fetch_add(1, Ordering::Relaxed);
                        break;
                    }
                }
                inflight.fetch_sub(1, Ordering::Relaxed);
            });
        }
    }
}
