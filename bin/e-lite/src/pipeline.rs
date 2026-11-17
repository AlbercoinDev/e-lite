//! The indexing pipeline: Core -> read -> decode/prevout -> apply -> commit
//! -> prune.
//!
//! This file holds the indexer itself: its state, reorgs, the commit and the
//! catch-up call. The rest lives in `pipeline/`: `core_pace` (pausing Core),
//! `supervise` (prune gate and chain supervisor), `progress` (log lines) and
//! `run` (the `index` command and its loop).

use super::*;

mod core_pace;
mod progress;
mod run;
mod supervise;

pub(crate) use run::*;
pub(crate) use supervise::*;

pub(crate) const CATCH_UP_BLOCKS: u64 = 512;

pub(crate) type FetchResult =
    std::result::Result<(u64, el_index::PreparedBlock, el_index::PrefetchedPrevouts), E>;
pub(crate) type PendingMap = std::collections::BTreeMap<usize, FetchResult>;

pub(crate) struct MilestoneState {
    pub(crate) last_height: u64,
    pub(crate) last_txnum: u64,
    pub(crate) last_at: Instant,
}

pub(crate) struct Indexer {
    pub(crate) store: Arc<Store>,
    pub(crate) core: CoreClient,
    pub(crate) bulk: Option<Arc<el_blocks::BulkSource>>,
    pub(crate) resolver: Option<Arc<Resolver>>,
    pub(crate) tunables: Arc<Tunables>,
    pub(crate) cfg: Config,
    pub(crate) lock: Arc<RwLock<()>>,
    pub(crate) shared: Option<Arc<Shared>>,
    pub(crate) shutdown: Arc<AtomicBool>,
    pub(crate) readers: Option<Arc<ReaderTracker>>,
    pub(crate) can_control_core: bool,
    pub(crate) lag_paused: AtomicBool,
    /// Core downloads are paused (disk pressure, lag guard or manual pause),
    /// but indexing of already-downloaded blocks may continue.
    pub(crate) downloads_paused: AtomicBool,
    /// Indexing itself is paused. Only the critical disk floor or shutdown set
    /// this; it is separate from pausing Core downloads.
    pub(crate) paused: AtomicBool,
    pub(crate) pause_marker: std::path::PathBuf,
    pub(crate) data_dir: std::path::PathBuf,
    pub(crate) core_datadir: std::path::PathBuf,
    pub(crate) blocks_dir: std::path::PathBuf,
    pub(crate) last_disk_check: Mutex<Instant>,
    /// Rate limit for the no-progress pipeline note (10 s).
    pub(crate) last_pipeline_note: Mutex<Option<Instant>>,
    pub(crate) progress_started: Instant,
    pub(crate) progress_start_height: u64,
    pub(crate) progress_start_txnum: u64,
    pub(crate) milestone: Mutex<MilestoneState>,
    pub(crate) milestone_sizes_running: Arc<AtomicBool>,
    pub(crate) bulk_started: AtomicBool,
    pub(crate) bulk_full_pause: AtomicBool,
    /// Byte watermark confirmation: a pause/resume must persist before acting.
    pub(crate) high_since: Mutex<Option<Instant>>,
    pub(crate) low_since: Mutex<Option<Instant>>,
    /// 0 = indexing while Core downloads, 1 = Core paused/e-lite draining.
    pub(crate) phase: AtomicU64,
    pub(crate) phase_state: Mutex<Option<(Instant, u64)>>,
    pub(crate) last_rocksdb_log: Mutex<Instant>,
    /// Latest Core observation shared by a 1 Hz poller thread. Keeps
    /// `getblockchaininfo` out of the block-application path.
    pub(crate) core_obs: Arc<Mutex<Option<(BlockchainInfo, Instant)>>>,
    /// `pruneblockchain` runs in its own thread: the WAL barrier is taken on
    /// the apply thread, but the RPC may spend tens of seconds in Core.
    pub(crate) prune_rx: Mutex<Option<std::sync::mpsc::Receiver<Result<i64, String>>>>,
    pub(crate) prune_in_flight: AtomicBool,
    /// A prune requested by the supervisor. It is started only at a commit
    /// boundary, after the current group has been committed and the WAL
    /// barrier has advanced the durable watermark.
    pub(crate) deferred_prune: Mutex<Option<u32>>,
}

impl Drop for Indexer {
    fn drop(&mut self) {
        // Never leave Core with P2P disabled after a clean e-lite shutdown.
        if self.can_control_core
            && (self.lag_paused.load(Ordering::Relaxed)
                || self.downloads_paused.load(Ordering::Relaxed))
        {
            let _ = self.core.set_network_active(true);
            self.mark_paused(false);
            if let Some(sh) = &self.shared {
                sh.stats.core_network_active.store(true, Ordering::Relaxed);
            }
        }
    }
}

