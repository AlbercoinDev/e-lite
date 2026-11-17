//! pipeline module (split from main.rs in alpha.29.17.0).

use super::*;

// ------------------------------------------------------------------- indexing

pub(crate) const CATCH_UP_BLOCKS: u64 = 512;
pub(crate) const GIB: u64 = 1024 * 1024 * 1024;

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
    pub(crate) bulk_lag_baseline: AtomicU64,
    pub(crate) bulk_started: AtomicBool,
    pub(crate) bulk_full_pause: AtomicBool,
    /// `true` when `[pruning] scheduler = "alternate"`: Core downloads and
    /// e-lite indexes in alternating phases. `false` keeps the overlap.
    pub(crate) scheduler_alternate: bool,
    /// Byte watermark confirmation: a pause/resume must persist before acting.
    pub(crate) high_since: Mutex<Option<Instant>>,
    pub(crate) low_since: Mutex<Option<Instant>>,
    /// The alternate scheduler has paused indexing (Core is downloading).
    /// Serving (Electrum, Tor, status) continues.
    pub(crate) index_paused: AtomicBool,
    /// 0 = overlap/indexing, 1 = Core paused/e-lite draining, 2 = Core phase.
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
        let requested_alternate = cfg.pruning.scheduler.eq_ignore_ascii_case("alternate");
        let scheduler_alternate = requested_alternate && can_control_core;
        if requested_alternate && !can_control_core {
            eprintln!(
                "[e-lite] WARNING: scheduler = \"alternate\" needs a Core that e-lite can pause; \
                 this Core is external/already running. Keeping overlap."
            );
        }
        if let Some(sh) = &shared {
            sh.stats
                .scheduler_alternate
                .store(scheduler_alternate, Ordering::Relaxed);
            sh.stats.scheduler_high_bytes.store(
                cfg.pruning
                    .max_bulk_lag_gib
                    .saturating_mul(1024 * 1024 * 1024),
                Ordering::Relaxed,
            );
            sh.stats.scheduler_low_bytes.store(
                cfg.pruning
                    .core_resume_ahead_gib
                    .saturating_mul(1024 * 1024 * 1024),
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
            bulk_lag_baseline: AtomicU64::new(u64::MAX),
            bulk_started: AtomicBool::new(false),
            bulk_full_pause: AtomicBool::new(false),
            scheduler_alternate,
            high_since: Mutex::new(None),
            low_since: Mutex::new(None),
            index_paused: AtomicBool::new(false),
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

    /// Disk backpressure with two levels:
    ///
    /// * below `min_free_gib`: pause Core downloads, but keep indexing the
    ///   blocks already on disk. The index can then advance and the prune gate
    ///   can delete old block files, which is what actually frees space.
    /// * below `critical_free_gib`: also pause indexing, to avoid filling the
    ///   index filesystem. Indexing resumes as soon as free space recovers
    ///   above the critical floor, even while downloads remain paused.
    ///
    /// This check is independent of Core RPC so it still works while Core is
    /// loading its block index or is unreachable.
    fn disk_guard(&self) -> bool {
        let low = disk_low_bytes(&self.cfg);
        if low == 0 {
            return false;
        }
        {
            let Ok(mut last) = self.last_disk_check.lock() else {
                return false;
            };
            if last.elapsed() < Duration::from_secs(1) {
                return self.is_paused();
            }
            *last = Instant::now();
        }
        let free = [
            el_chain::disk_free_bytes(&self.data_dir),
            el_chain::disk_free_bytes(&self.core_datadir),
            el_chain::disk_free_bytes(&self.blocks_dir),
        ]
        .into_iter()
        .flatten()
        .min();
        let Some(free) = free else {
            return false;
        };
        let critical = disk_critical_bytes(&self.cfg);
        let resume = disk_resume_bytes(&self.cfg);

        // Enough space: resume indexing first, then downloads.
        if free >= resume {
            if self.is_paused() {
                self.set_paused(false);
                if !self.can_control_core {
                    if let Some(sh) = &self.shared {
                        sh.stats.disk_low.store(false, Ordering::Relaxed);
                    }
                }
                eprintln!(
                    "[e-lite] disk recovered: {} MiB free; resumed indexing",
                    free / (1024 * 1024)
                );
            }
            if self.downloads_paused() {
                self.resume_downloads("disk recovered");
            }
            return false;
        }

        // Hard floor: both writers stop.
        if free < critical {
            if !self.is_paused() {
                self.set_paused(true);
                if let Some(sh) = &self.shared {
                    sh.stats.disk_low.store(true, Ordering::Relaxed);
                    sh.set_state(state::PAUSE_DOWNLOAD);
                }
                eprintln!(
                    "[e-lite] disk critical: {} MiB free (floor {} MiB); paused indexing",
                    free / (1024 * 1024),
                    critical / (1024 * 1024)
                );
            }
            if self.can_control_core {
                self.pause_downloads("disk critical");
            }
            return true;
        }

        // Below the download watermark: stop new blocks. A managed Core keeps
        // draining its backlog (and pruning can free space); an external Core
        // cannot be paused, so indexing stops as well.
        if free < low {
            if !self.can_control_core {
                if !self.is_paused() {
                    self.set_paused(true);
                    if let Some(sh) = &self.shared {
                        sh.stats.disk_low.store(true, Ordering::Relaxed);
                        sh.set_state(state::PAUSE_DOWNLOAD);
                    }
                    eprintln!(
                        "[e-lite] disk low: {} MiB free (threshold {} MiB); paused indexing (Core is external)",
                        free / (1024 * 1024),
                        low / (1024 * 1024)
                    );
                }
                return true;
            }
            // Between the critical floor and the low watermark: resume
            // indexing (drain the backlog) while Core downloads stay paused.
            if self.is_paused() {
                self.set_paused(false);
                eprintln!(
                    "[e-lite] disk above critical floor: {} MiB free; resumed indexing while downloads stay paused",
                    free / (1024 * 1024)
                );
            }
            self.pause_downloads("disk low");
            return false;
        }

        // Between `low` and `resume`: do not start a new pause, and keep an
        // existing pause (hysteresis) until `resume` is reached. Cold starts in
        // this band must not pause at all.
        self.is_paused()
    }

    fn downloads_paused(&self) -> bool {
        self.downloads_paused.load(Ordering::SeqCst)
    }

    /// Pause Core's P2P downloads without stopping indexing. The marker lets a
    /// later e-lite run restore `networkactive` after a crash.
    fn pause_downloads(&self, reason: &str) {
        if self.downloads_paused.swap(true, Ordering::SeqCst) {
            return;
        }
        // Confirm the RPC before claiming the pause; otherwise a failed request
        // would leave the internal state lying and future retries would be
        // skipped.
        if self.can_control_core {
            if let Err(e) = self.core.set_network_active(false) {
                self.downloads_paused.store(false, Ordering::SeqCst);
                eprintln!("[e-lite] cannot pause Core downloads: {e}; will retry");
                return;
            }
        }
        self.mark_paused(true);
        if let Some(sh) = &self.shared {
            sh.stats.core_network_active.store(false, Ordering::Relaxed);
            sh.stats.disk_low.store(true, Ordering::Relaxed);
            sh.set_state(state::PAUSE_DOWNLOAD);
        }
        eprintln!("[e-lite] {reason}: paused Core downloads");
    }

    fn resume_downloads(&self, reason: &str) {
        if !self.downloads_paused.swap(false, Ordering::SeqCst) {
            return;
        }
        if self.lag_paused.load(Ordering::SeqCst) {
            // A live lag pause still owns the network; keep the marker.
            if let Some(sh) = &self.shared {
                sh.stats.disk_low.store(false, Ordering::Relaxed);
            }
            eprintln!("[e-lite] {reason}: disk pressure cleared; Core stays paused by lag guard");
            return;
        }
        if self.can_control_core {
            if let Err(e) = self.core.set_network_active(true) {
                self.downloads_paused.store(true, Ordering::SeqCst);
                eprintln!("[e-lite] cannot resume Core downloads: {e}; will retry");
                return;
            }
        }
        self.mark_paused(false);
        if let Some(sh) = &self.shared {
            sh.stats.core_network_active.store(true, Ordering::Relaxed);
            sh.stats.disk_low.store(false, Ordering::Relaxed);
            sh.set_state(state::BUILDING);
        }
        eprintln!("[e-lite] {reason}: resumed Core downloads");
    }

    fn mark_paused(&self, paused: bool) {
        if paused {
            let _ = std::fs::write(
                &self.pause_marker,
                format!("pid={} at={:?}\n", std::process::id(), Instant::now()),
            );
        } else {
            let _ = std::fs::remove_file(&self.pause_marker);
        }
    }

    fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::SeqCst);
    }

    fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
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

    /// A watermark acts only after it has held for `secs`: `true` once
    /// `active` has been continuously set that long. `false` resets it.
    fn dwell_reached(&self, slot: &Mutex<Option<Instant>>, active: bool, secs: u64) -> bool {
        let Ok(mut state) = slot.lock() else {
            return false;
        };
        if !active {
            *state = None;
            return false;
        }
        let now = Instant::now();
        match *state {
            None => {
                *state = Some(now);
                false
            }
            Some(started) => now.duration_since(started) >= Duration::from_secs(secs),
        }
    }

    /// Track the current scheduler phase and its rate. `phase`: 0 overlap,
    /// 1 Core paused/e-lite draining, 2 Core downloading/e-lite paused.
    fn update_phase(&self, phase: u64, committed: u64) {
        let changed = self.phase.swap(phase, Ordering::Relaxed) != phase;
        if changed {
            // The byte window is easier to audit with the arithmetic in the
            // log: backlog = lag x average block size, compared with the
            // configured pause/resume marks.
            let lag = self.tunables.core_height().saturating_sub(committed);
            let avg = self.tunables.avg_block_bytes();
            let backlog_gib = lag.saturating_mul(avg) / GIB;
            let name = match phase {
                1 => "draining",
                2 => "core",
                _ => "indexing",
            };
            crate::debug_log!(
                "scheduler: phase {} (lag {} blocks x avgblk {} KiB = backlog {} GiB; \
                 pause at {} GiB, resume at {} GiB)",
                name,
                lag,
                avg / 1024,
                backlog_gib,
                self.cfg.pruning.max_bulk_lag_gib,
                self.cfg.pruning.core_resume_ahead_gib
            );
        }
        let (blocks, elapsed_ms) = if let Ok(mut state) = self.phase_state.lock() {
            if changed || state.is_none() {
                *state = Some((Instant::now(), committed));
            }
            if let Some((started, start_height)) = *state {
                (
                    committed.saturating_sub(start_height),
                    started.elapsed().as_millis() as u64,
                )
            } else {
                (0, 0)
            }
        } else {
            (0, 0)
        };
        if let Some(sh) = &self.shared {
            sh.stats.scheduler_phase.store(phase, Ordering::Relaxed);
            sh.stats
                .scheduler_phase_blocks
                .store(blocks, Ordering::Relaxed);
            sh.stats
                .scheduler_phase_elapsed_ms
                .store(elapsed_ms, Ordering::Relaxed);
        }
    }

    /// Pause/resume e-lite indexing for the `alternate` scheduler. Serving
    /// (Electrum, Tor, metrics) continues; only block application stops.
    fn set_index_paused(&self, paused: bool) {
        if !self.scheduler_alternate {
            if self.index_paused.swap(false, Ordering::SeqCst) {
                if let Some(sh) = &self.shared {
                    sh.stats
                        .scheduler_index_paused
                        .store(false, Ordering::Relaxed);
                }
            }
            return;
        }
        let previous = self.index_paused.swap(paused, Ordering::SeqCst);
        if previous == paused {
            return;
        }
        if let Some(sh) = &self.shared {
            sh.stats
                .scheduler_index_paused
                .store(paused, Ordering::Relaxed);
        }
        if paused {
            eprintln!(
                "[e-lite] scheduler alternate: e-lite indexing paused; Core downloading \
                 (serving continues)"
            );
        } else {
            eprintln!("[e-lite] scheduler alternate: Core paused; e-lite indexing");
        }
    }

    /// The lag guard no longer applies (no pruning to protect, IBD finished):
    /// hand the network back to Core if this guard was the one holding it.
    /// A disk pause keeps it; a failed RPC is retried on the next call.
    fn release_lag_pause(&self, paused: bool, message: &str) {
        if paused && !self.downloads_paused() && self.core.set_network_active(true).is_ok() {
            self.lag_paused.store(false, Ordering::Relaxed);
            self.mark_paused(false);
            if let Some(sh) = &self.shared {
                sh.stats.core_network_active.store(true, Ordering::Relaxed);
                refresh_ready_state(sh);
            }
            eprintln!("[e-lite] {message}");
        }
    }

    /// Pause Core's P2P when automatic pruning would outrun the index, or when
    /// the bulk guard is enabled for manual pruning. Core keeps at least
    /// `MIN_BLOCKS_TO_KEEP` (288) blocks, so the auto limit is capped below it.
    ///
    /// For manual pruning the guard uses absolute byte watermarks
    /// (`max_bulk_lag_gib`/`core_resume_ahead_gib`) over the estimated
    /// unindexed backlog. The legacy block window is used only when the GiB
    /// value is 0. In `alternate` mode, e-lite also stops indexing while Core
    /// downloads and resumes when Core is paused.
    fn lag_guard(&self, info: &BlockchainInfo, committed: u64) {
        if !self.can_control_core {
            return;
        }
        let paused = self.lag_paused.load(Ordering::Relaxed);
        let lag = info.blocks.saturating_sub(committed);
        let avg = self.tunables.avg_block_bytes();
        let backlog = backlog_bytes(lag, avg);
        let bulk_blocks = if self.cfg.core.prune_mib == 1 {
            self.cfg.pruning.max_bulk_lag_blocks as u64
        } else {
            0
        };
        let bulk_gib = if self.cfg.core.prune_mib == 1 {
            self.cfg.pruning.max_bulk_lag_gib
        } else {
            0
        };
        let bulk = self.cfg.core.prune_mib == 1 && (bulk_blocks > 0 || bulk_gib > 0);
        let auto_enabled = self.cfg.core.prune_mib != 1 && self.cfg.pruning.max_core_lag_blocks > 0;
        let high_bytes = (bulk_gib > 0).then(|| bulk_gib.saturating_mul(GIB));
        let low_bytes =
            (bulk_gib > 0).then(|| self.cfg.pruning.core_resume_ahead_gib.saturating_mul(GIB));
        if let Some(sh) = &self.shared {
            sh.stats
                .scheduler_backlog_bytes
                .store(backlog, Ordering::Relaxed);
            sh.stats
                .scheduler_high_bytes
                .store(high_bytes.unwrap_or(0), Ordering::Relaxed);
            sh.stats
                .scheduler_low_bytes
                .store(low_bytes.unwrap_or(0), Ordering::Relaxed);
            sh.stats
                .scheduler_avg_block_bytes
                .store(avg, Ordering::Relaxed);
        }
        let reader_pressure = self
            .readers
            .as_ref()
            .and_then(|r| r.earliest())
            .map(|h| (h as u64) < info.pruneheight.saturating_add(32))
            .unwrap_or(false);

        if !bulk && !auto_enabled {
            self.bulk_lag_baseline.store(u64::MAX, Ordering::Relaxed);
            self.set_index_paused(false);
            self.release_lag_pause(paused, "resumed Core downloads");
            return;
        }

        let mut is_bulk = false;
        let mut bulk_full = false;
        let pause_due;
        let resume_due;
        let limit;
        let resume;

        if bulk {
            is_bulk = true;
            if !info.initialblockdownload {
                self.bulk_lag_baseline.store(u64::MAX, Ordering::Relaxed);
                self.bulk_started.store(false, Ordering::Relaxed);
                self.bulk_full_pause.store(false, Ordering::Relaxed);
                self.set_index_paused(false);
                self.release_lag_pause(paused, "resumed Core downloads after IBD");
                return;
            }
            if let (Some(high), Some(low)) = (high_bytes, low_bytes) {
                let high_blocks = (high / avg.max(1)).max(1);
                let low_blocks = (low / avg.max(1)).max(1);
                if self.cfg.pruning.pause_core_during_bulk {
                    let first = !self.bulk_started.swap(true, Ordering::SeqCst);
                    if first && backlog >= high {
                        self.bulk_full_pause.store(true, Ordering::SeqCst);
                        self.tunables.reset_resume_shrink();
                    }
                } else {
                    self.bulk_started.store(true, Ordering::Relaxed);
                }
                bulk_full = self.bulk_full_pause.load(Ordering::SeqCst);
                limit = high_blocks;
                resume = low_blocks;
                let (raw_pause, raw_resume) = byte_window(backlog, high, low, reader_pressure);
                pause_due = if reader_pressure {
                    true
                } else {
                    self.dwell_reached(&self.high_since, raw_pause, 10)
                };
                resume_due = self.dwell_reached(&self.low_since, raw_resume, 10);
                if pause_due {
                    let _ = self.dwell_reached(&self.low_since, false, 10);
                }
            } else {
                let ahead = self.cfg.pruning.core_resume_ahead_blocks as u64;
                if self.cfg.pruning.pause_core_during_bulk {
                    let first = !self.bulk_started.swap(true, Ordering::SeqCst);
                    if first && lag > ahead {
                        self.bulk_full_pause.store(true, Ordering::SeqCst);
                        self.tunables.reset_resume_shrink();
                    }
                } else {
                    self.bulk_started.store(true, Ordering::Relaxed);
                }
                if self.bulk_full_pause.load(Ordering::SeqCst) {
                    bulk_full = true;
                    limit = ahead.max(1);
                    resume = ahead.max(1);
                } else {
                    let baseline = self.bulk_lag_baseline.load(Ordering::Relaxed);
                    let (new_baseline, pause_at, resume_at) =
                        bulk_lag_window(lag, baseline, bulk_blocks, paused);
                    self.bulk_lag_baseline
                        .store(new_baseline, Ordering::Relaxed);
                    limit = pause_at;
                    resume = resume_at;
                }
                pause_due = lag > limit || reader_pressure;
                resume_due = lag <= resume && !reader_pressure;
            }
        } else {
            if !info.pruned {
                self.set_index_paused(false);
                self.release_lag_pause(paused, "Core is no longer pruned: resumed downloads");
                return;
            }
            if info.pruneheight == 0 {
                self.set_index_paused(false);
                self.release_lag_pause(paused, "Core has no pruned blocks: resumed downloads");
                return;
            }
            limit = el_chain::lag_limit(
                info.blocks.min(u32::MAX as u64) as u32,
                info.pruneheight.min(u32::MAX as u64) as u32,
                self.cfg.pruning.max_core_lag_blocks,
            ) as u64;
            resume = (limit / 2).max(1);
            pause_due = lag > limit || reader_pressure;
            resume_due = lag <= resume && !reader_pressure;
        }

        let baseline = self.bulk_lag_baseline.load(Ordering::Relaxed);
        if !paused && pause_due {
            match self.core.set_network_active(false) {
                Ok(_) => {
                    self.lag_paused.store(true, Ordering::Relaxed);
                    self.mark_paused(true);
                    self.set_index_paused(false);
                    if let Some(sh) = &self.shared {
                        sh.stats.core_network_active.store(false, Ordering::Relaxed);
                        sh.set_state(state::PAUSE_DOWNLOAD);
                    }
                    if is_bulk {
                        if bulk_full {
                            eprintln!(
                                "[e-lite] bulk: paused Core downloads until {} blocks remain \
                                 (lag {lag}, backlog {} GiB)",
                                resume,
                                backlog / GIB
                            );
                        } else if high_bytes.is_some() {
                            eprintln!(
                                "[e-lite] lag {lag} blocks, backlog {} GiB (high {} GiB, low \
                                 {} GiB, reader {reader_pressure}): paused Core downloads",
                                backlog / GIB,
                                high_bytes.unwrap_or(0) / GIB,
                                low_bytes.unwrap_or(0) / GIB
                            );
                        } else {
                            eprintln!(
                                "[e-lite] lag {lag} blocks (baseline {baseline}, max growth \
                                 {bulk_blocks}, reader {reader_pressure}): paused Core downloads \
                                 until the index catches up"
                            );
                        }
                    } else {
                        eprintln!(
                            "[e-lite] lag {lag} blocks (limit {limit}, reader {reader_pressure}): \
                             paused Core downloads until the index catches up"
                        );
                    }
                }
                Err(e) => eprintln!("[e-lite] cannot pause Core downloads: {e}"),
            }
        } else if paused && resume_due {
            if self.downloads_paused() {
                self.lag_paused.store(false, Ordering::Relaxed);
                self.set_index_paused(false);
                return;
            }
            self.tunables.shrink_for_core_resume(&self.store);
            match self.core.set_network_active(true) {
                Ok(_) => {
                    self.lag_paused.store(false, Ordering::Relaxed);
                    self.mark_paused(false);
                    let prewarm = is_bulk && self.bulk_full_pause.swap(false, Ordering::SeqCst);
                    if is_bulk && prewarm {
                        self.bulk_lag_baseline.store(lag, Ordering::Relaxed);
                    }
                    self.set_index_paused(is_bulk && self.scheduler_alternate);
                    if let Some(sh) = &self.shared {
                        sh.stats.core_network_active.store(true, Ordering::Relaxed);
                        refresh_ready_state(sh);
                    }
                    if prewarm {
                        eprintln!(
                            "[e-lite] pre-warming Core networking at lag {lag} blocks \
                             (backlog {} GiB)",
                            backlog / GIB
                        );
                    } else {
                        eprintln!(
                            "[e-lite] lag down to {lag} blocks (backlog {} GiB): resumed Core \
                             downloads",
                            backlog / GIB
                        );
                    }
                }
                Err(e) => eprintln!("[e-lite] cannot resume Core downloads: {e}"),
            }
        } else if paused {
            let _ = self.core.set_network_active(false);
            self.mark_paused(true);
            self.set_index_paused(false);
        } else {
            self.set_index_paused(is_bulk && self.scheduler_alternate);
        }
    }

    fn prune_in_flight(&self) -> bool {
        self.prune_in_flight.load(Ordering::SeqCst)
    }

    fn defer_prune(&self, height: u32) {
        if let Ok(mut slot) = self.deferred_prune.lock() {
            *slot = Some(slot.map_or(height, |h| h.max(height)));
        }
    }

    fn take_deferred_prune(&self) -> Option<u32> {
        self.deferred_prune
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
    }

    /// Start a previously deferred prune. This is called at the top of the
    /// apply loop (after a commit) or from the supervisor tick, so the tip that
    /// the WAL barrier made durable is the tip the prune was evaluated
    /// against. The RPC itself runs in `el-prune`.
    fn run_deferred_prune(&self, supervisor: &mut el_chain::Supervisor) {
        let Some(height) = self.take_deferred_prune() else {
            return;
        };
        if self.prune_in_flight() {
            self.defer_prune(height);
            return;
        }
        eprintln!("[e-lite] deferred prune at {height}: commit barrier applied; starting el-prune");
        if !self.start_prune(height) {
            supervisor.on_prune_failed();
            if let Some(sh) = &self.shared {
                sh.stats.prune_failures.fetch_add(1, Ordering::Relaxed);
                refresh_ready_state(sh);
            }
        }
    }

    /// Launch `pruneblockchain` in a background thread. The caller has already
    /// taken the WAL durability barrier; the RPC itself does not need the
    /// apply thread.
    fn start_prune(&self, height: u32) -> bool {
        if self.prune_in_flight.swap(true, Ordering::SeqCst) {
            return false;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        if let Ok(mut slot) = self.prune_rx.lock() {
            *slot = Some(rx);
        } else {
            self.prune_in_flight.store(false, Ordering::SeqCst);
            return false;
        }
        let core = self.core.clone();
        match std::thread::Builder::new()
            .name("el-prune".to_string())
            .spawn(move || {
                let result = core
                    .prune_blockchain(height as u64)
                    .map_err(|e| e.to_string());
                let _ = tx.send(result);
            }) {
            Ok(_) => true,
            Err(e) => {
                eprintln!("[e-lite] cannot start the prune thread: {e}");
                if let Ok(mut slot) = self.prune_rx.lock() {
                    *slot = None;
                }
                self.prune_in_flight.store(false, Ordering::SeqCst);
                false
            }
        }
    }

    /// Collect a finished background prune and feed the supervisor state.
    fn poll_prune(&self, supervisor: &mut el_chain::Supervisor) {
        let msg = {
            let Ok(mut guard) = self.prune_rx.lock() else {
                return;
            };
            let Some(rx) = guard.as_ref() else {
                return;
            };
            match rx.try_recv() {
                Ok(v) => Some(v),
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    *guard = None;
                    Some(Err("prune thread ended without a result".to_string()))
                }
            }
        };
        let Some(result) = msg else {
            return;
        };
        if let Ok(mut guard) = self.prune_rx.lock() {
            *guard = None;
        }
        self.prune_in_flight.store(false, Ordering::SeqCst);
        match result {
            Ok(got) if got > 0 => {
                let got = got.clamp(0, u32::MAX as i64) as u32;
                supervisor.on_pruned(got);
                self.tunables.note_prune();
                if let Some(bulk) = &self.bulk {
                    match bulk.forget_missing_files() {
                        Ok(0) => {}
                        Ok(n) => eprintln!("[e-lite] dropped {n} block locators for pruned files"),
                        Err(e) => eprintln!("[e-lite] cannot clean block locators: {e}"),
                    }
                }
                eprintln!(
                    "[e-lite] pruned Core blocks below {got} (asynchronous request finished)"
                );
                if let Some(sh) = &self.shared {
                    refresh_ready_state(sh);
                }
            }
            Ok(got) => {
                // Core had no whole block file to remove. Keep the retry state
                // so the next safe height is attempted, not marked as pruned.
                supervisor.on_prune_failed();
                eprintln!(
                    "[e-lite] prune not possible yet (Core reports {got}); waiting for more blocks"
                );
                if let Some(sh) = &self.shared {
                    sh.stats.prune_failures.fetch_add(1, Ordering::Relaxed);
                    refresh_ready_state(sh);
                }
            }
            Err(e) => {
                supervisor.on_prune_failed();
                eprintln!("[e-lite] prune request failed: {e}");
                if let Some(sh) = &self.shared {
                    sh.stats.prune_failures.fetch_add(1, Ordering::Relaxed);
                    refresh_ready_state(sh);
                }
            }
        }
    }

    /// Run one supervisor step from inside the indexing loop. Keeping the
    /// pipeline alive across these steps is what avoids discarding prepared
    /// blocks every 10 s; the supervisor cadence is unchanged.
    fn supervisor_step(&self, supervisor: &mut el_chain::Supervisor) {
        self.poll_prune(supervisor);
        self.run_deferred_prune(supervisor);
        match supervisor_tick(
            supervisor,
            &self.core,
            &self.store,
            self.readers.as_deref(),
            &self.data_dir,
            &self.core_datadir,
            &self.blocks_dir,
            self.shared.as_ref(),
        ) {
            Ok(actions) => apply_supervisor_actions(
                supervisor,
                &self.core,
                actions,
                self.shared.as_ref(),
                Some(self),
                self.can_control_core,
            ),
            Err(e) => eprintln!("[e-lite] supervisor: {e}"),
        }
    }

    /// Publish Core's chain state for `/status` and the wallet path.
    fn publish_core_state(&self, info: &BlockchainInfo) {
        if let Some(sh) = &self.shared {
            sh.stats
                .core_pruneheight
                .store(info.pruneheight, Ordering::Relaxed);
            sh.stats.core_pruned.store(info.pruned, Ordering::Relaxed);
            sh.stats
                .core_ibd
                .store(info.initialblockdownload, Ordering::Relaxed);
        }
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
        // The alternate scheduler is the only phase that polls while e-lite is
        // not indexing. Use the 1 Hz observation there to avoid an RPC every
        // 500 ms; in the indexing phase always ask Core so a new block/reorg is
        // seen immediately.
        let info = if self.index_paused.load(Ordering::SeqCst) {
            match self.observed_core(Duration::from_secs(2)) {
                Some(info) => info,
                None => self.core.blockchain_info()?,
            }
        } else {
            self.core.blockchain_info()?
        };
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
        let phase = if self.lag_paused.load(Ordering::Relaxed) {
            1
        } else if self.index_paused.load(Ordering::SeqCst) {
            2
        } else {
            0
        };
        self.update_phase(phase, committed);
        if self.index_paused.load(Ordering::SeqCst) {
            // Alternate scheduler: Core is downloading. Keep serving the
            // current index; the next call re-checks the backlog.
            return Ok(Preflight::Idle(committed));
        }
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
        let guarded = auto_prune
            || (self.cfg.core.prune_mib == 1 && self.cfg.pruning.max_bulk_lag_blocks > 0);
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
            if self.index_paused.load(Ordering::SeqCst) {
                // Alternate scheduler switched to Core downloading. Commit the
                // applied prefix below, then return.
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

impl Indexer {
    fn print_progress(
        &self,
        tip: &IndexedTip,
        core_height: u64,
        pruneheight: u64,
        core_headers: u64,
        verification: f64,
        ibd: bool,
    ) {
        let secs = self.progress_started.elapsed().as_secs_f64().max(0.001);
        let blocks = (tip.height as u64).saturating_sub(self.progress_start_height) as f64;
        let txs = tip.txnum.saturating_sub(self.progress_start_txnum) as f64;
        let lag = core_height.saturating_sub(tip.height as u64);
        let pct = |value: u64, total: u64| {
            if total == 0 {
                "?".to_string()
            } else if value >= total {
                "100.0%".to_string()
            } else {
                // Never round up to 100% while blocks remain: the tail looked
                // synced at "(100.0%)" with lag 2 and the catch-up event never
                // became visible. `FULLY SYNCED` is the real 100%.
                format!("{:.1}%", (value as f64 / total as f64 * 100.0).min(99.9))
            }
        };
        let read_ms = self.tunables.read_ms.load(Ordering::Relaxed);
        let decode_ms = self.tunables.decode_ms();
        let (prefetch_p50, prefetch_p95, prefetch_max) = self.tunables.prefetch_percentiles();
        let (wait_p50, wait_p95) = self.tunables.wait_percentiles();
        let apply_ms = self.tunables.apply_ms.load(Ordering::Relaxed);
        let commit_ms = self.tunables.commit_ewma.load(Ordering::Relaxed);
        // Measured from the blocks actually committed: dividing by the
        // configured batch understated the cost of small groups by 10x.
        let commit_per_block = self.tunables.commit_per_block_ms.load(Ordering::Relaxed);
        let (commit_group, commit_reason) = self
            .tunables
            .last_commit()
            .map(|c| (c.blocks, c.reason.name()))
            .unwrap_or((0, "none"));
        let (phase_blocks, phase_ms) = self
            .phase_state
            .lock()
            .ok()
            .and_then(|s| *s)
            .map(|(started, height)| {
                let tip = tip.height as u64;
                (
                    tip.saturating_sub(height),
                    started.elapsed().as_millis() as u64,
                )
            })
            .unwrap_or((0, 0));
        let phase_rate = if phase_ms > 0 {
            phase_blocks as f64 * 1000.0 / phase_ms as f64
        } else {
            0.0
        };
        let phase_name = match self.phase.load(Ordering::Relaxed) {
            1 => "draining",
            2 => "core",
            _ => "indexing",
        };
        let recent_rate = self.tunables.window_rate();
        // The phase rate is the average since the phase changed (the whole
        // session when it never changes); the window rate is the honest
        // "right now". The ETA uses the window rate.
        let eta_note = if lag > 0 && recent_rate > 0 {
            format!(
                " eta {}",
                format_uptime(Duration::from_secs(lag / recent_rate.max(1)))
            )
        } else {
            String::new()
        };
        eprintln!(
            "[e-lite] height {} ({})  txnum {}  core {}/{} ({})  prune {}  lag {}  \
             {:.1} blocks/s  {:.0} tx/s  elapsed {}  phase {} {:.1} b/s  \
             recent {} b/s{} avgblk {}KiB  \
             read {}ms decode {}ms prefetch p50/p95/max {}/{}/{}ms wait p50/p95 {}/{}ms \
             apply {}ms commit {}ms commit/blk {}ms group {} reason {}{}",
            tip.height,
            pct(tip.height as u64, core_headers),
            tip.txnum,
            core_height,
            core_headers,
            pct(core_height, core_headers),
            pruneheight,
            lag,
            blocks / secs,
            txs / secs,
            format_uptime(self.progress_started.elapsed()),
            phase_name,
            phase_rate,
            recent_rate,
            eta_note,
            self.tunables.avg_block_bytes() / 1024,
            read_ms,
            decode_ms,
            prefetch_p50,
            prefetch_p95,
            prefetch_max,
            wait_p50,
            wait_p95,
            apply_ms,
            commit_ms,
            commit_per_block,
            commit_group,
            commit_reason,
            if ibd {
                format!("  (core in IBD, verify {:.1}%)", verification * 100.0)
            } else {
                String::new()
            }
        );
        if let Some(sh) = &self.shared {
            if let Ok(mut last) = self.last_rocksdb_log.lock() {
                if last.elapsed() >= Duration::from_secs(600) {
                    eprintln!(
                        "[e-lite] rocksdb: pending_compaction {} MiB, running {}, l0 {} files, \
                         memtable {} MiB, block_cache {} MiB, sst {} MiB, table_readers {} MiB, \
                         properties_ok {}",
                        sh.stats
                            .rocksdb_pending_compaction_bytes
                            .load(Ordering::Relaxed)
                            / (1024 * 1024),
                        sh.stats.rocksdb_running_compactions.load(Ordering::Relaxed),
                        sh.stats.rocksdb_level0_files.load(Ordering::Relaxed),
                        sh.stats.rocksdb_memtable_bytes.load(Ordering::Relaxed) / (1024 * 1024),
                        sh.stats.rocksdb_block_cache_bytes.load(Ordering::Relaxed) / (1024 * 1024),
                        sh.stats.rocksdb_sst_bytes.load(Ordering::Relaxed) / (1024 * 1024),
                        sh.stats.rocksdb_table_readers_bytes.load(Ordering::Relaxed)
                            / (1024 * 1024),
                        sh.stats.rocksdb_properties_ok.load(Ordering::Relaxed),
                    );
                    *last = Instant::now();
                    // Extra detail at `debug`: the per-family split shows
                    // which family holds the compaction debt, the level
                    // counts show where it sits, and the live counters show
                    // whether the cache and the write path behave.
                    for (name, pending, memtable, sst, readers, level0, ok) in self.store.cf_stats()
                    {
                        crate::debug_log!(
                            "rocksdb-cf {}: l0 {} pending {} MiB memtable {} MiB \
                             sst {} MiB readers {} MiB ok {}",
                            name,
                            level0,
                            pending / (1024 * 1024),
                            memtable / (1024 * 1024),
                            sst / (1024 * 1024),
                            readers / (1024 * 1024),
                            ok
                        );
                    }
                    let levels = self.store.level_files(3);
                    crate::debug_log!(
                        "rocksdb-levels: l0 {} l1 {} l2 {} l3 {} files (summed)",
                        levels.first().copied().unwrap_or(0),
                        levels.get(1).copied().unwrap_or(0),
                        levels.get(2).copied().unwrap_or(0),
                        levels.get(3).copied().unwrap_or(0)
                    );
                    if let Some(stats_line) = self.store.statistics_line() {
                        crate::debug_log!("rocksdb-stats: {}", stats_line);
                    }
                }
            }
        }
    }
    fn maybe_milestone(&self, tip: &IndexedTip) {
        let Some((from_height, from_txnum, elapsed)) = (|| {
            let mut state = match self.milestone.lock() {
                Ok(g) => g,
                Err(e) => e.into_inner(),
            };
            let due = milestone_due(
                state.last_height,
                tip.height as u64,
                state.last_at.elapsed(),
                self.cfg.index.milestone_blocks,
                self.cfg.index.milestone_seconds,
            );
            if !due {
                return None;
            }
            let out = (state.last_height, state.last_txnum, state.last_at.elapsed());
            state.last_height = tip.height as u64;
            state.last_txnum = tip.txnum;
            state.last_at = Instant::now();
            Some(out)
        })() else {
            return;
        };
        if self.milestone_sizes_running.swap(true, Ordering::SeqCst) {
            return;
        }
        let store = Arc::clone(&self.store);
        let data_dir = self.data_dir.clone();
        let core_datadir = self.core_datadir.clone();
        let running = Arc::clone(&self.milestone_sizes_running);
        let height = tip.height as u64;
        let txnum = tip.txnum;
        std::thread::spawn(move || {
            let lite = store.disk_usage_bytes();
            let core = dir_usage_bytes(&core_datadir);
            let free = el_chain::disk_free_bytes(&data_dir).unwrap_or(0);
            let secs = elapsed.as_secs_f64().max(0.001);
            let blocks = height.saturating_sub(from_height) as f64;
            let txs = txnum.saturating_sub(from_txnum) as f64;
            eprintln!(
                "[e-lite] milestone {}..{}: {:.0} blocks in {:.1}s ({:.1} blocks/s, {:.0} tx/s); \
                 lite {:.1} GiB, core {:.1} GiB, free {:.1} GiB",
                from_height,
                height,
                blocks,
                secs,
                blocks / secs,
                txs / secs,
                lite as f64 / 1024.0 / 1024.0 / 1024.0,
                core as f64 / 1024.0 / 1024.0 / 1024.0,
                free as f64 / 1024.0 / 1024.0 / 1024.0,
            );
            running.store(false, Ordering::SeqCst);
        });
    }
}

pub(crate) fn milestone_due(
    last_height: u64,
    height: u64,
    elapsed: Duration,
    blocks: u64,
    seconds: u64,
) -> bool {
    let by_blocks = blocks > 0 && height >= last_height.saturating_add(blocks);
    let by_time = seconds > 0 && elapsed >= Duration::from_secs(seconds);
    by_blocks || by_time
}

pub(crate) fn commit_sync_policy(mode: &str, ibd: bool, height: u64, core_tip: u64) -> bool {
    match mode {
        "always" => true,
        "txids" => false,
        _ => !(ibd || height.saturating_add(1_000) < core_tip),
    }
}

/// Bulk lag guard window. `u64::MAX` means "no baseline yet". The baseline
/// follows the lowest lag seen **while Core is downloading**. While paused it
/// must stay fixed; otherwise every block indexed would lower the resume
/// threshold and Core would resume immediately.
/// Unindexed backlog in bytes. Heights are not comparable (a block at 818k is
/// several times a block at 100k), so the high/low watermarks are bytes.
pub(crate) fn backlog_bytes(lag_blocks: u64, avg_block_bytes: u64) -> u64 {
    lag_blocks.saturating_mul(avg_block_bytes.max(1))
}

/// Byte watermarks for the bulk scheduler. `high` pauses Core, `low` resumes
/// it. Reader pressure (a reader about to hit Core's prune height) forces the
/// pause: the block may not exist any more.
pub(crate) fn byte_window(
    backlog: u64,
    high: u64,
    low: u64,
    reader_pressure: bool,
) -> (bool, bool) {
    (
        backlog >= high || reader_pressure,
        backlog <= low && !reader_pressure,
    )
}

pub(crate) fn bulk_lag_window(
    lag: u64,
    baseline: u64,
    limit: u64,
    paused: bool,
) -> (u64, u64, u64) {
    let base = if paused {
        if baseline == u64::MAX {
            lag
        } else {
            baseline
        }
    } else if baseline == u64::MAX || lag < baseline {
        lag
    } else {
        baseline
    };
    (
        base,
        base.saturating_add(limit),
        base.saturating_add(limit / 2),
    )
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

pub(crate) fn supervisor_config(
    cfg: &Config,
    can_control_core: bool,
) -> el_chain::SupervisorConfig {
    el_chain::SupervisorConfig {
        disk_enabled: true,
        disk_low_bytes: disk_low_bytes(cfg),
        disk_resume_bytes: disk_resume_bytes(cfg),
        // Only a Core owned by e-lite may be pruned. With manual pruning
        // (`prune_mib = 1`) this is the only pruning authority. With automatic
        // pruning it is an additional, conservative gate.
        prune_enabled: can_control_core,
        undo_margin: cfg.index.reorg_window,
        core_recent_margin: 288,
        min_gain: cfg.pruning.min_gain_blocks,
        min_gain_live: cfg.pruning.min_gain_live_blocks,
        prune_interval: Duration::from_secs(cfg.pruning.prune_interval_seconds),
        prune_defer_free_bytes: {
            // 0 means "do not defer": it must not be raised to min_free_gib.
            if cfg.pruning.prune_defer_free_gib == 0 {
                None
            } else {
                Some(
                    cfg.pruning
                        .prune_defer_free_gib
                        .max(cfg.pruning.min_free_gib)
                        .saturating_mul(GIB_BYTES),
                )
            }
        },
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn supervisor_tick(
    sup: &mut el_chain::Supervisor,
    core: &CoreClient,
    store: &Store,
    readers: Option<&el_chain::ReaderTracker>,
    data_dir: &std::path::Path,
    core_dir: &std::path::Path,
    blocks_dir: &std::path::Path,
    shared: Option<&Arc<Shared>>,
) -> R<Vec<el_chain::Action>> {
    // Watch both filesystems: e-lite's index and Core's datadir may be on
    // different disks, and either one running out must stop both writers.
    // Read this before Core RPC so a slow/unreachable Core cannot hide a full
    // disk.
    let free = [
        el_chain::disk_free_bytes(data_dir),
        el_chain::disk_free_bytes(core_dir),
        el_chain::disk_free_bytes(blocks_dir),
    ]
    .into_iter()
    .flatten()
    .min();
    let info = match core.blockchain_info() {
        Ok(info) => info,
        Err(_) => return Ok(sup.disk_tick(free)),
    };
    if let Some(sh) = shared {
        sh.stats.core_height.store(info.blocks, Ordering::Relaxed);
        sh.stats.core_headers.store(info.headers, Ordering::Relaxed);
        sh.stats.core_verification_bps.store(
            (info.verificationprogress.clamp(0.0, 1.0) * 10_000.0) as u64,
            Ordering::Relaxed,
        );
        sh.stats
            .core_pruneheight
            .store(info.pruneheight, Ordering::Relaxed);
        sh.stats.core_pruned.store(info.pruned, Ordering::Relaxed);
        sh.stats
            .core_ibd
            .store(info.initialblockdownload, Ordering::Relaxed);
        if let Ok(net) = core.network_info() {
            sh.stats
                .core_network_active
                .store(net.networkactive, Ordering::Relaxed);
            sh.stats
                .core_connections
                .store(net.connections, Ordering::Relaxed);
        }
    }
    let committed = store.committed_tip()?.map(|t| t.height as u64).unwrap_or(0);
    let input = el_chain::TickInput {
        committed_height: committed.min(u32::MAX as u64) as u32,
        core_tip: info.blocks.min(u32::MAX as u64) as u32,
        core_pruned: info.pruned,
        core_pruneheight: info.pruneheight.min(u32::MAX as u64) as u32,
        free_bytes: free,
        readers_min: readers.and_then(|r| r.earliest()),
        core_ibd: info.initialblockdownload,
    };
    Ok(sup.tick(&input))
}

/// Recompute READY vs BUILDING from the durable tip and Core stats instead of
/// assuming that a prune/resume left us at the tip.
pub(crate) fn refresh_ready_state(shared: &Shared) {
    if shared.stats.disk_low.load(Ordering::Relaxed) {
        shared.set_state(state::PAUSE_DOWNLOAD);
        return;
    }
    if shared.sync_status().ready {
        shared.set_state(state::READY);
    } else {
        shared.set_state(state::BUILDING);
    }
}

pub(crate) fn apply_supervisor_actions(
    sup: &mut el_chain::Supervisor,
    core: &CoreClient,
    actions: Vec<el_chain::Action>,
    shared: Option<&Arc<Shared>>,
    idx: Option<&Indexer>,
    can_control_core: bool,
) {
    for action in actions {
        match action {
            el_chain::Action::PauseDownloads => {
                if let Some(i) = idx {
                    if can_control_core {
                        // Pause Core only; keep indexing/probing so the prune
                        // gate can free space.
                        i.pause_downloads("disk low");
                    } else if !i.is_paused() {
                        // An external Core cannot be paused. Stop indexing to
                        // protect the disk instead.
                        i.set_paused(true);
                        i.mark_paused(false);
                        if let Some(sh) = shared {
                            sh.stats.disk_low.store(true, Ordering::Relaxed);
                            sh.set_state(state::PAUSE_DOWNLOAD);
                        }
                        eprintln!("[e-lite] disk low: paused indexing (Core is external)");
                    }
                } else {
                    if let Some(sh) = shared {
                        sh.stats.disk_low.store(true, Ordering::Relaxed);
                        sh.set_state(state::PAUSE_DOWNLOAD);
                    }
                    if can_control_core {
                        match core.set_network_active(false) {
                            Ok(_) => eprintln!("[e-lite] disk low: paused Core downloads"),
                            Err(e) => eprintln!("[e-lite] cannot pause Core downloads: {e}"),
                        }
                    } else {
                        eprintln!("[e-lite] disk low: paused indexing (Core is external)");
                    }
                }
            }
            el_chain::Action::ResumeDownloads => {
                if let Some(i) = idx {
                    if can_control_core {
                        i.resume_downloads("disk recovered");
                    } else {
                        i.set_paused(false);
                        if let Some(sh) = shared {
                            sh.stats.disk_low.store(false, Ordering::Relaxed);
                            refresh_ready_state(sh);
                        }
                        eprintln!("[e-lite] disk recovered: resumed indexing");
                    }
                } else {
                    if let Some(sh) = shared {
                        sh.stats.disk_low.store(false, Ordering::Relaxed);
                        refresh_ready_state(sh);
                    }
                    if can_control_core {
                        match core.set_network_active(true) {
                            Ok(_) => eprintln!("[e-lite] disk recovered: resumed Core downloads"),
                            Err(e) => eprintln!("[e-lite] cannot resume Core downloads: {e}"),
                        }
                    } else {
                        eprintln!("[e-lite] disk recovered: resumed indexing");
                    }
                }
            }
            el_chain::Action::Prune(height) => {
                if !can_control_core {
                    continue;
                }
                // A bulk index may have committed the tip with sync=false. Make
                // that tip durable before Core deletes the blocks it needs.
                // `SyncWAL` is the durability barrier: forcing every memtable
                // to SST first added a multi-second stall to every prune and
                // is not required for recovery (a WriteBatch is atomic across
                // CFs and txids.dat is already fsynced).
                if let Some(i) = idx {
                    if let Err(e) = i.store.sync_wal_barrier() {
                        eprintln!("[e-lite] cannot sync the index before pruning: {e}");
                        sup.on_prune_failed();
                        if let Some(sh) = shared {
                            sh.stats.prune_failures.fetch_add(1, Ordering::Relaxed);
                        }
                        continue;
                    }
                }
                if let Some(i) = idx {
                    if i.prune_in_flight() {
                        continue;
                    }
                    sup.on_prune_requested(height);
                    if let Some(sh) = shared {
                        sh.stats.prune_requests.fetch_add(1, Ordering::Relaxed);
                        sh.set_state(state::PRUNING);
                    }
                    // Defer the RPC to the next commit boundary. The WAL
                    // barrier above already covers the committed tip, and the
                    // apply thread keeps working while `el-prune` calls Core.
                    i.defer_prune(height);
                } else {
                    // Defensive fallback: no Indexer to own the thread.
                    sup.on_prune_requested(height);
                    if let Some(sh) = shared {
                        sh.stats.prune_requests.fetch_add(1, Ordering::Relaxed);
                        sh.set_state(state::PRUNING);
                    }
                    match core.prune_blockchain(height as u64) {
                        Ok(got) if got > 0 => {
                            let got = (got as u64).min(u32::MAX as u64) as u32;
                            sup.on_pruned(got);
                            eprintln!(
                                "[e-lite] pruned Core blocks below {got} (requested {height})"
                            );
                        }
                        Ok(got) => {
                            sup.on_prune_failed();
                            eprintln!(
                                "[e-lite] prune at {height} not possible yet (Core reports {got}); waiting for more blocks"
                            );
                            if let Some(sh) = shared {
                                sh.stats.prune_failures.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        Err(e) => {
                            sup.on_prune_failed();
                            eprintln!("[e-lite] prune request failed: {e}");
                            if let Some(sh) = shared {
                                sh.stats.prune_failures.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                    if let Some(sh) = shared {
                        refresh_ready_state(sh);
                    }
                }
            }
        }
    }
}

/// Background controller: runs independently of the indexing pipeline so a
/// single long `catch_up_limited` call cannot delay memory protection. The
/// step itself is rate-limited by `maybe_adapt`.
pub(crate) fn spawn_controller(
    tunables: Arc<Tunables>,
    store: Arc<Store>,
    shared: Option<Arc<Shared>>,
    flag: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("el-tune".to_string())
        .spawn(move || loop {
            for _ in 0..5 {
                if flag.load(Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
            if flag.load(Ordering::SeqCst) {
                return;
            }
            let committed = store
                .committed_tip()
                .ok()
                .flatten()
                .map(|t| t.height as u64)
                .unwrap_or(0);
            let (mode, lag) = tunables.mode_for(committed);
            if tunables
                .maybe_adapt(mode, lag, &store, shared.as_ref().map(|s| &s.stats))
                .is_some()
            {
                tunables.apply_store(&store);
            }
            // The dynamic ZFS ARC cap runs on its own cadence, independent of
            // the adaptive tuning gate.
            tunables.arc_tick(lag);
        })
        .unwrap_or_else(|e| {
            eprintln!("[e-lite] cannot start the tuning thread: {e}");
            std::thread::spawn(|| {})
        })
}

pub(crate) fn cmd_index(cfg: &Config, chain: Chain) -> R<()> {
    let core_running = core_client(cfg, chain)?.tcp_reachable();
    preflight(cfg, chain, true, core_running)?;
    let started_at = SystemTime::now();
    let started = Instant::now();
    let plan = tuning::MachinePlan::detect_with_core_reserve(
        cfg,
        chain,
        core_reserve_mib(cfg, core_running),
    );
    eprintln!("[e-lite] machine plan: {}", plan.describe());
    let rotational = plan.data_disk.kind == tuning::DiskKind::Hdd;
    let tunables = Arc::new(Tunables::new_with_plan(cfg, rotational, Some(&plan)));
    let store = open_store_with(cfg, chain, tunables.cache_mib(), tunables.background_jobs())?;
    tunables.apply_store(&store);
    warn_if_disk_low(cfg, chain);
    let core = core_client(cfg, chain)?;
    let flag = shutdown_flag();
    // A from-zero managed Core begins downloading immediately, so the slot
    // must be taken before it starts, not after the first index call.
    let from_zero = store.committed_tip().ok().flatten().is_none();
    let initial_lock =
        if from_zero && cfg.core.manage != CoreManage::External && ibd_lock::IbdLock::enabled() {
            ibd_lock::IbdLock::acquire_waiting(&flag, chain)
        } else {
            None
        };
    if flag.load(Ordering::SeqCst) {
        println!("shutdown requested; index stopped");
        return Ok(());
    }
    let mut core_sup = CoreSupervisor::new(cfg, chain);
    let Some(identity) =
        ensure_identity_blocking(cfg, chain, &store, &core, &mut core_sup, &flag, true, None)?
    else {
        println!("shutdown requested; index stopped");
        return Ok(());
    };
    let bulk = build_bulk(cfg, chain, identity.genesis, tunables.readers())?;
    println!(
        "e-lite {}: indexing started at {} (network {})",
        VERSION,
        format_local_time(started_at),
        chain.name()
    );
    let can_control = core_sup.can_control_core();
    let store_for_controller = Arc::clone(&store);
    let tunables_for_controller = Arc::clone(&tunables);
    let controller_flag = Arc::clone(&flag);
    let idx = Indexer::new(
        store,
        core,
        bulk,
        None,
        Arc::clone(&tunables),
        cfg.clone(),
        Arc::new(RwLock::new(())),
        None,
        Arc::clone(&flag),
        None,
        can_control,
        chain,
    );
    let controller = spawn_controller(
        Arc::clone(&tunables_for_controller),
        Arc::clone(&store_for_controller),
        None,
        Arc::clone(&controller_flag),
    );
    let result = index_loop(
        cfg,
        chain,
        None,
        idx,
        &mut core_sup,
        flag,
        !cfg.index.follow,
        initial_lock,
    );
    controller_flag.store(true, Ordering::SeqCst);
    let _ = controller.join();
    core_sup.stop();
    if result.is_ok() {
        println!(
            "[e-lite] stopped at {} (uptime {})",
            format_local_time(SystemTime::now()),
            format_uptime(started.elapsed())
        );
    }
    result
}

pub(crate) fn sleep_with_flag(seconds: u64, flag: &Arc<AtomicBool>) -> bool {
    for _ in 0..seconds {
        if flag.load(Ordering::SeqCst) {
            return false;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    !flag.load(Ordering::SeqCst)
}

/// Wait until Core is reachable and the database identity is verified or
/// initialized. With `require_core = false`, an existing database may be used
/// while Core is down (serving cached data).
#[allow(clippy::too_many_arguments)]
pub(crate) fn ensure_identity_blocking(
    cfg: &Config,
    chain: Chain,
    store: &Store,
    core: &CoreClient,
    core_sup: &mut CoreSupervisor,
    shutdown: &Arc<AtomicBool>,
    require_core: bool,
    shared: Option<&Arc<Shared>>,
) -> R<Option<NetworkIdentity>> {
    // Enforce `managed` ownership before trusting a Core that is already up.
    if cfg.core.manage != CoreManage::External {
        if let Err(se) = core_sup.ensure_started(core) {
            if cfg.core.manage == CoreManage::Managed {
                return fail(format!("cannot start managed Core: {se}"));
            }
            eprintln!("[e-lite] Core start: {se}");
        }
    }
    let stored = stored_identity(store, chain)?;
    if let Some(id) = &stored {
        match core_identity(core, chain) {
            Ok(live) => {
                verify_identity(id, &live)?;
                if let Some(sh) = shared {
                    sh.set_identity(live.clone());
                    sh.set_state(state::BUILDING);
                }
                return Ok(Some(live));
            }
            Err(_) if !require_core => {
                // Offline serve: keep the pinned identity and let the caller
                // print a single "serving cached index" line.
                if let Some(sh) = shared {
                    sh.set_state(state::CORE_UNAVAILABLE);
                }
                return Ok(Some(id.clone()));
            }
            Err(_) => {}
        }
    }

    let mut backoff = 1u64;
    let mut loading_logged = false;
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return Ok(None);
        }
        match core_identity(core, chain) {
            Ok(live) => {
                if let Some(id) = &stored {
                    verify_identity(id, &live)?;
                } else {
                    store.init_identity(&live)?;
                    println!(
                        "initialized {} at {}",
                        chain.name(),
                        cfg.data_dir(chain).display()
                    );
                    println!("genesis: {}", live.genesis);
                }
                if let Some(sh) = shared {
                    sh.set_identity(live.clone());
                    sh.set_state(state::BUILDING);
                }
                return Ok(Some(live));
            }
            Err(e) => {
                let msg = e.to_string();
                let loading = msg.contains("Loading block index")
                    || msg.contains("Verifying blocks")
                    || msg.contains("(code -28)");
                if let Some(sh) = shared {
                    sh.set_state(if stored.is_some() {
                        state::CORE_UNAVAILABLE
                    } else {
                        state::WAITING_FOR_CORE
                    });
                }
                match core_sup.ensure_started(core) {
                    Ok(CoreStartOutcome::Started) => {
                        eprintln!("[e-lite] started Core; waiting for its RPC")
                    }
                    Ok(_) => {}
                    Err(se) if cfg.core.manage == CoreManage::Managed => {
                        return fail(format!("cannot start managed Core: {se}"));
                    }
                    Err(se) => eprintln!("[e-lite] Core start: {se}"),
                }
                if loading {
                    if !loading_logged {
                        eprintln!(
                            "[e-lite] Core is loading its block index; waiting for RPC readiness"
                        );
                        loading_logged = true;
                    }
                } else {
                    eprintln!("[e-lite] waiting for Bitcoin Core ({msg}); retrying in {backoff}s");
                    loading_logged = false;
                }
                if !sleep_with_flag(backoff, shutdown) {
                    return Ok(None);
                }
                backoff = (backoff * 2).min(30);
            }
        }
    }
}

/// Reload the hot subset of the config. Cold fields are reported but require a
/// restart; the last valid config stays in effect on parse/validation errors.
pub(crate) fn reload_hot(
    old: &Config,
    idx: &Indexer,
    supervisor: &mut el_chain::Supervisor,
    shared: Option<&Arc<Shared>>,
    can_control_core: bool,
    chain: Chain,
) {
    let Some(path) = config_path() else {
        eprintln!("[e-lite] reload requested but no config file was resolved");
        return;
    };
    let path_str = path.to_string_lossy().to_string();
    match Config::load(Some(&path_str)) {
        Ok(mut new) => {
            new.apply_network_defaults(chain);
            if let Err(e) = new.validate() {
                eprintln!("[e-lite] reload rejected ({e}); keeping the previous config");
                return;
            }
            let mut changed = idx.tunables.reload(&new);
            idx.tunables.apply_store(&idx.store);
            if let (Some(a), Some(b)) = (
                logging::parse(&new.logging.level),
                logging::parse(&old.logging.level),
            ) {
                if a != b {
                    logging::set_level(a);
                    el_serve::set_debug_logs(a >= logging::DEBUG);
                    el_rawtx::set_debug_logs(a >= logging::DEBUG);
                    changed.push("logging.level");
                }
            }
            if new.server.advertise_electrumx_compat != old.server.advertise_electrumx_compat {
                // New connections get the new banner; open ones keep theirs.
                el_serve::set_electrumx_compat(new.server.advertise_electrumx_compat);
                changed.push("server.advertise_electrumx_compat");
            }
            if changed.contains(&"profile") {
                if let Err(e) = idx
                    .store
                    .set_background_jobs(idx.tunables.background_jobs())
                {
                    eprintln!("[e-lite] reload: cannot set RocksDB background jobs: {e}");
                }
            }
            supervisor.update_config(supervisor_config(&new, can_control_core));
            if let Some(sh) = shared {
                idx.tunables.publish(&idx.store, &sh.stats);
            }
            if new.core.prune_mib != old.core.prune_mib
                || new.index.block_source != old.index.block_source
                || new.index.reorg_window != old.index.reorg_window
                || new.network.data_dir != old.network.data_dir
                || new.server.listen != old.server.listen
                || new.server.tls_listen != old.server.tls_listen
                || new.server.tls_cert != old.server.tls_cert
                || new.server.tls_key != old.server.tls_key
                || new.index.write_buffer_mib != old.index.write_buffer_mib
                || new.index.milestone_blocks != old.index.milestone_blocks
                || new.index.milestone_seconds != old.index.milestone_seconds
                || new.pruning.max_core_lag_blocks != old.pruning.max_core_lag_blocks
                || new.pruning.max_bulk_lag_blocks != old.pruning.max_bulk_lag_blocks
                || new.pruning.max_bulk_lag_gib != old.pruning.max_bulk_lag_gib
                || new.pruning.core_resume_ahead_blocks != old.pruning.core_resume_ahead_blocks
                || new.pruning.core_resume_ahead_gib != old.pruning.core_resume_ahead_gib
                || new.pruning.pause_core_during_bulk != old.pruning.pause_core_during_bulk
                || new.pruning.scheduler != old.pruning.scheduler
            {
                eprintln!(
                    "[e-lite] reload: cold fields changed (prune_mib, block_source, \
                     reorg_window, data_dir, server listeners/TLS, write buffers, \
                     milestones, lag limits/scheduler); restart required"
                );
            }
            eprintln!(
                "[e-lite] reloaded {} hot field(s): {}",
                changed.len(),
                if changed.is_empty() {
                    "no changes".to_string()
                } else {
                    changed.join(", ")
                }
            );
        }
        Err(e) => eprintln!("[e-lite] reload failed ({e}); keeping the previous config"),
    }
}

/// Poll Core once per second into a shared snapshot. The block-application
/// loop then reads that snapshot instead of opening an RPC connection every
/// 100 ms; auto-prune keeps a synchronous fallback for safety.
pub(crate) fn spawn_core_observer(idx: &Indexer, flag: Arc<AtomicBool>) {
    let core = idx.core.clone();
    let obs = idx.core_observation();
    let shared = idx.shared.clone();
    let tunables = Arc::clone(&idx.tunables);
    let _ = std::thread::Builder::new()
        .name("el-core-obs".to_string())
        .spawn(move || {
            while !flag.load(Ordering::SeqCst) {
                if let Ok(info) = core.blockchain_info() {
                    if let Ok(mut guard) = obs.lock() {
                        *guard = Some((info.clone(), Instant::now()));
                    }
                    tunables.set_core_state(info.blocks, info.initialblockdownload);
                    if let Some(sh) = &shared {
                        sh.stats.core_height.store(info.blocks, Ordering::Relaxed);
                        sh.stats.core_headers.store(info.headers, Ordering::Relaxed);
                        sh.stats
                            .core_pruneheight
                            .store(info.pruneheight, Ordering::Relaxed);
                        sh.stats.core_pruned.store(info.pruned, Ordering::Relaxed);
                        sh.stats
                            .core_ibd
                            .store(info.initialblockdownload, Ordering::Relaxed);
                        sh.stats.core_verification_bps.store(
                            (info.verificationprogress.clamp(0.0, 1.0) * 10_000.0) as u64,
                            Ordering::Relaxed,
                        );
                    }
                }
                for _ in 0..10 {
                    if flag.load(Ordering::SeqCst) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        });
}

/// Shared index loop for `index` and `run`. `stop_at_tip` implements
/// `--no-follow`; `run` always follows.
#[allow(clippy::too_many_arguments)]
pub(crate) fn index_loop(
    cfg: &Config,
    chain: Chain,
    shared: Option<Arc<Shared>>,
    idx: Indexer,
    core_sup: &mut CoreSupervisor,
    flag: Arc<AtomicBool>,
    stop_at_tip: bool,
    mut ibd_lock: Option<ibd_lock::IbdLock>,
) -> R<()> {
    let can_control = idx.can_control_core;
    let mut supervisor = el_chain::Supervisor::new(supervisor_config(cfg, can_control));
    let poll = Duration::from_secs(cfg.pruning.poll_seconds.max(1));
    let mut last_tick = Instant::now()
        .checked_sub(poll)
        .unwrap_or_else(Instant::now);
    let mut last_height = u64::MAX;
    let mut was_ready = false;
    let mut was_fully_synced = false;
    // The READY/FULLY SYNCED INFO lines print once per session; later live
    // re-syncs (Core one block ahead, then caught up again) log at debug.
    let mut printed_ready_once = false;
    let mut printed_fully_once = false;
    let mut backoff = 1u64;
    let mut loading_logged = false;
    eprintln!(
        "[e-lite] tuning profile={} readers={} commit={}ms commit_sync={} batch={} cache={}MiB jobs={} prefetch={} adaptive={}",
        idx.tunables.profile().name(),
        idx.tunables.readers(),
        idx.tunables.commit_ms(),
        cfg.index.commit_sync,
        idx.tunables.batch_blocks(),
        idx.tunables.cache_mib(),
        idx.tunables.background_jobs(),
        idx.tunables.prefetch_blocks(),
        idx.tunables.adaptive()
    );
    let (
        mem_total,
        _mem_available,
        mem_effective,
        mem_core_reserve,
        mem_reserve,
        mem_ceiling,
        mem_memtables,
        mem_runtime,
    ) = idx.tunables.memory_numbers();
    let mem_now = tuning::memory_info();
    eprintln!(
        "[e-lite] memory: total {} MiB, available {} MiB, effective {} MiB \
         (core reserve {}), free {} MiB, buff/cache {} MiB (sreclaim {}, shmem {}), \
         arc {}, rss {}, reserve {} MiB, ceiling {} MiB -> cache {} MiB, \
         memtables {} MiB, runtime {} MiB",
        mem_total,
        mem_now.available_mib,
        mem_effective,
        mem_core_reserve,
        mem_now.free_mib,
        mem_now.cached_mib,
        mem_now.sreclaimable_mib,
        mem_now.shmem_mib,
        mem_now
            .arc_mib
            .map(|v| v.to_string())
            .unwrap_or_else(|| "none".to_string()),
        tuning::process_rss_mib(),
        mem_reserve,
        mem_ceiling,
        idx.tunables.cache_mib(),
        mem_memtables,
        mem_runtime
    );
    spawn_core_observer(&idx, Arc::clone(&flag));
    if let Some(sh) = &shared {
        idx.tunables.publish(&idx.store, &sh.stats);
    }

    // Acquire the intensive-sync slot *before* the first 512-block call. The
    // old loop checked only after a call returned; at 1-2 blocks/s that left a
    // ~5-9 minute window where a second instance could index concurrently.
    if ibd_lock.is_none() && ibd_lock::IbdLock::enabled() && idx.core.tcp_reachable() {
        if let Ok(info) = idx.core.blockchain_info() {
            let committed = idx
                .store
                .committed_tip()
                .ok()
                .flatten()
                .map(|t| t.height as u64)
                .unwrap_or(0);
            let lag = info.blocks.saturating_sub(committed);
            if info.initialblockdownload || lag > 5_000 {
                ibd_lock = ibd_lock::IbdLock::acquire_waiting(&flag, chain);
                if flag.load(Ordering::SeqCst) {
                    return Ok(());
                }
            }
        }
    }

    loop {
        if reload_flag().swap(false, Ordering::SeqCst) {
            reload_hot(
                cfg,
                &idx,
                &mut supervisor,
                shared.as_ref(),
                can_control,
                chain,
            );
        }
        if flag.load(Ordering::SeqCst) {
            if shared.is_none() {
                println!("shutdown requested; index stopped");
            }
            return Ok(());
        }
        match idx.catch_up_limited(CATCH_UP_BLOCKS, &mut supervisor, &mut last_tick) {
            Ok(height) => {
                if flag.load(Ordering::SeqCst) {
                    return Ok(());
                }
                backoff = 1;
                loading_logged = false;
                let core_height = shared
                    .as_ref()
                    .map(|s| s.stats.core_height.load(Ordering::Relaxed))
                    .unwrap_or_else(|| idx.tunables.core_height());
                let core_ibd = shared
                    .as_ref()
                    .map(|s| s.stats.core_ibd.load(Ordering::Relaxed))
                    .unwrap_or_else(|| idx.tunables.core_ibd());
                let lag = core_height.saturating_sub(height);
                // Intensive sync = Core IBD or a large backlog. Only one runs
                // per user; the others wait here (still serving) and start on
                // their own once the slot is released.
                let intensive = core_ibd || lag > 5_000;
                if intensive && ibd_lock.is_none() && ibd_lock::IbdLock::enabled() {
                    ibd_lock = ibd_lock::IbdLock::acquire_waiting(&flag, chain);
                    if flag.load(Ordering::SeqCst) {
                        return Ok(());
                    }
                } else if !intensive && lag <= 1_000 {
                    if let Some(lock) = ibd_lock.take() {
                        eprintln!(
                            "[e-lite] caught up; released the intensive-sync slot ({})",
                            lock.path.display()
                        );
                    }
                }
                // The `el-tune` thread is the single owner of adaptation. A
                // second call site here could pass the 30 s gate concurrently
                // and change cache/readers twice.
                if let Some(sh) = &shared {
                    let caught_up = !core_ibd && height >= core_height;
                    let next = if idx.is_paused() || idx.downloads_paused() || supervisor.paused() {
                        state::PAUSE_DOWNLOAD
                    } else if caught_up {
                        state::READY
                    } else {
                        state::BUILDING
                    };
                    sh.set_state(next);
                    let sync = sh.sync_status();
                    let tls = sh
                        .tls_listen
                        .as_ref()
                        .map(|addr| format!(" · SSL {addr}"))
                        .unwrap_or_default();
                    let print_fully = sync.fully_synced && !was_fully_synced;
                    let print_ready = sync.ready && !was_ready && !print_fully;
                    let session = format_uptime(idx.progress_started.elapsed());
                    if print_fully {
                        if !printed_fully_once {
                            eprintln!("{}", readiness_line(sh, &sync, &tls, true, &session));
                            printed_fully_once = true;
                        } else {
                            // In live mode Core gets ahead by a block and the
                            // index re-syncs every few minutes; the first
                            // FULLY SYNCED of the session is the INFO line.
                            crate::debug_log!(
                                "scheduler: fully synced again at height {} (core {})",
                                sync.height,
                                sync.core_height
                            );
                        }
                    } else if print_ready {
                        if !printed_ready_once {
                            eprintln!("{}", readiness_line(sh, &sync, &tls, false, &session));
                            printed_ready_once = true;
                        } else {
                            crate::debug_log!(
                                "scheduler: ready again at height {} (core {})",
                                sync.height,
                                sync.core_height
                            );
                        }
                    }
                    if sync.fully_synced {
                        was_fully_synced = true;
                        was_ready = true;
                    } else if sync.ready {
                        was_ready = true;
                        was_fully_synced = false;
                    } else {
                        was_ready = false;
                        was_fully_synced = false;
                    }
                }
                if stop_at_tip {
                    let core_tip = idx.core.blockchain_info()?.blocks;
                    if height >= core_tip {
                        println!("caught up at height {height}");
                        return Ok(());
                    }
                }
                if height == last_height {
                    std::thread::sleep(Duration::from_millis(500));
                }
                last_height = height;
            }
            Err(e) => {
                let msg = e.to_string();
                match classify_error(e.as_ref()) {
                    ErrorClass::Permanent => {
                        if let Some(sh) = &shared {
                            sh.set_state(state::FATAL);
                        }
                        return Err(e);
                    }
                    ErrorClass::Transient => {
                        if let Some(sh) = &shared {
                            sh.set_state(state::CORE_UNAVAILABLE);
                        }
                        let _ = core_sup.ensure_started(&idx.core);
                        let loading = el_rpc::is_core_loading(&msg);
                        if loading {
                            if !loading_logged {
                                eprintln!(
                                    "[e-lite] Core is loading its block index; waiting for RPC readiness"
                                );
                                loading_logged = true;
                            }
                        } else {
                            eprintln!(
                                "[e-lite] transient error: {msg}; retrying in {backoff}s \
                                 (serving cached index if available)"
                            );
                            loading_logged = false;
                        }
                        if !sleep_with_flag(backoff, &flag) {
                            return Ok(());
                        }
                        backoff = (backoff * 2).min(30);
                        continue;
                    }
                }
            }
        }
        if idx.is_paused() {
            // Indexing is stopped by the critical disk floor. Still run the
            // supervisor so the prune gate can delete old Core block files and
            // free space; without this the system cannot recover on its own.
            // Check every second, not every poll, because the disk is critical.
            if last_tick.elapsed() >= Duration::from_secs(1) {
                idx.supervisor_step(&mut supervisor);
                last_tick = Instant::now();
            } else {
                std::thread::sleep(Duration::from_millis(200));
            }
        } else if last_tick.elapsed() >= poll
            || (idx.downloads_paused() && last_tick.elapsed() >= Duration::from_secs(1))
        {
            idx.supervisor_step(&mut supervisor);
            last_tick = Instant::now();
        }
    }
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
