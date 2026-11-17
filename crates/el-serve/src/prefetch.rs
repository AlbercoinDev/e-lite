//! prefetch module (split from lib.rs in alpha.29.17.0).

use super::*;

/// Background block prefetcher for confirmed history entries.
///
/// When a wallet asks for a script history, e-lite already knows the heights
/// (and one txid per height). Fetching those blocks in the background means the
/// subsequent `blockchain.transaction.get` calls hit the block cache instead of
/// waiting for a provider. Deduplicated by height, bounded queue, and only used
/// when external sources exist.
/// Default history blocks queued per response when the configuration leaves
/// `rawtx.prefetch_history_blocks = 0` (block privacy mode). The wallet asks
/// for these transactions right after the history; starting them now is what
/// makes a cold load over Tor fast (AUD-160).
pub const PREFETCH_PER_HISTORY: usize = 256;
/// Prefetch queue length.
pub const PREFETCH_QUEUE: usize = 1024;

/// One queued prefetch: a block and the history transactions it contains.
pub(crate) type PrefetchJob = (Hash32, u32, Vec<Hash32>);

pub struct Prefetcher {
    tx: Option<mpsc::SyncSender<PrefetchJob>>,
    seen: Arc<Mutex<HashSet<Hash32>>>,
    queued: Arc<AtomicU64>,
    done: Arc<AtomicU64>,
    /// Blocks queued per history response (0 = disabled, on demand only).
    per_history: usize,
}

impl Prefetcher {
    pub fn disabled() -> Self {
        Self {
            tx: None,
            seen: Arc::new(Mutex::new(HashSet::new())),
            queued: Arc::new(AtomicU64::new(0)),
            done: Arc::new(AtomicU64::new(0)),
            per_history: 0,
        }
    }

    pub fn new(
        resolver: Arc<Resolver>,
        _readers: Arc<ReaderTracker>,
        workers: usize,
        queue: usize,
        per_history: usize,
    ) -> Self {
        let (tx, rx) = mpsc::sync_channel::<PrefetchJob>(queue.max(1));
        let rx = Arc::new(Mutex::new(rx));
        let done = Arc::new(AtomicU64::new(0));
        let seen = Arc::new(Mutex::new(HashSet::new()));
        for _ in 0..workers.max(1) {
            let resolver = Arc::clone(&resolver);
            let rx = Arc::clone(&rx);
            let done = Arc::clone(&done);
            let seen = Arc::clone(&seen);
            std::thread::spawn(move || loop {
                let item = {
                    let guard = match rx.lock() {
                        Ok(g) => g,
                        Err(_) => return,
                    };
                    guard.recv()
                };
                let Ok((block_hash, height, txids)) = item else {
                    return;
                };
                // No reader guard: prefetch is an optimisation and may resolve
                // from external sources. It must never block pruning. All the
                // history transactions of the block are cached from a single
                // download (AUD-139); an external download owes decoys and
                // leaves a download slot free for wallet requests (AUD-159).
                if resolver.prefetch_block_txs(height, &txids).is_err() {
                    // Allow a later history request to retry this block.
                    if let Ok(mut seen) = seen.lock() {
                        seen.remove(&block_hash);
                    }
                }
                done.fetch_add(1, Ordering::Relaxed);
            });
        }
        Self {
            tx: Some(tx),
            seen: Arc::clone(&seen),
            queued: Arc::new(AtomicU64::new(0)),
            done,
            per_history,
        }
    }

    /// (queued, completed) background prefetches.
    pub fn stats(&self) -> (u64, u64) {
        (
            self.queued.load(Ordering::Relaxed),
            self.done.load(Ordering::Relaxed),
        )
    }

    /// Blocks queued per history response; 0 means on demand only.
    pub fn per_history(&self) -> usize {
        self.per_history
    }

    /// True when this block is already queued, prefetched or being fetched.
    /// A transaction request enqueues a block only once; the download itself
    /// deduplicates by hash.
    pub fn wants(&self, hash: &Hash32) -> bool {
        if self.tx.is_none() {
            return true;
        }
        match self.seen.lock() {
            Ok(seen) => seen.contains(hash),
            Err(_) => true,
        }
    }

    /// Enqueue one block with the history transactions it contains.
    /// Duplicates and a full queue are dropped silently: prefetch is an
    /// optimisation, never a correctness path.
    pub fn enqueue(&self, block_hash: Hash32, height: u32, txids: Vec<Hash32>) -> bool {
        let Some(tx) = &self.tx else {
            return false;
        };
        let Ok(mut seen) = self.seen.lock() else {
            return false;
        };
        if seen.len() >= 20_000 {
            seen.clear();
        }
        if !seen.insert(block_hash) {
            return false;
        }
        if tx.try_send((block_hash, height, txids)).is_err() {
            // Queue full or worker gone: do not mark it as done.
            seen.remove(&block_hash);
            return false;
        }
        self.queued.fetch_add(1, Ordering::Relaxed);
        true
    }
}
