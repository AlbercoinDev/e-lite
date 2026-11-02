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
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub use cache::BlockCache;
pub use core_source::CoreBlockSource;
pub use esplora::EsploraSource;
pub use p2p::P2pSource;
pub use verify::{extract_tx, verify_block, verify_witness_commitment};

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
}

#[derive(Clone, Debug)]
pub struct RawtxConfig {
    /// When false, only the local Core block source and the disk cache are used.
    pub enabled: bool,
    pub cache_mib: u64,
    pub decoys: u32,
    pub tor_socks: Option<String>,
    pub allow_clearnet: bool,
    pub use_p2p: bool,
    pub use_esplora: bool,
    pub p2p_v2: bool,
    pub esplora_urls: Vec<String>,
    pub p2p_peers: Vec<String>,
    pub p2p_timeout: Duration,
    pub chain: Chain,
}

pub struct Resolver {
    store: Arc<Store>,
    /// Local source, tried first and never cached.
    local: Option<Arc<dyn BlockSource>>,
    /// External sources, tried in random order.
    external: Vec<Arc<dyn BlockSource>>,
    cache: Option<BlockCache>,
    decoys: u32,
    stats: Arc<RawtxStats>,
    chain: Chain,
}

impl Resolver {
    pub fn new(store: Arc<Store>, cfg: &RawtxConfig) -> Result<Self> {
        let mut external: Vec<Arc<dyn BlockSource>> = Vec::new();
        let tor = cfg.tor_socks.clone();

        if cfg.enabled {
            if cfg.use_p2p && !cfg.p2p_peers.is_empty() {
                if tor.is_none() && !cfg.allow_clearnet {
                    // fail closed: no external P2P without Tor
                } else {
                    external.push(Arc::new(P2pSource::new(
                        cfg.p2p_peers.clone(),
                        tor.clone(),
                        cfg.p2p_timeout,
                        cfg.chain,
                        cfg.p2p_v2,
                    )));
                }
            }
            if cfg.use_esplora && !cfg.esplora_urls.is_empty() {
                match EsploraSource::new(
                    cfg.esplora_urls.clone(),
                    tor.clone(),
                    cfg.allow_clearnet,
                    Duration::from_secs(60),
                ) {
                    Ok(s) => external.push(Arc::new(s)),
                    Err(e) => eprintln!("[e-lite] esplora disabled: {e}"),
                }
            }
        }

        let cache = if cfg.cache_mib > 0 {
            Some(BlockCache::open(
                &store.path().join("rawtx-cache"),
                cfg.cache_mib.saturating_mul(1024 * 1024),
            )?)
        } else {
            None
        };

        Ok(Resolver {
            store,
            local: None,
            external,
            cache,
            decoys: cfg.decoys,
            stats: Arc::new(RawtxStats::default()),
            chain: cfg.chain,
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
        self.external.len()
    }

    pub fn chain(&self) -> Chain {
        self.chain
    }

    /// Resolve a confirmed transaction's raw hex at `height`.
    pub fn raw_tx(&self, txid: Hash32, height: u32) -> Result<String> {
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
                        eprintln!("[e-lite] cached block {} rejected: {e}", rec.hash);
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
                        eprintln!("[e-lite] local rawtx rejected: {e}");
                    }
                }
            }
        }

        // 3) External sources, random order.
        if self.external.is_empty() {
            self.stats.unavailable.fetch_add(1, Ordering::Relaxed);
            return Err(RawtxError::Unavailable(
                "no external source configured (or Tor is required and unavailable)".to_string(),
            ));
        }
        let mut order: Vec<usize> = (0..self.external.len()).collect();
        order.shuffle(&mut thread_rng());
        let mut errors = Vec::new();
        for idx in order {
            let source = &self.external[idx];
            match source.fetch_block(&rec.hash) {
                Ok(bytes) => {
                    self.stats.fetches.fetch_add(1, Ordering::Relaxed);
                    self.stats
                        .bytes
                        .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                    match self.verified_tx(&bytes, &rec, txid) {
                        Ok(hex) => {
                            self.cache_store(&rec.hash, &bytes);
                            self.spawn_decoys(rec.hash);
                            return Ok(hex);
                        }
                        Err(e) => {
                            self.stats.verify_failures.fetch_add(1, Ordering::Relaxed);
                            eprintln!("[e-lite] {} returned an invalid block: {e}", source.name());
                        }
                    }
                }
                Err(e) => errors.push(format!("{}: {e}", source.name())),
            }
        }
        self.stats.unavailable.fetch_add(1, Ordering::Relaxed);
        Err(RawtxError::Unavailable(errors.join("; ")))
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
        if self.decoys == 0 || self.external.is_empty() {
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
            let Some(hash) = chosen else { continue };
            let sources = self.external.clone();
            let stats = Arc::clone(&self.stats);
            std::thread::spawn(move || {
                let mut order: Vec<usize> = (0..sources.len()).collect();
                order.shuffle(&mut thread_rng());
                for idx in order {
                    if sources[idx].fetch_block(&hash).is_ok() {
                        stats.decoys.fetch_add(1, Ordering::Relaxed);
                        return;
                    }
                }
            });
        }
    }
}
