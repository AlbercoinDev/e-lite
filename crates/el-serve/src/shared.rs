//! shared module (split from lib.rs in alpha.29.17.0).

use super::*;

pub struct Shared {
    pub store: Arc<Store>,
    pub core: CoreClient,
    pub resolver: Arc<Resolver>,
    /// Effective public Esplora bases for this network (from public-servers.toml
    /// or the [rawtx] override). Empty means no Esplora probe is possible.
    pub public_servers: Vec<String>,
    /// Electrum servers of the per-txid path (AUD-163), resolved at startup.
    /// Zero with a non-empty Esplora list means the block path only.
    pub tx_sources: usize,
    /// Transport of the external path: `tor`, `tor+fallback` or `direct`
    /// (`rawtx.use_tor = false`, AUD-168).
    pub rawtx_path: String,
    /// Startup probe results, one JSON object per provider.
    pub provider_status: Mutex<Vec<Value>>,
    pub provider_ready: AtomicBool,
    pub tor_ready: AtomicBool,
    pub clearnet_fallback: AtomicBool,
    pub prefetch: Prefetcher,
    pub mempool: Arc<RwLock<Mempool>>,
    pub relayfee: Mutex<f64>,
    pub fee_estimates: Mutex<FeeEstimateCache>,
    pub error_log: Mutex<HashMap<String, Instant>>,
    pub clients: Arc<ClientRegistry>,
    pub readers: Arc<ReaderTracker>,
    pub silent_payments_enabled: bool,
    pub identity: RwLock<NetworkIdentity>,
    pub headers: HeaderCache,
    pub db_bytes: Mutex<DbBytesCache>,
    pub dirty_scripts: DirtyScripts,
    pub data_dir: std::path::PathBuf,
    pub core_datadir: std::path::PathBuf,
    pub log_file: PathBuf,
    pub listen: String,
    pub tls_listen: Option<String>,
    pub onion: Mutex<Option<String>>,
    pub onion_plain_port: u16,
    pub onion_tls_port: u16,
    pub banner_file: Option<PathBuf>,
    pub max_clients: usize,
    pub state: AtomicU8,
    pub shutdown: Arc<AtomicBool>,
    pub lock: Arc<RwLock<()>>,
    pub stats: Stats,
}

/// Coherent readiness view shared by the banner, `/status` and logs.
#[derive(Debug, Clone, Copy, Default)]
pub struct SyncStatus {
    pub ready: bool,
    pub at_core_tip: bool,
    pub fully_synced: bool,
    pub offline: bool,
    pub height: u64,
    pub headers: u64,
    pub core_height: u64,
    pub pruneheight: u64,
    pub core_ibd: bool,
    pub verification: f64,
    pub lag: u64,
    pub percent: f64,
    pub core_percent: f64,
    pub blocks_per_sec: u64,
}

impl Shared {
    pub fn sync_status(&self) -> SyncStatus {
        let height = self
            .store
            .committed_tip()
            .ok()
            .flatten()
            .map(|t| t.height as u64)
            .unwrap_or_else(|| self.stats.indexed_height.load(Ordering::Relaxed));
        let headers = self.stats.core_headers.load(Ordering::Relaxed);
        let core_height = self.stats.core_height.load(Ordering::Relaxed);
        let core_ibd = self.stats.core_ibd.load(Ordering::Relaxed);
        let verification =
            self.stats.core_verification_bps.load(Ordering::Relaxed) as f64 / 10_000.0;
        let offline = self.get_state() == state::CORE_UNAVAILABLE;
        // `height + 1 >= core_height` would call an index one block behind
        // "at tip". Require the exact tip, and full sync also requires Core's
        // headers to be at the same height.
        let at_core_tip = core_height > 0 && height == core_height;
        let fully_synced =
            !core_ibd && core_height > 0 && height == core_height && headers == core_height;
        // `ready` used to fire while Core was still in IBD whenever the index
        // briefly matched Core's short tip ("READY ... Core still in IBD").
        // Wallets are only served with a stable tip when Core itself is done.
        let ready = (at_core_tip && !core_ibd) || (offline && height > 0);
        let pct = |value: u64, total: u64| {
            if total == 0 {
                0.0
            } else {
                (value as f64 / total as f64 * 100.0).min(100.0)
            }
        };
        SyncStatus {
            ready,
            at_core_tip,
            fully_synced,
            offline,
            height,
            headers,
            core_height,
            pruneheight: self.stats.core_pruneheight.load(Ordering::Relaxed),
            core_ibd,
            verification,
            lag: core_height.saturating_sub(height),
            percent: pct(height, headers),
            core_percent: pct(core_height, headers),
            blocks_per_sec: self.stats.tuning_blocks_per_sec.load(Ordering::Relaxed),
        }
    }
}

