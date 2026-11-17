//! stats module (split from lib.rs in alpha.29.17.0).

use super::*;

/// Why a `blockchain.transaction.get` could not be answered. Counters only:
/// no txid, script or block hash is kept (AUD-132).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxGetFailure {
    /// Not in the index and unknown to Core (mempool included).
    NotFound,
    /// Confirmed, Core pruned the block, and no external source can be
    /// asked: none configured, or Tor is required and unavailable.
    NoSource,
    /// Confirmed, Core pruned the block, and every source that was asked
    /// failed (timeout, HTTP error, cooling down).
    ExternalFailed,
    /// A source answered with data that did not verify.
    VerifyFailed,
    /// Store, cache or Core decode error.
    Internal,
}

impl TxGetFailure {
    pub const ALL: [TxGetFailure; 5] = [
        TxGetFailure::NotFound,
        TxGetFailure::NoSource,
        TxGetFailure::ExternalFailed,
        TxGetFailure::VerifyFailed,
        TxGetFailure::Internal,
    ];

    pub fn label(self) -> &'static str {
        match self {
            TxGetFailure::NotFound => "not_found",
            TxGetFailure::NoSource => "no_source",
            TxGetFailure::ExternalFailed => "external_failed",
            TxGetFailure::VerifyFailed => "verify_failed",
            TxGetFailure::Internal => "internal",
        }
    }

    pub(crate) fn of(error: &el_rawtx::RawtxError) -> Self {
        use el_rawtx::RawtxError;
        match error {
            RawtxError::NoSource(_) => TxGetFailure::NoSource,
            RawtxError::Unavailable(_) => TxGetFailure::ExternalFailed,
            RawtxError::Verify(_) => TxGetFailure::VerifyFailed,
            RawtxError::Config(_) | RawtxError::Io(_) => TxGetFailure::Internal,
        }
    }
}

#[derive(Default)]
pub struct TxGetFailures([AtomicU64; 5]);

impl TxGetFailures {
    pub(crate) fn add(&self, cause: TxGetFailure) -> u64 {
        self.0[cause as usize].fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn get(&self, cause: TxGetFailure) -> u64 {
        self.0[cause as usize].load(Ordering::Relaxed)
    }

    pub fn to_json(&self) -> Value {
        Value::Object(
            TxGetFailure::ALL
                .iter()
                .map(|c| (c.label().to_string(), json!(self.get(*c))))
                .collect(),
        )
    }
}

/// Counters taken when a connection opens, so the `debug:` wallet lines can
/// report what *that* session did: request counts alone cannot tell a cold
/// run (external fetches) from a warm one (cache hits) when the process has
/// been up for hours.
#[derive(Clone, Copy, Debug, Default)]
pub struct CacheBaseline {
    hits: u64,
    puts: u64,
    fetches: u64,
    failures: [u64; 5],
}

/// Difference between two [`CacheBaseline`] snapshots.
#[derive(Clone, Copy, Debug, Default)]
pub struct CacheDelta {
    pub hits: u64,
    pub puts: u64,
    pub fetches: u64,
    failures: [u64; 5],
}

impl CacheDelta {
    /// `none` or `external_failed 2 no_source 1`: causes only, never hashes.
    pub fn failures_text(&self) -> String {
        let parts: Vec<String> = TxGetFailure::ALL
            .iter()
            .enumerate()
            .filter(|(i, _)| self.failures[*i] > 0)
            .map(|(i, cause)| format!("{} {}", cause.label(), self.failures[i]))
            .collect();
        if parts.is_empty() {
            "none".to_string()
        } else {
            parts.join(" ")
        }
    }
}

impl CacheBaseline {
    pub fn capture(shared: &Shared) -> Self {
        let rawtx = shared.resolver.stats();
        CacheBaseline {
            hits: rawtx.cache_hits.load(Ordering::Relaxed),
            puts: rawtx.cache_puts.load(Ordering::Relaxed),
            fetches: rawtx.fetches.load(Ordering::Relaxed),
            failures: TxGetFailure::ALL.map(|c| shared.stats.tx_get_failures.get(c)),
        }
    }