impl Indexer {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        store: Arc<Store>,
        core: CoreClient,
        bulk: Option<Arc<el_blocks::BulkSource>>,
        resolver: Option<Arc<Resolver>>,
        tunables: Arc<Tunables>,
        cfg: Config,
        lock: Arc<RwLock<()>>,
        shared: Option<Arc<Shared>>,
        shutdown: Arc<AtomicBool>,
        readers: Option<Arc<ReaderTracker>>,
        can_control_core: bool,
        chain: Chain,
    ) -> Self {
        let data_dir = cfg.data_dir(chain);
        let core_datadir = cfg.core_datadir(chain);
        let blocks_dir = cfg.blocks_dir(chain);
        let pause_marker = data_dir.join("core-paused-by-e-lite");
        let tip = store.committed_tip().ok().flatten();
        let start_height = tip.as_ref().map(|t| t.height as u64).unwrap_or(0);
        let start_txnum = tip.as_ref().map(|t| t.txnum).unwrap_or(0);
        let now = Instant::now();
        if let Some(sh) = &shared {
            sh.stats.scheduler_high_bytes.store(
                cfg.pruning.max_bulk_lag_gib.saturating_mul(GIB),
                Ordering::Relaxed,
            );
            sh.stats.scheduler_low_bytes.store(
                cfg.pruning.core_resume_ahead_gib.saturating_mul(GIB),
                Ordering::Relaxed,
            );
        }
        Indexer {
            store,
            core,
            bulk,
            resolver,
            tunables,
            cfg,
            lock,
            shared,
            shutdown,
            readers,
            can_control_core,
            lag_paused: AtomicBool::new(false),
            downloads_paused: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            pause_marker,
            data_dir,
            core_datadir,
            blocks_dir,
            last_disk_check: Mutex::new(
                Instant::now()
                    .checked_sub(Duration::from_secs(60))
                    .unwrap_or_else(Instant::now),
            ),
            last_pipeline_note: Mutex::new(None),
            progress_started: now,
            progress_start_height: start_height,
            progress_start_txnum: start_txnum,
            milestone: Mutex::new(MilestoneState {
                last_height: start_height,
                last_txnum: start_txnum,
                last_at: now,
            }),
            milestone_sizes_running: Arc::new(AtomicBool::new(false)),
            bulk_started: AtomicBool::new(false),
            bulk_full_pause: AtomicBool::new(false),
            high_since: Mutex::new(None),
            low_since: Mutex::new(None),
            phase: AtomicU64::new(0),
            phase_state: Mutex::new(None),
            last_rocksdb_log: Mutex::new(Instant::now()),
            core_obs: Arc::new(Mutex::new(None)),
            prune_rx: Mutex::new(None),
            prune_in_flight: AtomicBool::new(false),
            deferred_prune: Mutex::new(None),
        }
    }

    /// Shared handle for the Core observation poller.
    fn core_observation(&self) -> Arc<Mutex<Option<(BlockchainInfo, Instant)>>> {
        Arc::clone(&self.core_obs)
    }

    /// Cached observation, falling back to a synchronous RPC only when the
    /// cached value is missing or older than `max_age`.
    fn observed_core(&self, max_age: Duration) -> Option<BlockchainInfo> {
        if let Ok(guard) = self.core_obs.lock() {
            if let Some((info, at)) = guard.as_ref() {
                if at.elapsed() <= max_age {
                    return Some(info.clone());
                }
            }
        }
        None
    }

    fn stats_core_height(&self, h: u64) {
        if let Some(sh) = &self.shared {
            sh.stats.core_height.store(h, Ordering::Relaxed);
        }
        self.tunables.set_core_state(h, self.tunables.core_ibd());
    }

    /// Live Core tip and pruneheight for progress lines. Falls back to the
    /// catch-up snapshot when Core is briefly unavailable.
    fn live_core_and_prune(&self, fallback_core: u64, fallback_prune: u64) -> (u64, u64) {
        let cached = self
            .shared
            .as_ref()
            .map(|sh| sh.stats.core_height.load(Ordering::Relaxed))
            .unwrap_or(0);
        let core = self
            .core
            .block_count()
            .unwrap_or(fallback_core)
            .max(fallback_core)
            .max(cached);
        let prune = self
            .shared
            .as_ref()
            .map(|sh| sh.stats.core_pruneheight.load(Ordering::Relaxed))
            .filter(|h| *h > 0)
            .unwrap_or(fallback_prune);
        (core, prune)
    }

    fn refresh_core_height(&self) {
        if let Ok(h) = self.core.block_count() {
            self.stats_core_height(h);
        }
    }

    fn stats_tip(&self, tip: &IndexedTip, last_block_ms: u64) {
        if let Some(sh) = &self.shared {
            sh.stats
                .indexed_height
                .store(tip.height as u64, Ordering::Relaxed);
            sh.stats.txnum.store(tip.txnum, Ordering::Relaxed);
            sh.stats
                .last_block_ms
                .store(last_block_ms, Ordering::Relaxed);
            sh.stats.blocks_applied.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn handle_reorg(&self, core_tip: u64) -> R<()> {
        let Some(tip) = self.store.committed_tip()? else {
            return Ok(());
        };
        let check_height = (tip.height as u64).min(core_tip);
        let core_hash = self.core.block_hash(check_height)?;
        let db_rec = self
            .store
            .get_block(check_height as u32)?
            .ok_or("database is missing its own tip block")?;
        if db_rec.hash.to_hex() == core_hash {
            // Core's chain can be shorter than the index (invalidateblock,
            // manual rollback). Drop the blocks above Core's tip.
            if (tip.height as u64) > core_tip {
                let window = self.cfg.index.reorg_window as u64;
                if (tip.height as u64).saturating_sub(core_tip) > window {
                    return fatal(format!(
                        "Core tip {core_tip} is {} blocks below the index ({}), beyond the \
                         {window}-block undo window: NEEDS_RECOVERY (restore a snapshot or reindex)",
                        (tip.height as u64).saturating_sub(core_tip),
                        tip.height
                    ));
                }
                if let Some(sh) = &self.shared {
                    sh.set_state(state::REORG);
                }
                eprintln!(
                    "[e-lite] reorg detected: Core tip {core_tip} is behind the index ({}); rewinding",
                    tip.height
                );
                let _g = self.lock.write().map_err(|_| "lock poisoned")?;
                rewind_to(&self.store, check_height as u32)?;
                drop(_g);
                if let Some(sh) = &self.shared {
                    sh.headers.invalidate_from(check_height as u32);
                    sh.dirty_scripts.mark_all_subscriptions();
                    refresh_ready_state(sh);
                }
            }
            return Ok(());
        }

        if let Some(sh) = &self.shared {
            sh.set_state(state::REORG);
        }
        let window = self.cfg.index.reorg_window as u64;
        let mut h = check_height;
        loop {
            if h == 0 {
                return fatal(format!(
                    "genesis mismatch: db {}, core {} — wrong network or wiped node",
                    tip.block_hash,
                    self.core.block_hash(0)?
                ));
            }
            let ch = self.core.block_hash(h)?;
            let db = self
                .store
                .get_block(h as u32)?
                .ok_or_else(|| format!("db missing block {h}"))?;
            if db.hash.to_hex() == ch {
                break;
            }
            h -= 1;
            if (tip.height as u64).saturating_sub(h) > window {
                return fatal(format!(
                    "reorg deeper than the {window}-block undo window: NEEDS_RECOVERY (restore a snapshot or reindex)"
                ));
            }
        }
        eprintln!(
            "[e-lite] reorg detected: rewinding from {} to {} and replaying the active chain",
            tip.height, h
        );
        {
            let _g = self.lock.write().map_err(|_| "lock poisoned")?;
            rewind_to(&self.store, h as u32)?;
        }
        if let Some(sh) = &self.shared {
            sh.headers.invalidate_from(h as u32);
            sh.dirty_scripts.mark_all_subscriptions();
        }
        Ok(())
    }

    /// `auto`: relax the RocksDB fsync during bulk/IBD and return to strict
    /// durability when the index is live. `txids.dat` is fsynced either way.
    fn commit_sync(&self, ibd: bool, height: u64, core_tip: u64) -> bool {
        commit_sync_policy(&self.cfg.index.commit_sync, ibd, height, core_tip)
    }

    #[allow(clippy::too_many_arguments)]
    fn commit_group(
        &self,
        batch: WriteBatch,
        tip: &IndexedTip,
        txids: &[Hash32],
        blocks: u32,
        bytes: u64,
        last_block_ms: u64,
        undo: &[el_model::UndoRecord],
        sync: bool,
        reason: CommitReason,
    ) -> R<()> {
        let mut batch = batch;
        // Undo is built for every applied block, so an early commit (time,
        // transaction limit, boundary, shutdown) is as rewindable as a full
        // batch. Only the records inside the window of the tip actually
        // committed are written (AUD-131); older ones in the database are
        // deleted in the same batch.
        prune_undo(
            &self.store,
            tip.height,
            self.cfg.index.reorg_window,
            &mut batch,
        )?;
        write_window_undo(
            &self.store,
            &mut batch,
            undo,
            tip.height,
            self.cfg.index.reorg_window,
        )?;
        {
            let _g = self.lock.write().map_err(|_| "lock poisoned")?;
            let commit_started = Instant::now();
            let result = self.store.commit_with_sync(batch, tip, txids, sync);
            let (ct, cd) = self.store.last_commit_timings_us();
            // The spike check compares against the per-block cost of the last
            // full group *before* this one, so a sustained slowdown cannot be
            // silently re-normalised into the baseline.
            let prev_per_block = self.tunables.commit_per_block_ms.load(Ordering::Relaxed);
            let commit_elapsed = commit_started.elapsed();
            self.tunables.record_commit_group(
                commit_started,
                commit_elapsed,
                blocks as u64,
                txids.len() as u64,
                bytes,
                reason,
                ct,
                cd,
            );
            if result.is_ok() && reason.full() && blocks > 0 {
                self.warn_commit_spike(
                    commit_elapsed,
                    blocks,
                    txids.len() as u64,
                    bytes,
                    reason,
                    tip.height as u64,
                    prev_per_block,
                );
            }
            crate::trace_log!(
                "commit-group: {} ms, {} blocks / {} txs / {} MiB, reason {}, \
                 txids {} us, db {} us, sync {}",
                commit_elapsed.as_millis(),
                blocks,
                txids.len(),
                bytes / (1024 * 1024),
                reason.name(),
                ct,
                cd,
                sync
            );
            if let Err(e) = result {
                // Whatever happened, keep the flat file aligned with the DB.
                if let Ok(Some(t)) = self.store.committed_tip() {
                    self.store.truncate_txids(t.txnum)?;
                }
                return Err(e.into());
            }
        }
        self.stats_tip(tip, last_block_ms);
        if let Some(sh) = &self.shared {
            sh.stats.commits.fetch_add(1, Ordering::Relaxed);
            sh.stats
                .committed_txs
                .fetch_add(txids.len() as u64, Ordering::Relaxed);
            let (ct, cd) = self.store.last_commit_timings_us();
            sh.stats.commit_txids_us.store(ct, Ordering::Relaxed);
            sh.stats.commit_db_us.store(cd, Ordering::Relaxed);
            let c = self.store.compaction_stats();
            sh.stats
                .rocksdb_pending_compaction_bytes
                .store(c.pending_compaction_bytes, Ordering::Relaxed);
            sh.stats
                .rocksdb_running_compactions
                .store(c.running_compactions, Ordering::Relaxed);
            sh.stats
                .rocksdb_memtable_bytes
                .store(c.memtable_bytes, Ordering::Relaxed);
            sh.stats
                .rocksdb_block_cache_bytes
                .store(c.block_cache_bytes, Ordering::Relaxed);
            sh.stats
                .rocksdb_sst_bytes
                .store(c.sst_files_bytes, Ordering::Relaxed);
            sh.stats
                .rocksdb_table_readers_bytes
                .store(c.table_readers_bytes, Ordering::Relaxed);
            sh.stats
                .rocksdb_level0_files
                .store(c.level0_files, Ordering::Relaxed);
            sh.stats
                .rocksdb_properties_ok
                .store(c.properties_ok, Ordering::Relaxed);
            let mut cf_map = serde_json::Map::new();
            for (name, pending, memtable, sst, readers, level0, ok) in self.store.cf_stats() {
                cf_map.insert(
                    name.to_string(),
                    json!({
                        "pending_compaction_bytes": pending,
                        "memtable_bytes": memtable,
                        "sst_bytes": sst,
                        "table_readers_bytes": readers,
                        "level0_files": level0,
                        "properties_ok": ok,
                    }),
                );
            }
            if let Ok(mut v) = sh.stats.rocksdb_cf.lock() {
                *v = serde_json::Value::Object(cf_map);
            }
            let fd = self.store.fd_plan();
            sh.stats
                .fd_soft_limit
                .store(fd.soft_limit.unwrap_or(0), Ordering::Relaxed);
            sh.stats.fd_reserve.store(fd.reserve, Ordering::Relaxed);
            sh.stats
                .max_open_files_requested
                .store(fd.requested as u64, Ordering::Relaxed);
            sh.stats
                .max_open_files_effective
                .store(fd.effective as u64, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Explain a slow commit group from the log alone. Bulk groups
    /// legitimately run for the full commit interval (20-60 s), so the
    /// thresholds are well above normal bulk cost: twice the configured time
    /// limit, or 5x the previous full group's per-block cost once the group
    /// reaches 10 s. The context — running compactions, L0 files, real
    /// memtables, pending compaction and the Core backlog — is sampled at the
    /// spike so a stall like a 45 s commit can be attributed afterwards from
    /// the log.
    #[allow(clippy::too_many_arguments)]
    fn warn_commit_spike(
        &self,
        d: Duration,
        blocks: u32,
        txs: u64,
        bytes: u64,
        reason: CommitReason,
        tip_height: u64,
        prev_per_block: u64,
    ) {
        let ms = d.as_millis() as u64;
        if ms < 10_000 {
            return;
        }
        let over_limit = ms >= self.tunables.bulk_commit_ms_limit().saturating_mul(2);
        let per_block = ms / blocks.max(1) as u64;
        let over_ewma = per_block >= prev_per_block.max(1_000).saturating_mul(5);
        if !(over_limit || over_ewma) {
            return;
        }
        let c = self.store.compaction_stats();
        let lag = self.tunables.core_height().saturating_sub(tip_height);
        eprintln!(
            "[e-lite] commit spike: {} ms for {} blocks / {} txs ({} MiB), reason {}; \
             rocksdb running {}, l0 {} files, memtable {} MiB, pending_compaction {} MiB; \
             lag {} blocks; previous per-block cost {} ms",
            ms,
            blocks,
            txs,
            bytes / (1024 * 1024),
            reason.name(),
            c.running_compactions,
            c.level0_files,
            c.memtable_bytes / (1024 * 1024),
            c.pending_compaction_bytes / (1024 * 1024),
            lag,
            prev_per_block,
        );
    }

    /// Everything a catch-up call checks before it reads a block: disks,
    /// Core's state, a pause left by a crashed run, pruning coherence, reorgs
    /// and the scheduler. `Idle` means there is nothing to index right now.
    fn preflight(&self) -> R<Preflight> {
        let tip_at_start = self.store.committed_tip()?;
        let height_at_start = tip_at_start.map(|t| t.height as u64).unwrap_or(0);
        // Check the disks before any Core RPC or block write. If Core is slow
        // or unreachable, this still stops the indexer before the disk fills.
        if self.disk_guard() {
            return Ok(Preflight::Idle(height_at_start));
        }
        // Always ask Core: a new block or a reorg must be seen at once.
        let info = self.core.blockchain_info()?;
        let core_tip = info.blocks;
        self.tunables
            .set_core_state(core_tip, info.initialblockdownload);
        self.stats_core_height(core_tip);
        self.publish_core_state(&info);
        // A previous e-lite run may have paused Core and crashed. The marker
        // is only written by e-lite, so resuming is safe. Do not consume a
        // marker written by a *live* lag pause: the guard owns that state and
        // must resume at its own watermark (75% of the limit).
        if self.can_control_core
            && !self.lag_paused.load(Ordering::Relaxed)
            && !self.downloads_paused()
            && self.pause_marker.is_file()
            && self.core.set_network_active(true).is_ok()
        {
            let _ = std::fs::remove_file(&self.pause_marker);
            self.lag_paused.store(false, Ordering::Relaxed);
            if let Some(sh) = &self.shared {
                sh.stats.core_network_active.store(true, Ordering::Relaxed);
            }
            eprintln!("[e-lite] resumed Core downloads after a previous pause");
        }
        if self.is_paused() {
            return Ok(Preflight::Idle(height_at_start));
        }
        let repair_limit = self
            .resolver
            .as_ref()
            .filter(|r| r.has_repair_sources())
            .map(|_| self.cfg.pruning.repair_max_gap_blocks as u64);
        check_pruned(&info, tip_at_start.as_ref(), repair_limit)?;
        // Pruned IBD from scratch is local-only and requires a Core owned by
        // e-lite, so the lag guard can pause it before it prunes.
        if info.pruned && tip_at_start.is_none() {
            if self.bulk.is_none() {
                return fatal(
                    "Core is pruned and the index is empty: pruned IBD requires a local Core \
                     blocks directory (run e-lite on the Core host) and [core] prune_mib = 1",
                );
            }
            if !self.can_control_core {
                return fatal(
                    "Core is pruned and the index is empty, but e-lite did not start Core; \
                     stop Core and use [core] manage = \"managed\" with prune_mib = 1, or use a full node",
                );
            }
        }
        self.handle_reorg(core_tip)?;

        // `handle_reorg` may have rewound the database; always continue from
        // the committed tip after the rewind, not from the pre-reorg tip.
        let tip = self.store.committed_tip()?;
        let committed = tip.map(|t| t.height as u64).unwrap_or(0);

        // Automatic pruning (`-prune=NNN`) is decided by Core's own tip. If we
        // fall too far behind, stop new downloads while the backlog is drained.
        self.lag_guard(&info, committed);
        let phase = u64::from(self.lag_paused.load(Ordering::Relaxed));
        self.update_phase(phase, committed);
        Ok(Preflight::Ready { info, tip })
    }

    /// Start the readers of one call: each fetches a block, decodes it and
    /// resolves its prevouts in parallel with the apply thread.
    fn spawn_readers(
        &self,
        start: u64,
        hashes: Vec<String>,
        pruneheight: u64,
    ) -> ReaderPool<FetchResult> {
        // At the tip a call asks for one or two blocks every 500 ms: spawning
        // the full reader pool for that was pure thread churn (AUD-146).
        let workers = self.tunables.readers().min(hashes.len()).max(1);
        // Bound prepared blocks (fetched/decoded/prevout-mapped) to a small
        // window, by both worker count and the configured prefetch depth. The
        // result channel alone only bounds queued results; the apply thread
        // can keep draining out-of-order results into `pending`. A window
        // measured only in blocks ignores that a full block can be 100x
        // heavier than an empty one.
        let default_window = (workers * 2).clamp(4, 32);
        let configured = self.tunables.prefetch_blocks();
        let window = if configured == 0 {
            default_window
        } else {
            default_window.min(configured.clamp(2, 64))
        };
        let items = hashes.len();
        let hashes = Arc::new(hashes);
        let core = self.core.clone();
        let bulk = self.bulk.clone();
        let resolver = self.resolver.clone();
        let store = Arc::clone(&self.store);
        let tunables = Arc::clone(&self.tunables);
        ReaderPool::spawn(workers, window, items, move |i| {
            let read_started = Instant::now();
            let fetched = fetch_block(
                &core,
                &bulk,
                resolver.as_deref(),
                pruneheight,
                start + i as u64,
                &hashes[i],
            );
            tunables.record_read(read_started.elapsed());
            let (height, expected_hash, bytes) = fetched?;
            let decode_started = Instant::now();
            let prepared = prepare_block_bytes(&bytes, &expected_hash, height as u32);
            tunables.record_decode(decode_started.elapsed());
            let prepared = prepared.map_err(|e| e.to_string())?;
            // Resolve prevouts here, in parallel with the apply thread, so
            // random UTXO reads do not serialise the whole pipeline. Missing
            // entries fall back to the overlay/point lookup during apply.
            let prefetch_started = Instant::now();
            let prevouts = prefetch_prevouts(&store, &prepared);
            tunables.record_prefetch(prefetch_started.elapsed());
            prevouts
                .map(|m| (height, prepared, m))
                .map_err(|e| E::from(e.to_string()))
        })
    }

    /// Wait for block `index` of the call. Readers finish out of order, so
    /// later blocks are parked in `pending`. `None` ends the call: the readers
    /// are gone, or none of them produced this block in 10 s.
    fn next_prepared(
        &self,
        pool: &ReaderPool<FetchResult>,
        pending: &mut PendingMap,
        index: usize,
        start: u64,
    ) -> Option<FetchResult> {
        let wait_started = Instant::now();
        // Bound the wait: a wedged reader (a panic dies silently, an RPC can
        // stall) must not block the apply loop forever. The next call has
        // fresh readers; the timeout only makes the stall loud.
        let wait_deadline = wait_started + Duration::from_secs(10);
        while !pending.contains_key(&index) {
            let Some(remaining) = wait_deadline.checked_duration_since(Instant::now()) else {
                crate::debug_log!(
                    "pipeline: 10 s without block {} from the readers; \
                     ending the call (items pending {})",
                    start + index as u64,
                    pending.len()
                );
                break;
            };
            match pool.recv_timeout(remaining) {
                Ok((i, item)) => {
                    pending.insert(i, item);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            }
        }
        self.tunables.record_wait(wait_started.elapsed());
        pending.remove(&index)
    }

    /// Re-evaluate the lag guard between blocks of a long call, from the 1 Hz
    /// Core observation, so a pruning Core cannot outrun the index while a
    /// 512-block call is in progress.
    fn live_lag_check(&self, info: &BlockchainInfo, committed: u64, last_check: &mut Instant) {
        let auto_prune = self.cfg.core.prune_mib >= 550;
        let guarded =
            auto_prune || (self.cfg.core.prune_mib == 1 && self.cfg.pruning.max_bulk_lag_gib > 0);
        if !guarded || !info.pruned || last_check.elapsed() < Duration::from_millis(100) {
            return;
        }
        // Use the 1 Hz observation. Auto-prune keeps a synchronous fallback
        // (at most every 2 s) because Core could otherwise outrun a stale
        // snapshot.
        let max_age = if auto_prune {
            Duration::from_secs(2)
        } else {
            Duration::from_secs(5)
        };
        let live = self.observed_core(max_age).or_else(|| {
            if auto_prune {
                self.core.blockchain_info().ok()
            } else {
                None
            }
        });
        if let Some(live) = live {
            self.stats_core_height(live.blocks);
            self.publish_core_state(&live);
            // Pause decisions use the last *committed* tip, not
            // applied-but-uncommitted progress.
            if live.pruneheight > committed.saturating_add(1) {
                eprintln!(
                    "[e-lite] WARNING: Core pruned to {} but the index is at {}; \
                     blocks {}..{} are no longer available",
                    live.pruneheight,
                    committed,
                    committed + 1,
                    live.pruneheight.saturating_sub(1)
                );
            }
            self.lag_guard(&live, committed);
        }
        *last_check = Instant::now();
    }

    /// Whether the group must be committed now, and why. Bulk commits are
    /// deliberately infrequent: the prune gate uses the *committed* tip, so a
    /// 20-60 s interval cannot lose data that Core has already pruned. A tiny
    /// group is not committed on the timer; it waits for a useful minimum (or
    /// 60 s) to avoid 1-block commits.
    fn commit_reason(&self, group: &CommitGroup) -> Option<CommitReason> {
        let elapsed = group.started.elapsed();
        if group.blocks >= self.tunables.batch_blocks().max(1) {
            Some(CommitReason::Blocks)
        } else if group.txids.len() >= self.tunables.max_group_txs() {
            Some(CommitReason::Txs)
        } else if group.bytes >= self.tunables.max_group_bytes() {
            Some(CommitReason::Bytes)
        } else if elapsed >= self.tunables.commit_interval()
            && (group.blocks >= self.tunables.commit_min_blocks()
                || elapsed >= Duration::from_secs(60))
        {
            Some(CommitReason::Time)
        } else {
            None
        }
    }

    /// Commit the group at `tip`, publish the new state (rates, `/status`,
    /// progress line, subscriptions) and leave the group empty.
    fn flush_group(
        &self,
        group: &mut CommitGroup,
        tip: &IndexedTip,
        info: &BlockchainInfo,
        reason: CommitReason,
    ) -> R<()> {
        let sync = self.commit_sync(info.initialblockdownload, tip.height as u64, info.blocks);
        self.commit_group(
            std::mem::take(&mut group.batch),
            tip,
            &group.txids,
            group.blocks,
            group.bytes as u64,
            group.last_ms,
            group.undo.make_contiguous(),
            sync,
            reason,
        )?;
        let (live_core, live_prune) = self.live_core_and_prune(info.blocks, info.pruneheight);
        let rate_secs = self.progress_started.elapsed().as_secs_f64().max(0.001);
        self.tunables.record_tx_rate(
            ((tip.txnum.saturating_sub(self.progress_start_txnum)) as f64 / rate_secs) as u64,
        );
        self.tunables.record_blocks_rate(
            ((tip.height as u64).saturating_sub(self.progress_start_height) as f64 / rate_secs)
                as u64,
        );
        if let Some(sh) = &self.shared {
            self.tunables.publish(&self.store, &sh.stats);
        }
        self.print_progress(
            tip,
            live_core,
            live_prune,
            info.headers,
            info.verificationprogress,
            info.initialblockdownload,
        );
        self.maybe_milestone(tip);
        if let Some(sh) = &self.shared {
            for script in group.scripts.drain() {
                sh.dirty_scripts.mark(script);
            }
        }
        group.reset();
        Ok(())
    }

    /// Count the work of one call and make a call without progress visible.
    fn account_call(&self, call: &CallSummary) {
        if let Some(sh) = &self.shared {
            sh.stats
                .pipeline_prepared
                .fetch_add(call.applied + call.discarded, Ordering::Relaxed);
            sh.stats
                .pipeline_applied
                .fetch_add(call.applied, Ordering::Relaxed);
            sh.stats
                .pipeline_discarded
                .fetch_add(call.discarded, Ordering::Relaxed);
        }
        // Make a no-progress call impossible to miss: this is exactly the tail
        // signature (calls every 500 ms, wanted 2, applied 0, no commits, no
        // READY). Rate-limited so a real stall prints ~6 lines per minute.
        if call.wanted < 64 && call.applied == 0 {
            let note_at = self.last_pipeline_note.lock().map(|mut s| {
                let due = s
                    .map(|at| at.elapsed() >= Duration::from_secs(10))
                    .unwrap_or(true);
                if due {
                    *s = Some(Instant::now());
                }
                due
            });
            if note_at.unwrap_or(false) {
                crate::debug_log!(
                    "pipeline: no progress: committed {} of target {} (wanted {}, \
                     applied 0, discarded {}, paused_by_disk {}, disk_guard {})",
                    call.committed,
                    call.target,
                    call.wanted,
                    call.discarded,
                    call.paused_by_disk,
                    self.disk_guard()
                );
            }
        } else if call.wanted >= 64 {
            crate::debug_log!(
                "pipeline: call end committed {} (wanted {}, applied {}, discarded {})",
                call.committed,
                call.wanted,
                call.applied,
                call.discarded
            );
        }
    }

    /// Index at most `max_blocks` before returning. The supervisor runs at
    /// commit boundaries, so a long catch-up does not need to tear the
    /// pipeline down to prune or check the disk.
    fn catch_up_limited(
        &self,
        max_blocks: u64,
        supervisor: &mut el_chain::Supervisor,
        last_tick: &mut Instant,
    ) -> R<u64> {
        let (info, mut tip) = match self.preflight()? {
            Preflight::Idle(height) => return Ok(height),
            Preflight::Ready { info, tip } => (info, tip),
        };
        let core_tip = info.blocks;
        let mut committed_height = tip.map(|t| t.height as u64).unwrap_or(0);

        let start = tip.map(|t| t.height as u64 + 1).unwrap_or(0);
        if start > core_tip || max_blocks == 0 {
            self.refresh_core_height();
            return Ok(committed_height);
        }
        let target = core_tip.min(start.saturating_add(max_blocks.saturating_sub(1)));
        let wanted = target - start + 1;
        let hashes = self.core.block_hashes(start, wanted)?;
        if hashes.is_empty() {
            crate::debug_log!(
                "pipeline: block_hashes({start}, {wanted}) returned empty; core tip {core_tip}"
            );
            self.refresh_core_height();
            return Ok(committed_height);
        }
        if wanted >= 64 {
            // Real-work calls only: at the tail the calls repeat every 500 ms
            // and the end-note (rate-limited) carries the diagnosis.
            crate::debug_log!(
                "pipeline: call start {} target {} wanted {} (bulk {})",
                start,
                target,
                wanted,
                self.bulk.is_some()
            );
        }
        let hashes_len = hashes.len();
        let pool = self.spawn_readers(start, hashes, info.pruneheight);

        let mut group = CommitGroup::new();
        let mut applied_total = 0u64;
        let mut result: R<()> = Ok(());
        let mut paused_by_disk = false;
        let mut last_lag_check = Instant::now();
        // Give the supervisor a chance to prune/act even when a 512-block
        // catch-up call is slow; this bounds prune latency independently of
        // the number of blocks processed.
        let supervisor_poll = Duration::from_secs(self.cfg.pruning.poll_seconds.max(1));
        let track_scripts = self.shared.is_some();

        let mut pending: PendingMap = PendingMap::new();
        let mut next_apply = 0usize;
        while next_apply < hashes_len {
            // A prune requested at the previous commit boundary starts here,
            // after the commit and its WAL barrier. It never runs in the middle
            // of an uncommitted group.
            self.run_deferred_prune(supervisor);
            let Some(item) = self.next_prepared(&pool, &mut pending, next_apply, start) else {
                break;
            };
            next_apply += 1;
            if self.shutdown.load(Ordering::SeqCst) {
                break;
            }
            if self.is_paused() || self.disk_guard() {
                paused_by_disk = true;
                break;
            }
            let (height, prepared, prevouts) = match item {
                Ok(v) => v,
                Err(e) => {
                    result = Err(e);
                    break;
                }
            };
            let t0 = Instant::now();
            // Undo is built for every block: a commit forced by a time or
            // transaction limit, a boundary or shutdown must be as rewindable
            // as a full batch. Only the reorg window is written at commit.
            match apply_prepared_deferred_undo(
                &self.store,
                height as u32,
                &prepared,
                tip,
                &mut group.overlay,
                &mut group.batch,
                Some(&prevouts),
            ) {
                Ok(out) => {
                    tip = Some(out.tip);
                    let raw_size = prepared.raw_size();
                    group.push(
                        height as u32,
                        out,
                        raw_size,
                        self.cfg.index.reorg_window,
                        track_scripts,
                    );
                    applied_total += 1;
                    self.tunables.record_block_bytes(raw_size);
                    group.last_ms = t0.elapsed().as_millis() as u64;
                    self.tunables.record_apply(t0.elapsed());
                    self.live_lag_check(&info, committed_height, &mut last_lag_check);
                }
                Err(e) => {
                    result = Err(e.into());
                    break;
                }
            }
            pool.release();
            if let Some(reason) = self.commit_reason(&group) {
                let current = tip.expect("tip set");
                if let Err(e) = self.flush_group(&mut group, &current, &info, reason) {
                    result = Err(e);
                    break;
                }
                committed_height = current.height as u64;
                if last_tick.elapsed() >= supervisor_poll {
                    self.supervisor_step(supervisor);
                    *last_tick = Instant::now();
                }
            }
        }

        // A clean end-of-call always commits the applied prefix. The batch is
        // already built and undo is kept for every applied block, so the
        // commit is the cheap step; skipping it made the tail refetch the same
        // blocks every 500 ms call and stall forever a few blocks behind the
        // tip (no commits, no READY). The disk guard still stops the *next*
        // call from fetching more.
        if result.is_ok() && group.blocks > 0 {
            if paused_by_disk || self.disk_guard() {
                crate::debug_log!(
                    "pipeline: committing the applied prefix ({} blocks) under \
                     disk pressure before pausing",
                    group.blocks
                );
            }
            let current = tip.expect("tip set");
            let shutting_down = self.shutdown.load(Ordering::SeqCst);
            let reason = if shutting_down {
                CommitReason::Shutdown
            } else {
                CommitReason::Boundary
            };
            self.flush_group(&mut group, &current, &info, reason)?;
            committed_height = current.height as u64;
            if shutting_down {
                eprintln!(
                    "[e-lite] shutdown requested; flushed at height {}",
                    current.height
                );
            }
        }

        // Work prepared but never applied is waste. It is expected at the end
        // of a 512-block call (the window in flight), not every 10 s.
        let discarded = pending.len() as u64 + pool.finish();
        self.account_call(&CallSummary {
            wanted,
            target,
            applied: applied_total,
            discarded,
            committed: committed_height,
            paused_by_disk,
        });
        result?;
        self.refresh_core_height();
        Ok(committed_height)
    }
}

/// Outcome of the checks that precede a catch-up call.
enum Preflight {
    /// Nothing to index now; the committed height.
    Idle(u64),
    Ready {
        info: BlockchainInfo,
        tip: Option<IndexedTip>,
    },
}

/// What one catch-up call did, for the counters and the debug notes.
struct CallSummary {
    wanted: u64,
    target: u64,
    applied: u64,
    discarded: u64,
    committed: u64,
    paused_by_disk: bool,
}

/// Blocks applied but not yet committed: one RocksDB batch plus what the
/// commit needs besides it.
struct CommitGroup {
    batch: WriteBatch,
    overlay: GroupOverlay,
    txids: Vec<Hash32>,
    /// Scripts touched by the group, marked dirty for subscriptions at commit.
    scripts: std::collections::HashSet<ScriptHash>,
    /// Undo of the applied blocks, trimmed to the reorg window as the group
    /// grows. Only these are written at commit (AUD-131); bulk blocks far
    /// below the tip never reach RocksDB.
    undo: std::collections::VecDeque<el_model::UndoRecord>,
    blocks: u32,
    bytes: usize,
    /// Apply time of the last block, for `/status`.
    last_ms: u64,
    started: Instant,
}

impl CommitGroup {
    fn new() -> Self {
        CommitGroup {
            batch: WriteBatch::default(),
            overlay: GroupOverlay::new(),
            txids: Vec::new(),
            scripts: std::collections::HashSet::new(),
            undo: std::collections::VecDeque::new(),
            blocks: 0,
            bytes: 0,
            last_ms: 0,
            started: Instant::now(),
        }
    }

    /// Account one applied block (its writes are already in `batch`).
    fn push(
        &mut self,
        height: u32,
        out: el_index::ApplyOutcome,
        raw_size: usize,
        undo_window: u32,
        track_scripts: bool,
    ) {
        self.txids.extend_from_slice(&out.txids);
        let floor = height.saturating_sub(undo_window);
        while self.undo.front().is_some_and(|u| u.height < floor) {
            self.undo.pop_front();
        }
        self.undo.push_back(out.undo);
        if track_scripts {
            self.scripts.extend(out.touched_scripts);
        }
        self.blocks += 1;
        self.bytes = self.bytes.saturating_add(raw_size);
    }

    /// Start the next group after a commit.
    fn reset(&mut self) {
        self.batch = WriteBatch::default();
        self.overlay.clear();
        self.txids.clear();
        self.scripts.clear();
        self.undo.clear();
        self.blocks = 0;
        self.bytes = 0;
        self.started = Instant::now();
    }
}

pub(crate) fn fetch_block(
    core: &CoreClient,
    bulk: &Option<Arc<el_blocks::BulkSource>>,
    resolver: Option<&Resolver>,
    pruneheight: u64,
    height: u64,
    hash: &str,
) -> std::result::Result<(u64, String, Vec<u8>), E> {
    let expected = Hash32::from_hex(hash).map_err(|e| e.to_string())?;

    // A prune gap cannot be served by Core or the local block files. Fetch the
    // missing block from a verified cache/P2P/Esplora source instead. This path
    // is never used for heights Core can still serve.
    let mut repair_error = None;
    if height.saturating_add(1) <= pruneheight {
        if let Some(resolver) = resolver {
            match resolver.fetch_block_by_hash(&expected) {
                Ok(bytes) => {
                    eprintln!(
                        "[e-lite] repaired pruned block {height} ({hash}) from a verified source"
                    );
                    return Ok((height, hash.to_string(), bytes));
                }
                Err(e) => repair_error = Some(e.to_string()),
            }
        }
    }

    if let Some(bulk) = bulk {
        if let Ok(Some(bytes)) = bulk.fetch(&expected) {
            return Ok((height, hash.to_string(), bytes));
        }
        // Give the scanner a short window to publish a record Core has just
        // written, then fall back to RPC.
        if bulk
            .wait_for(&expected, Duration::from_millis(50))
            .is_some()
        {
            match bulk.fetch(&expected) {
                Ok(Some(bytes)) => return Ok((height, hash.to_string(), bytes)),
                Ok(None) => {}
                Err(e) => eprintln!("[e-lite] bulk read failed at {height}: {e}"),
            }
        }
    }
    match core.block_hex(hash) {
        Ok(raw) => {
            let bytes = hex::decode(&raw).map_err(|e| e.to_string())?;
            Ok((height, hash.to_string(), bytes))
        }
        Err(e) => {
            let msg = e.to_string();
            match repair_error {
                // Core pruned it and no verified source has it: retrying
                // cannot bring the block back.
                Some(repair) => fatal(format!("{msg}; repair failed: {repair}")),
                None => Err(msg.into()),
            }
        }
    }
}

pub(crate) fn commit_sync_policy(mode: &str, ibd: bool, height: u64, core_tip: u64) -> bool {
    match mode {
        "always" => true,
        "txids" => false,
        _ => !(ibd || height.saturating_add(1_000) < core_tip),
    }
}

pub(crate) fn dir_usage_bytes(path: &std::path::Path) -> u64 {
    fn walk(p: &std::path::Path, acc: &mut u64) {
        let Ok(rd) = std::fs::read_dir(p) else {
            return;
        };
        for entry in rd.flatten() {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                walk(&entry.path(), acc);
            } else if meta.is_file() {
                *acc = acc.saturating_add(meta.len());
            }
        }
    }
    let mut acc = 0;
    walk(path, &mut acc);
    acc
}

/// A pruned Core can serve only if the local index already covers the gap.
pub(crate) fn check_pruned(
    info: &BlockchainInfo,
    tip: Option<&IndexedTip>,
    repair_max_gap: Option<u64>,
) -> R<()> {
    if !info.pruned {
        return Ok(());
    }
    let tip = match tip {
        Some(t) => t,
        None => {
            // A local managed Core can bootstrap while it has not pruned yet.
            if info.pruneheight == 0 {
                return Ok(());
            }
            return fatal(
                "Core is pruned and the index is empty: use a full node, or let e-lite own Core \
                 with [core] prune_mib = 1 (manual pruning) so it can index before pruning",
            );
        }
    };
    if (tip.height as u64) + 1 < info.pruneheight {
        let first = tip.height as u64 + 1;
        let gap = info.pruneheight.saturating_sub(first);
        if let Some(max) = repair_max_gap {
            if gap <= max {
                eprintln!(
                    "[e-lite] prune gap of {gap} blocks ({first}..{}): fetching verified blocks \
                     from the repair sources",
                    info.pruneheight.saturating_sub(1)
                );
                return Ok(());
            }
        }
        return fatal(format!(
            "Core is pruned from height {} but the index stops at {}; blocks {}..{} are unavailable \
             (gap {}). Use a full node, enable a verified P2P/Esplora repair source, or use \
             [core] prune_mib = 1 so Core cannot create this gap.",
            info.pruneheight,
            tip.height,
            first,
            info.pruneheight.saturating_sub(1),
            gap
        ));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A Core `getblockchaininfo` answer for the prune checks (also used by
    /// the error-classification test in main.rs).
    pub(crate) fn info(pruned: bool, pruneheight: u64) -> BlockchainInfo {
        BlockchainInfo {
            chain: "signet".to_string(),
            blocks: 1000,
            headers: 1000,
            pruned,
            pruneheight,
            prune_target_size: 0,
            size_on_disk: 0,
            initialblockdownload: false,
            signet_challenge: None,
            verificationprogress: 1.0,
        }
    }

    pub(crate) fn tip(height: u32) -> IndexedTip {
        IndexedTip {
            height,
            block_hash: Hash32::ZERO,
            schema_version: el_model::codec::SCHEMA_VERSION,
            txnum: 0,
        }
    }

    #[test]
    fn full_node_allows_any_tip() {
        assert!(check_pruned(&info(false, 0), None, None).is_ok());
    }

    #[test]
    fn pruned_with_empty_index_is_rejected() {
        assert!(check_pruned(&info(true, 100), None, None).is_err());
    }

    #[test]
    fn pruned_gap_is_rejected() {
        // Index stops at 50, Core only has blocks from 100 onwards.
        assert!(check_pruned(&info(true, 100), Some(&tip(50)), None).is_err());
        assert!(check_pruned(&info(true, 100), Some(&tip(98)), None).is_err());
        // The same gap is allowed when a verified repair source is available.
        assert!(check_pruned(&info(true, 100), Some(&tip(98)), Some(2)).is_ok());
    }

    #[test]
    fn pruned_with_covered_tip_is_allowed() {
        assert!(check_pruned(&info(true, 100), Some(&tip(99)), None).is_ok());
        assert!(check_pruned(&info(true, 100), Some(&tip(150)), None).is_ok());
    }

    #[test]
    fn commit_sync_auto_relaxes_only_in_bulk() {
        assert!(!commit_sync_policy("auto", true, 100_000, 200_000));
        assert!(!commit_sync_policy("auto", false, 100_000, 200_000));
        assert!(commit_sync_policy("auto", false, 200_000, 200_000));
        assert!(commit_sync_policy("always", true, 100_000, 200_000));
        assert!(!commit_sync_policy("txids", false, 200_000, 200_000));
    }

    #[test]
    fn undo_window_is_measured_from_the_real_commit_tip() {
        // AUD-131: only the window of the tip actually committed is written.
        let dir = std::env::temp_dir().join(format!(
            "e-lite-undo-window-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = Store::open(&dir).unwrap();
        let window = 288u32;
        let records: Vec<el_model::UndoRecord> = (600..=1_200u32)
            .map(|height| el_model::UndoRecord {
                height,
                ..Default::default()
            })
            .collect();
        // A group that commits early at 1159 (time limit) still writes undo
        // for 1158 down to 1159 - 288, and nothing above the committed tip.
        let mut batch = WriteBatch::default();
        let written = write_window_undo(&store, &mut batch, &records, 1_159, window).unwrap();
        assert_eq!(written, window as usize + 1);
        // A tip below the window keeps everything up to the tip.
        let low: Vec<el_model::UndoRecord> = (0..=100u32)
            .map(|height| el_model::UndoRecord {
                height,
                ..Default::default()
            })
            .collect();
        let mut batch = WriteBatch::default();
        assert_eq!(
            write_window_undo(&store, &mut batch, &low, 100, window).unwrap(),
            101
        );
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn directory_usage_counts_regular_files() {
        let dir = std::env::temp_dir().join(format!(
            "e-lite-dir-usage-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a"), vec![0u8; 100]).unwrap();
        std::fs::write(dir.join("sub/b"), vec![0u8; 50]).unwrap();
        assert_eq!(dir_usage_bytes(&dir), 150);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