impl Shared {
    pub fn set_state(&self, s: u8) {
        self.state.store(s, Ordering::SeqCst);
    }

    pub fn get_state(&self) -> u8 {
        self.state.load(Ordering::SeqCst)
    }

    pub fn identity(&self) -> NetworkIdentity {
        match self.identity.read() {
            Ok(g) => g.clone(),
            Err(e) => e.into_inner().clone(),
        }
    }

    pub fn set_identity(&self, id: NetworkIdentity) {
        match self.identity.write() {
            Ok(mut g) => *g = id,
            Err(e) => *e.into_inner() = id,
        }
    }

    fn db_bytes_cached(&self) -> u64 {
        if let Ok(mut cache) = self.db_bytes.lock() {
            let fresh = cache
                .at
                .map(|at| at.elapsed() < Duration::from_secs(60))
                .unwrap_or(false);
            if !fresh {
                cache.bytes = self.store.disk_usage_bytes();
                cache.at = Some(Instant::now());
            }
            cache.bytes
        } else {
            0
        }
    }

    fn disk_breakdown(&self) -> (u64, u64, u64, u64, u64) {
        let total = self.db_bytes_cached();
        let txids = std::fs::metadata(self.store.txids_path())
            .map(|m| m.len())
            .unwrap_or(0);
        let cache = self.resolver.cache_size_bytes();
        let log = if self.log_file.starts_with(&self.data_dir) {
            std::fs::metadata(&self.log_file)
                .map(|m| m.len())
                .unwrap_or(0)
        } else {
            0
        };
        let snapshots = top_level_snapshot_bytes(&self.data_dir);
        let cache_on_data = self.resolver.cache_dir().starts_with(&self.data_dir);
        let index =
            total.saturating_sub(txids + log + snapshots + if cache_on_data { cache } else { 0 });
        (index, txids, cache, log, snapshots)
    }

