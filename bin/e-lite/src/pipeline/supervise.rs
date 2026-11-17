//! The prune gate and the `el_chain::Supervisor` glue: deferred and
//! background prune RPCs, the supervisor tick and the actions it returns
//! (prune, pause, resume). It runs at commit boundaries (`supervisor_step`).

use super::*;

impl Indexer {
    pub(super) fn prune_in_flight(&self) -> bool {
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
    pub(super) fn run_deferred_prune(&self, supervisor: &mut el_chain::Supervisor) {
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
    pub(super) fn supervisor_step(&self, supervisor: &mut el_chain::Supervisor) {
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
    pub(super) fn publish_core_state(&self, info: &BlockchainInfo) {
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
                        .saturating_mul(GIB),
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