    pub fn since(self, shared: &Shared) -> CacheDelta {
        let now = CacheBaseline::capture(shared);
        CacheDelta {
            hits: now.hits.saturating_sub(self.hits),
            puts: now.puts.saturating_sub(self.puts),
            fetches: now.fetches.saturating_sub(self.fetches),
            failures: std::array::from_fn(|i| now.failures[i].saturating_sub(self.failures[i])),
        }
    }
}

pub struct Stats {
    pub indexed_height: AtomicU64,
    pub core_height: AtomicU64,
    pub core_headers: AtomicU64,
    pub core_verification_bps: AtomicU64,
    pub core_pruneheight: AtomicU64,
    pub core_pruned: AtomicBool,
    pub core_network_active: AtomicBool,
    pub core_ibd: AtomicBool,
    pub core_connections: AtomicU64,
    pub txnum: AtomicU64,
    pub requests_total: AtomicU64,
    pub errors_total: AtomicU64,
    pub rate_limited: AtomicU64,
    /// `blockchain.transaction.get` failures by cause.
    pub tx_get_failures: TxGetFailures,
    /// Lines read from clients (valid or not), so `lines_total - batch_lines`
    /// is the number of single-request lines.
    pub lines_total: AtomicU64,
    /// Lines that carried a JSON-RPC batch array, and the items they held.
    pub batch_lines: AtomicU64,
    pub batch_items: AtomicU64,
    pub batch_max_items: AtomicU64,
    pub clients: AtomicUsize,
    /// Connections whose client left while lines were still running: they
    /// no longer hold a `max_clients` slot. Bounded by `max_clients`.
    pub detached_clients: AtomicUsize,
    pub last_block_ms: AtomicU64,
    pub notifier_runs: AtomicU64,
    pub notifications_sent: AtomicU64,
    pub notifier_errors: AtomicU64,
    pub blocks_applied: AtomicU64,
    /// Commit groups completed and transactions committed (cumulative).
    pub commits: AtomicU64,
    pub committed_txs: AtomicU64,
    /// Cumulative milliseconds spent in each indexing stage.
    pub stage_read_us: AtomicU64,
    pub stage_decode_us: AtomicU64,
    pub stage_prefetch_us: AtomicU64,
    pub stage_wait_us: AtomicU64,
    pub stage_apply_us: AtomicU64,
    pub stage_commit_us: AtomicU64,
    pub commit_txids_us: AtomicU64,
    pub commit_db_us: AtomicU64,
    /// RocksDB state sampled at commit time (benchmark/diagnostics).
    pub rocksdb_pending_compaction_bytes: AtomicU64,
    pub rocksdb_running_compactions: AtomicU64,
    pub rocksdb_memtable_bytes: AtomicU64,
    pub rocksdb_block_cache_bytes: AtomicU64,
    pub rocksdb_sst_bytes: AtomicU64,
    pub rocksdb_table_readers_bytes: AtomicU64,
    /// Summed level-0 file count across column families (flush/compaction
    /// stall signal). Sampled at commit time.
    pub rocksdb_level0_files: AtomicU64,
    pub rocksdb_properties_ok: AtomicBool,
    /// Descriptor budget: requested/effective max_open_files and the process
    /// soft limit. Zero means "not measured".
    pub fd_soft_limit: AtomicU64,
    pub fd_reserve: AtomicU64,
    pub max_open_files_requested: AtomicU64,
    pub max_open_files_effective: AtomicU64,
    pub prune_requests: AtomicU64,
    pub prune_failures: AtomicU64,
    pub disk_low: AtomicBool,
    pub tuning_readers: AtomicU64,
    pub tuning_commit_ms: AtomicU64,
    pub tuning_batch_blocks: AtomicU64,
    pub tuning_cache_mib: AtomicU64,
    pub tuning_read_ms: AtomicU64,
    pub tuning_apply_ms: AtomicU64,
    pub tuning_decode_ms: AtomicU64,
    pub tuning_wait_ms: AtomicU64,
    pub tuning_prefetch_p50_ms: AtomicU64,
    pub tuning_prefetch_p95_ms: AtomicU64,
    pub tuning_prefetch_max_ms: AtomicU64,
    pub tuning_wait_p50_ms: AtomicU64,
    pub tuning_wait_p95_ms: AtomicU64,
    pub tuning_commit_ms_ewma: AtomicU64,
    pub tuning_commit_per_block_ms: AtomicU64,
    /// Wall-clock between the start of consecutive commits. `commit_share`
    /// must divide by this, not by the configured interval.
    pub tuning_commit_cycle_ms: AtomicU64,
    /// Last full commit group: duration, cycle, blocks, reason and cost split.
    pub tuning_commit_last_ms: AtomicU64,
    pub tuning_commit_last_cycle_ms: AtomicU64,
    pub tuning_commit_last_blocks: AtomicU64,
    pub tuning_commit_last_txs: AtomicU64,
    pub tuning_commit_last_bytes: AtomicU64,
    pub tuning_commit_last_txids_us: AtomicU64,
    pub tuning_commit_last_db_us: AtomicU64,
    pub tuning_commit_last_reason: AtomicU64,
    pub tuning_adapt_steps: AtomicU64,
    pub tuning_tx_per_sec: AtomicU64,
    pub tuning_blocks_per_sec: AtomicU64,
    pub tuning_profile: AtomicU64,
    pub tuning_adaptive: AtomicBool,
    pub tuning_background_jobs: AtomicU64,
    pub tls_handshakes: AtomicU64,
    pub tls_handshake_failures: AtomicU64,
    pub mem_total_mib: AtomicU64,
    pub mem_available_mib: AtomicU64,
    pub mem_cgroup_limit_mib: AtomicU64,
    pub mem_cgroup_used_mib: AtomicU64,
    pub mem_reclaimable_mib: AtomicU64,
    pub mem_cached_mib: AtomicU64,
    pub mem_cache_max_mib: AtomicU64,
    pub mem_memtables_mib: AtomicU64,
    pub mem_memtable_bytes_mib: AtomicU64,
    pub mem_table_readers_mib: AtomicU64,
    pub mem_effective_mib: AtomicU64,
    pub mem_dynamic_reserve_mib: AtomicU64,
    pub mem_reserve_mib: AtomicU64,
    pub mem_budget_mib: AtomicU64,
    pub tor_bootstrapped: AtomicBool,
    /// The node runs under e-lite's own supervisor, how many times it was
    /// restarted and why the previous node ended.
    pub supervised: AtomicBool,
    pub restarts: AtomicU64,
    pub restart_reason: Mutex<String>,
    /// Managed Tor: effective loopback SOCKS port (0 = unknown/not managed)
    /// and whether a control port exists (e-lite does not create one).
    pub tor_managed: AtomicBool,
    pub tor_socks_port: AtomicU64,
    pub tor_control_port: AtomicU64,
    /// Pipeline work accounting: blocks prepared, applied and discarded when
    /// a catch-up call ends. Discarded should stay near zero.
    pub pipeline_prepared: AtomicU64,
    pub pipeline_applied: AtomicU64,
    pub pipeline_discarded: AtomicU64,
    /// Core/e-lite scheduler: backlog bytes and high/low watermarks.
    pub scheduler_alternate: AtomicBool,
    pub scheduler_index_paused: AtomicBool,
    pub scheduler_backlog_bytes: AtomicU64,
    pub scheduler_high_bytes: AtomicU64,
    pub scheduler_low_bytes: AtomicU64,
    pub scheduler_avg_block_bytes: AtomicU64,
    pub scheduler_phase: AtomicU64,
    pub scheduler_phase_blocks: AtomicU64,
    pub scheduler_phase_elapsed_ms: AtomicU64,
    /// Machine plan detected at startup (CPU/RAM/disk -> knobs).
    pub plan: Mutex<serde_json::Value>,
    /// Per-column-family RocksDB pressure, published at commit time.
    pub rocksdb_cf: Mutex<serde_json::Value>,
    pub started: Instant,
}

impl Default for Stats {
    fn default() -> Self {
        Stats {
            indexed_height: AtomicU64::new(0),
            core_height: AtomicU64::new(0),
            core_headers: AtomicU64::new(0),
            core_verification_bps: AtomicU64::new(0),
            core_pruneheight: AtomicU64::new(0),
            core_pruned: AtomicBool::new(false),
            core_network_active: AtomicBool::new(true),
            core_ibd: AtomicBool::new(false),
            core_connections: AtomicU64::new(0),
            txnum: AtomicU64::new(0),
            requests_total: AtomicU64::new(0),
            errors_total: AtomicU64::new(0),
            rate_limited: AtomicU64::new(0),
            tx_get_failures: TxGetFailures::default(),
            lines_total: AtomicU64::new(0),
            batch_lines: AtomicU64::new(0),
            batch_items: AtomicU64::new(0),
            batch_max_items: AtomicU64::new(0),
            clients: AtomicUsize::new(0),
            detached_clients: AtomicUsize::new(0),
            last_block_ms: AtomicU64::new(0),
            notifier_runs: AtomicU64::new(0),
            notifications_sent: AtomicU64::new(0),
            notifier_errors: AtomicU64::new(0),
            blocks_applied: AtomicU64::new(0),
            commits: AtomicU64::new(0),
            committed_txs: AtomicU64::new(0),
            stage_read_us: AtomicU64::new(0),
            stage_decode_us: AtomicU64::new(0),
            stage_prefetch_us: AtomicU64::new(0),
            stage_wait_us: AtomicU64::new(0),
            stage_apply_us: AtomicU64::new(0),
            stage_commit_us: AtomicU64::new(0),
            commit_txids_us: AtomicU64::new(0),
            commit_db_us: AtomicU64::new(0),
            rocksdb_pending_compaction_bytes: AtomicU64::new(0),
            rocksdb_running_compactions: AtomicU64::new(0),
            rocksdb_memtable_bytes: AtomicU64::new(0),
            rocksdb_block_cache_bytes: AtomicU64::new(0),
            rocksdb_sst_bytes: AtomicU64::new(0),
            rocksdb_table_readers_bytes: AtomicU64::new(0),
            rocksdb_level0_files: AtomicU64::new(0),
            rocksdb_properties_ok: AtomicBool::new(false),
            fd_soft_limit: AtomicU64::new(0),
            fd_reserve: AtomicU64::new(0),
            max_open_files_requested: AtomicU64::new(0),
            max_open_files_effective: AtomicU64::new(0),
            prune_requests: AtomicU64::new(0),
            prune_failures: AtomicU64::new(0),
            disk_low: AtomicBool::new(false),
            tuning_readers: AtomicU64::new(0),
            tuning_commit_ms: AtomicU64::new(0),
            tuning_batch_blocks: AtomicU64::new(0),
            tuning_cache_mib: AtomicU64::new(0),
            tuning_read_ms: AtomicU64::new(0),
            tuning_apply_ms: AtomicU64::new(0),
            tuning_decode_ms: AtomicU64::new(0),
            tuning_wait_ms: AtomicU64::new(0),
            tuning_prefetch_p50_ms: AtomicU64::new(0),
            tuning_prefetch_p95_ms: AtomicU64::new(0),
            tuning_prefetch_max_ms: AtomicU64::new(0),
            tuning_wait_p50_ms: AtomicU64::new(0),
            tuning_wait_p95_ms: AtomicU64::new(0),
            tuning_commit_ms_ewma: AtomicU64::new(0),
            tuning_commit_per_block_ms: AtomicU64::new(0),
            tuning_commit_cycle_ms: AtomicU64::new(0),
            tuning_commit_last_ms: AtomicU64::new(0),
            tuning_commit_last_cycle_ms: AtomicU64::new(0),
            tuning_commit_last_blocks: AtomicU64::new(0),
            tuning_commit_last_txs: AtomicU64::new(0),
            tuning_commit_last_bytes: AtomicU64::new(0),
            tuning_commit_last_txids_us: AtomicU64::new(0),
            tuning_commit_last_db_us: AtomicU64::new(0),
            tuning_commit_last_reason: AtomicU64::new(0),
            tuning_adapt_steps: AtomicU64::new(0),
            tuning_tx_per_sec: AtomicU64::new(0),
            tuning_blocks_per_sec: AtomicU64::new(0),
            tuning_profile: AtomicU64::new(0),
            tuning_adaptive: AtomicBool::new(false),
            tuning_background_jobs: AtomicU64::new(0),
            tls_handshakes: AtomicU64::new(0),
            tls_handshake_failures: AtomicU64::new(0),
            mem_total_mib: AtomicU64::new(0),
            mem_available_mib: AtomicU64::new(0),
            mem_cgroup_limit_mib: AtomicU64::new(0),
            mem_cgroup_used_mib: AtomicU64::new(0),
            mem_reclaimable_mib: AtomicU64::new(0),
            mem_cached_mib: AtomicU64::new(0),
            mem_cache_max_mib: AtomicU64::new(0),
            mem_memtables_mib: AtomicU64::new(0),
            mem_memtable_bytes_mib: AtomicU64::new(0),
            mem_table_readers_mib: AtomicU64::new(0),
            mem_effective_mib: AtomicU64::new(0),
            mem_dynamic_reserve_mib: AtomicU64::new(0),
            mem_reserve_mib: AtomicU64::new(0),
            mem_budget_mib: AtomicU64::new(0),
            tor_bootstrapped: AtomicBool::new(false),
            supervised: AtomicBool::new(false),
            restarts: AtomicU64::new(0),
            restart_reason: Mutex::new(String::new()),
            tor_managed: AtomicBool::new(false),
            tor_socks_port: AtomicU64::new(0),
            tor_control_port: AtomicU64::new(0),
            pipeline_prepared: AtomicU64::new(0),
            pipeline_applied: AtomicU64::new(0),
            pipeline_discarded: AtomicU64::new(0),
            scheduler_alternate: AtomicBool::new(false),
            scheduler_index_paused: AtomicBool::new(false),
            scheduler_backlog_bytes: AtomicU64::new(0),
            scheduler_high_bytes: AtomicU64::new(0),
            scheduler_low_bytes: AtomicU64::new(0),
            scheduler_avg_block_bytes: AtomicU64::new(0),
            scheduler_phase: AtomicU64::new(0),
            scheduler_phase_blocks: AtomicU64::new(0),
            scheduler_phase_elapsed_ms: AtomicU64::new(0),
            plan: Mutex::new(serde_json::json!({})),
            rocksdb_cf: Mutex::new(serde_json::json!({})),
            started: Instant::now(),
        }
    }
}

/// Cached header hashes for checkpoint proofs. Header hashes are immutable
/// except across a reorg, which invalidates the cache from the fork point.
#[derive(Default)]
pub struct HeaderCache {
    hashes: Mutex<Vec<Hash32>>,
}

impl HeaderCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn invalidate_from(&self, height: u32) {
        if let Ok(mut v) = self.hashes.lock() {
            v.truncate(height as usize);
        }
    }