    pub fn status_json(&self) -> Value {
        let (height, txnum, hash) = match self.store.committed_tip() {
            Ok(Some(t)) => (t.height as u64, t.txnum, t.block_hash.to_hex()),
            _ => (0, 0, String::new()),
        };
        let core_height = self.stats.core_height.load(Ordering::Relaxed);
        let pruneheight = self.stats.core_pruneheight.load(Ordering::Relaxed);
        let sync = self.sync_status();
        let db_bytes = self.db_bytes_cached();
        let (index_bytes, txids_bytes, cache_bytes, log_bytes, snapshot_bytes) =
            self.disk_breakdown();
        let rawtx = self.resolver.stats();
        let id = self.identity();
        let mem = self.mempool.read();
        let mem_count = mem.as_ref().map(|m| m.len()).unwrap_or(0);
        let mem_version = mem.as_ref().map(|m| m.version()).unwrap_or(0);
        let mem_unresolved = mem.as_ref().map(|m| m.unresolved_inputs()).unwrap_or(0);
        let mem_buckets = mem.as_ref().map(|m| m.fee_histogram().len()).unwrap_or(0);
        drop(mem);
        let machine = self
            .stats
            .plan
            .lock()
            .map(|v| v.clone())
            .unwrap_or_else(|_| json!({}));
        let rocksdb_cf = self
            .stats
            .rocksdb_cf
            .lock()
            .map(|v| v.clone())
            .unwrap_or_else(|_| json!({}));
        json!({
            "version": VERSION,
            "network": id.chain.name(),
            "server": {
                "banner": server_banner(),
                "electrumx_compat": ELECTRUMX_COMPAT.load(Ordering::Relaxed),
                "lines_total": self.stats.lines_total.load(Ordering::Relaxed),
                "batch_lines": self.stats.batch_lines.load(Ordering::Relaxed),
                "batch_items": self.stats.batch_items.load(Ordering::Relaxed),
                "batch_max_items": self.stats.batch_max_items.load(Ordering::Relaxed),
            },
            "machine": machine,
            "genesis": id.genesis.to_hex(),
            "state": state::name(self.get_state()),
            "indexed_height": height,
            "core_height": core_height,
            "core_headers": self.stats.core_headers.load(Ordering::Relaxed),
            "core_rpc_loopback": self.core.is_loopback(),
            "core_verification_progress": self.stats.core_verification_bps.load(Ordering::Relaxed) as f64
                / 10_000.0,
            "ready": sync.ready,
            "at_core_tip": sync.at_core_tip,
            "fully_synced": sync.fully_synced,
            "offline": sync.offline,
            "blocks_per_sec": sync.blocks_per_sec,
            "lag_blocks": core_height.saturating_sub(height),
            "retention_blocks": core_height.saturating_sub(pruneheight),
            "txnum": txnum,
            "tip_hash": hash,
            "db_bytes": db_bytes,
            "disk": {
                "index_bytes": index_bytes,
                "txids_bytes": txids_bytes,
                "cache_bytes": cache_bytes,
                "cache_dir": self.resolver.cache_dir().display().to_string(),
                "log_bytes": log_bytes,
                "snapshot_bytes": snapshot_bytes,
                "total_bytes": db_bytes,
            },
            "disk_free_bytes": el_chain::disk_free_bytes(&self.data_dir).unwrap_or(0),
            "core_disk_free_bytes": el_chain::disk_free_bytes(&self.core_datadir).unwrap_or(0),
            "disk_low": self.stats.disk_low.load(Ordering::Relaxed),
            "blocks_applied": self.stats.blocks_applied.load(Ordering::Relaxed),
            "commits": self.stats.commits.load(Ordering::Relaxed),
            "committed_txs": self.stats.committed_txs.load(Ordering::Relaxed),
            "stage_totals_us": {
                "read": self.stats.stage_read_us.load(Ordering::Relaxed),
                "decode": self.stats.stage_decode_us.load(Ordering::Relaxed),
                "prefetch": self.stats.stage_prefetch_us.load(Ordering::Relaxed),
                "wait": self.stats.stage_wait_us.load(Ordering::Relaxed),
                "apply": self.stats.stage_apply_us.load(Ordering::Relaxed),
                "commit": self.stats.stage_commit_us.load(Ordering::Relaxed),
            },
            "commit_breakdown_us": {
                "txids_append_fsync": self.stats.commit_txids_us.load(Ordering::Relaxed),
                "rocksdb_write": self.stats.commit_db_us.load(Ordering::Relaxed),
            },
            "rocksdb": {
                "pending_compaction_bytes": self.stats.rocksdb_pending_compaction_bytes.load(Ordering::Relaxed),
                "running_compactions": self.stats.rocksdb_running_compactions.load(Ordering::Relaxed),
                "level0_files": self.stats.rocksdb_level0_files.load(Ordering::Relaxed),
                "memtable_bytes": self.stats.rocksdb_memtable_bytes.load(Ordering::Relaxed),
                "block_cache_bytes": self.stats.rocksdb_block_cache_bytes.load(Ordering::Relaxed),
                "sst_bytes": self.stats.rocksdb_sst_bytes.load(Ordering::Relaxed),
                "table_readers_bytes": self.stats.rocksdb_table_readers_bytes.load(Ordering::Relaxed),
                "properties_ok": self.stats.rocksdb_properties_ok.load(Ordering::Relaxed),
                "cf": rocksdb_cf,
            },
            "file_descriptors": {
                "soft_limit": self.stats.fd_soft_limit.load(Ordering::Relaxed),
                "reserve": self.stats.fd_reserve.load(Ordering::Relaxed),
                "max_open_files_requested": self.stats.max_open_files_requested.load(Ordering::Relaxed),
                "max_open_files_effective": self.stats.max_open_files_effective.load(Ordering::Relaxed),
            },
            "clients": self.stats.clients.load(Ordering::Relaxed) as u64,
            "max_clients": self.max_clients as u64,
            "requests_total": self.stats.requests_total.load(Ordering::Relaxed),
            "errors_total": self.stats.errors_total.load(Ordering::Relaxed),
            "rate_limited": self.stats.rate_limited.load(Ordering::Relaxed),
            "rate_limit_rps": self.clients.rate_limit().0,
            "rate_burst": self.clients.rate_limit().1,
            "last_block_ms": self.stats.last_block_ms.load(Ordering::Relaxed),
            "uptime_seconds": self.stats.started.elapsed().as_secs(),
            "listen": self.listen,
            "tls_listen": self.tls_listen,
            "onion": self
                .onion
                .lock()
                .ok()
                .and_then(|v| v.clone())
                .unwrap_or_default(),
            "onion_plain_port": self.onion_plain_port,
            "onion_tls_port": self.onion_tls_port,
            "tor_bootstrapped": self.stats.tor_bootstrapped.load(Ordering::Relaxed),
            "tor": {
                "managed": self.stats.tor_managed.load(Ordering::Relaxed),
                "socks_port": self.stats.tor_socks_port.load(Ordering::Relaxed),
                // e-lite's managed torrc has no ControlPort; Core's own onion
                // is disabled with -listenonion=0.
                "control_port": self.stats.tor_control_port.load(Ordering::Relaxed),
            },
            "pipeline": {
                "prepared": self.stats.pipeline_prepared.load(Ordering::Relaxed),
                "applied": self.stats.pipeline_applied.load(Ordering::Relaxed),
                "discarded": self.stats.pipeline_discarded.load(Ordering::Relaxed),
            },
            "scheduler": {
                "mode": if self.stats.scheduler_alternate.load(Ordering::Relaxed) {
                    "alternate"
                } else {
                    "overlap"
                },
                "index_paused": self.stats.scheduler_index_paused.load(Ordering::Relaxed),
                "backlog_bytes": self.stats.scheduler_backlog_bytes.load(Ordering::Relaxed),
                "high_bytes": self.stats.scheduler_high_bytes.load(Ordering::Relaxed),
                "low_bytes": self.stats.scheduler_low_bytes.load(Ordering::Relaxed),
                "avg_block_bytes": self.stats.scheduler_avg_block_bytes.load(Ordering::Relaxed),
                "phase": match self.stats.scheduler_phase.load(Ordering::Relaxed) {
                    1 => "draining",
                    2 => "core",
                    _ => "indexing",
                },
                "phase_blocks": self.stats.scheduler_phase_blocks.load(Ordering::Relaxed),
                "phase_elapsed_ms": self.stats.scheduler_phase_elapsed_ms.load(Ordering::Relaxed),
            },
            "tls_handshakes": self.stats.tls_handshakes.load(Ordering::Relaxed),
            "tls_handshake_failures": self
                .stats
                .tls_handshake_failures
                .load(Ordering::Relaxed),
            "protocol": PROTOCOL_MAX,
            "memory": {
                "total_mib": self.stats.mem_total_mib.load(Ordering::Relaxed),
                "available_mib": self.stats.mem_available_mib.load(Ordering::Relaxed),
                "cgroup_limit_mib": self.stats.mem_cgroup_limit_mib.load(Ordering::Relaxed),
                "cgroup_used_mib": self.stats.mem_cgroup_used_mib.load(Ordering::Relaxed),
                "reclaimable_mib": self.stats.mem_reclaimable_mib.load(Ordering::Relaxed),
                "cached_mib": self.stats.mem_cached_mib.load(Ordering::Relaxed),
                "cache_max_mib": self.stats.mem_cache_max_mib.load(Ordering::Relaxed),
                "memtables_mib": self.stats.mem_memtables_mib.load(Ordering::Relaxed),
                "memtable_bytes_mib": self.stats.mem_memtable_bytes_mib.load(Ordering::Relaxed),
                "table_readers_mib": self.stats.mem_table_readers_mib.load(Ordering::Relaxed),
                "effective_mib": self.stats.mem_effective_mib.load(Ordering::Relaxed),
                "dynamic_reserve_mib": self.stats.mem_dynamic_reserve_mib.load(Ordering::Relaxed),
                "reserve_mib": self.stats.mem_reserve_mib.load(Ordering::Relaxed),
                "budget_mib": self.stats.mem_budget_mib.load(Ordering::Relaxed),
            },
            "tuning": {
                "readers": self.stats.tuning_readers.load(Ordering::Relaxed),
                "commit_interval_ms": self.stats.tuning_commit_ms.load(Ordering::Relaxed),
                "batch_blocks": self.stats.tuning_batch_blocks.load(Ordering::Relaxed),
                "cache_mib": self.stats.tuning_cache_mib.load(Ordering::Relaxed),
                "read_ms_ewma": self.stats.tuning_read_ms.load(Ordering::Relaxed),
                "apply_ms_ewma": self.stats.tuning_apply_ms.load(Ordering::Relaxed),
                "decode_ms_ewma": self.stats.tuning_decode_ms.load(Ordering::Relaxed),
                "wait_ms_ewma": self.stats.tuning_wait_ms.load(Ordering::Relaxed),
                "prefetch_p50_ms": self.stats.tuning_prefetch_p50_ms.load(Ordering::Relaxed),
                "prefetch_p95_ms": self.stats.tuning_prefetch_p95_ms.load(Ordering::Relaxed),
                "prefetch_max_ms": self.stats.tuning_prefetch_max_ms.load(Ordering::Relaxed),
                "wait_p50_ms": self.stats.tuning_wait_p50_ms.load(Ordering::Relaxed),
                "wait_p95_ms": self.stats.tuning_wait_p95_ms.load(Ordering::Relaxed),
                "commit_ms_ewma": self.stats.tuning_commit_ms_ewma.load(Ordering::Relaxed),
                "commit_per_block_ms": self.stats.tuning_commit_per_block_ms.load(Ordering::Relaxed),
                "commit_cycle_ms": self.stats.tuning_commit_cycle_ms.load(Ordering::Relaxed),
                "commit_last_ms": self.stats.tuning_commit_last_ms.load(Ordering::Relaxed),
                "commit_last_cycle_ms": self.stats.tuning_commit_last_cycle_ms.load(Ordering::Relaxed),
                "commit_last_blocks": self.stats.tuning_commit_last_blocks.load(Ordering::Relaxed),
                "commit_last_txs": self.stats.tuning_commit_last_txs.load(Ordering::Relaxed),
                "commit_last_bytes": self.stats.tuning_commit_last_bytes.load(Ordering::Relaxed),
                "commit_last_txids_us": self.stats.tuning_commit_last_txids_us.load(Ordering::Relaxed),
                "commit_last_db_us": self.stats.tuning_commit_last_db_us.load(Ordering::Relaxed),
                "commit_last_reason": self.stats.tuning_commit_last_reason.load(Ordering::Relaxed),
                "adapt_steps": self.stats.tuning_adapt_steps.load(Ordering::Relaxed),
                "tx_per_sec": self.stats.tuning_tx_per_sec.load(Ordering::Relaxed),
                "blocks_per_sec": self.stats.tuning_blocks_per_sec.load(Ordering::Relaxed),
                "profile": match self.stats.tuning_profile.load(Ordering::Relaxed) {
                    1 => "hdd", 2 => "ssd", 3 => "nvme", 4 => "manual", _ => "auto"
                },
                "adaptive": self.stats.tuning_adaptive.load(Ordering::Relaxed),
                "background_jobs": self.stats.tuning_background_jobs.load(Ordering::Relaxed),
            },
            "core": {
                "pruneheight": pruneheight,
                "pruned": self.stats.core_pruned.load(Ordering::Relaxed),
                "network_active": self.stats.core_network_active.load(Ordering::Relaxed),
                "initial_block_download": self.stats.core_ibd.load(Ordering::Relaxed),
                "connections": self.stats.core_connections.load(Ordering::Relaxed),
                "prune_requests": self.stats.prune_requests.load(Ordering::Relaxed),
                "prune_failures": self.stats.prune_failures.load(Ordering::Relaxed),
            },
            "notifier": {
                "runs": self.stats.notifier_runs.load(Ordering::Relaxed),
                "sent": self.stats.notifications_sent.load(Ordering::Relaxed),
                "errors": self.stats.notifier_errors.load(Ordering::Relaxed),
            },
            "mempool": {
                "count": mem_count,
                "version": mem_version,
                "unresolved_inputs": mem_unresolved,
                "histogram_buckets": mem_buckets,
            },
            "public_servers": &self.public_servers,
            "provider_status": self
                .provider_status
                .lock()
                .map(|s| s.clone())
                .unwrap_or_default(),
            "provider_ready": self.provider_ready.load(Ordering::Relaxed),
            "providers_healthy": self
                .provider_status
                .lock()
                .map(|s| s.iter().filter(|p| p.get("healthy").and_then(Value::as_bool).unwrap_or(false)).count() as u64)
                .unwrap_or(0),
            "tor_ready": self.tor_ready.load(Ordering::Relaxed),
            "clearnet_fallback": self.clearnet_fallback.load(Ordering::Relaxed),
            "rawtx": {
                "path": self.rawtx_path,
                "cache_hits": rawtx.cache_hits.load(Ordering::Relaxed),
                "cache_puts": rawtx.cache_puts.load(Ordering::Relaxed),
                "cache_blocks": self.resolver.cached_blocks(),
                "cache_txs": self.resolver.cached_txs(),
                "cache_bytes": self.resolver.cache_size_bytes(),
                "cache_dir": self.resolver.cache_dir().display().to_string(),
                "fetches": rawtx.fetches.load(Ordering::Relaxed),
                "bytes": rawtx.bytes.load(Ordering::Relaxed),
                "verifications": rawtx.verifications.load(Ordering::Relaxed),
                "verify_failures": rawtx.verify_failures.load(Ordering::Relaxed),
                "decoys": rawtx.decoys.load(Ordering::Relaxed),
                "decoys_pending": self.resolver.decoys_pending(),
                "unavailable": rawtx.unavailable.load(Ordering::Relaxed),
                "tx_get_failures": self.stats.tx_get_failures.to_json(),
                "external_sources": self.resolver.external_sources(),
                "p2p_attempts": rawtx.p2p_attempts.load(Ordering::Relaxed),
                "p2p_failures": rawtx.p2p_failures.load(Ordering::Relaxed),
                "p2p_skipped": rawtx.p2p_skipped.load(Ordering::Relaxed),
                "p2p_tor_failures": rawtx.p2p_tor_failures.load(Ordering::Relaxed),
                "tx_mode": {
                    "active": self.resolver.tx_mode_servers().is_some(),
                    "servers": self.resolver.tx_mode_servers().unwrap_or(0),
                    "preset": self.resolver.privacy().preset,
                    "mode": self.resolver.privacy().mode.name(),
                    "decoy_wallets": self.resolver.privacy().decoy_wallets,
                    "session_size": self.resolver.privacy().session_size,
                    "sessions": rawtx.tx_sessions.load(Ordering::Relaxed),
                    "real_sent": rawtx.tx_real.load(Ordering::Relaxed),
                    "decoys_sent": rawtx.tx_decoys.load(Ordering::Relaxed),
                    "verified": rawtx.tx_verified.load(Ordering::Relaxed),
                    "rejected": rawtx.tx_verify_failed.load(Ordering::Relaxed),
                    "block_fallback": rawtx.tx_fallback.load(Ordering::Relaxed),
                },
                "race": {
                    "attempts": rawtx.race_attempts.load(Ordering::Relaxed),
                    "hedges": rawtx.race_hedges.load(Ordering::Relaxed),
                    "wins_esplora": rawtx.race_wins_esplora.load(Ordering::Relaxed),
                    "wins_p2p": rawtx.race_wins_p2p.load(Ordering::Relaxed),
                    "wins_other": rawtx.race_wins_other.load(Ordering::Relaxed),
                    "win_ms_total": rawtx.race_win_ms.load(Ordering::Relaxed),
                },
            },
            "prefetch": {
                "queued": self.prefetch.stats().0,
                "done": self.prefetch.stats().1,
            },
        })
    }
}

/// On-demand snapshots are user files; count the common top-level layout only.
pub(crate) fn top_level_snapshot_bytes(dir: &std::path::Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_string_lossy();
            if !path.is_file() || !name.ends_with(".tar.gz") {
                return None;
            }
            entry.metadata().ok().map(|m| m.len())
        })
        .sum()
}

/// Free bytes on the filesystem containing `path` (0 when it cannot be read).
pub fn disk_free_bytes(path: &std::path::Path) -> u64 {
    el_chain::disk_free_bytes(path).unwrap_or(0)
}