    pub fn chain(&self, store: &Store, cp_height: u32) -> Result<Vec<Hash32>, String> {
        let need = cp_height as usize + 1;
        let mut v = self
            .hashes
            .lock()
            .map_err(|_| "header cache lock poisoned".to_string())?;
        while v.len() < need {
            let h = v.len() as u32;
            let rec = store
                .get_block(h)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("height {h} not indexed"))?;
            v.push(rec.hash);
        }
        Ok(v[..need].to_vec())
    }
}

#[derive(Default)]
pub struct DbBytesCache {
    pub(crate) bytes: u64,
    pub(crate) at: Option<Instant>,
}

/// Scripts whose status changed since the notifier last ran. The indexer marks
/// confirmed changes and the mempool marks overlay changes, so the notifier
/// only recomputes the affected subscriptions.
#[derive(Default)]
pub struct DirtyScripts {
    inner: Mutex<HashSet<ScriptHash>>,
    force_all: AtomicBool,
    /// Wakes the notifier as soon as something is marked, instead of a
    /// fixed 2 s poll (AUD-146).
    changed: Condvar,
}

impl DirtyScripts {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn mark(&self, script: ScriptHash) {
        if let Ok(mut set) = self.inner.lock() {
            set.insert(script);
        }
        self.changed.notify_all();
    }

    pub fn mark_all(&self, scripts: impl IntoIterator<Item = ScriptHash>) {
        if let Ok(mut set) = self.inner.lock() {
            set.extend(scripts);
        }
        self.changed.notify_all();
    }

    /// A reorg can change scripts that are not in the replayed blocks' dirty
    /// set yet (for example, while the index is between rewind and replay).
    pub fn mark_all_subscriptions(&self) {
        self.force_all.store(true, Ordering::SeqCst);
        self.changed.notify_all();
    }

    /// Wait until something is marked, or `timeout`. Returns at once when
    /// marks are already pending.
    pub fn wait(&self, timeout: Duration) {
        let Ok(set) = self.inner.lock() else {
            std::thread::sleep(timeout);
            return;
        };
        if !set.is_empty() || self.force_all.load(Ordering::SeqCst) {
            return;
        }
        let _ = self.changed.wait_timeout(set, timeout);
    }

    pub fn take(&self) -> (HashSet<ScriptHash>, bool) {
        let force = self.force_all.swap(false, Ordering::SeqCst);
        let scripts = self
            .inner
            .lock()
            .map(|mut set| std::mem::take(&mut *set))
            .unwrap_or_default();
        (scripts, force)
    }
}
