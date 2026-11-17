//! Pacing Core's downloads against the index: the disk guard, pause/resume
//! of Core's network, the byte lag window (`lag_guard`) and the phase it
//! publishes. Core is paused only through these functions.

use super::*;

impl Indexer {
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
    pub(super) fn disk_guard(&self) -> bool {
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

    pub(super) fn downloads_paused(&self) -> bool {
        self.downloads_paused.load(Ordering::SeqCst)
    }

    /// Pause Core's P2P downloads without stopping indexing. The marker lets a
    /// later e-lite run restore `networkactive` after a crash.
    pub(super) fn pause_downloads(&self, reason: &str) {
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

    pub(super) fn resume_downloads(&self, reason: &str) {
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

    pub(super) fn mark_paused(&self, paused: bool) {
        if paused {
            let _ = std::fs::write(
                &self.pause_marker,
                format!("pid={} at={:?}\n", std::process::id(), Instant::now()),
            );
        } else {
            let _ = std::fs::remove_file(&self.pause_marker);
        }
    }

    pub(super) fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::SeqCst);
    }

    pub(super) fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
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

    /// Track the current scheduler phase and its rate. `phase`: 0 overlap
    /// (Core and e-lite both work), 1 Core paused while e-lite drains.
    pub(super) fn update_phase(&self, phase: u64, committed: u64) {
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
    /// unindexed backlog.
    pub(super) fn lag_guard(&self, info: &BlockchainInfo, committed: u64) {
        if !self.can_control_core {
            return;
        }
        let paused = self.lag_paused.load(Ordering::Relaxed);
        let lag = info.blocks.saturating_sub(committed);
        let avg = self.tunables.avg_block_bytes();
        let backlog = backlog_bytes(lag, avg);
        let bulk = self.cfg.core.prune_mib == 1 && self.cfg.pruning.max_bulk_lag_gib > 0;
        let auto_enabled = self.cfg.core.prune_mib != 1 && self.cfg.pruning.max_core_lag_blocks > 0;
        let high_bytes = bulk.then(|| self.cfg.pruning.max_bulk_lag_gib.saturating_mul(GIB));
        let low_bytes = bulk.then(|| self.cfg.pruning.core_resume_ahead_gib.saturating_mul(GIB));
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
                self.bulk_started.store(false, Ordering::Relaxed);
                self.bulk_full_pause.store(false, Ordering::Relaxed);
                self.release_lag_pause(paused, "resumed Core downloads after IBD");
                return;
            }
            let high = high_bytes.unwrap_or(u64::MAX);
            let low = low_bytes.unwrap_or(0);
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
            if !info.pruned {
                self.release_lag_pause(paused, "Core is no longer pruned: resumed downloads");
                return;
            }
            if info.pruneheight == 0 {
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

        if !paused && pause_due {
            match self.core.set_network_active(false) {
                Ok(_) => {
                    self.lag_paused.store(true, Ordering::Relaxed);
                    self.mark_paused(true);
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
                        } else {
                            eprintln!(
                                "[e-lite] lag {lag} blocks, backlog {} GiB (high {} GiB, low \
                                 {} GiB, reader {reader_pressure}): paused Core downloads",
                                backlog / GIB,
                                high_bytes.unwrap_or(0) / GIB,
                                low_bytes.unwrap_or(0) / GIB
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
                return;
            }
            self.tunables.shrink_for_core_resume(&self.store);
            match self.core.set_network_active(true) {
                Ok(_) => {
                    self.lag_paused.store(false, Ordering::Relaxed);
                    self.mark_paused(false);
                    let prewarm = is_bulk && self.bulk_full_pause.swap(false, Ordering::SeqCst);
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
        }
    }
}

/// Unindexed backlog in bytes. Heights are not comparable (a block at 818k is
/// several times a block at 100k), so the high/low watermarks are bytes.
fn backlog_bytes(lag_blocks: u64, avg_block_bytes: u64) -> u64 {
    lag_blocks.saturating_mul(avg_block_bytes.max(1))
}

/// Byte watermarks for the bulk scheduler. `high` pauses Core, `low` resumes
/// it. Reader pressure (a reader about to hit Core's prune height) forces the
/// pause: the block may not exist any more.
fn byte_window(backlog: u64, high: u64, low: u64, reader_pressure: bool) -> (bool, bool) {
    (
        backlog >= high || reader_pressure,
        backlog <= low && !reader_pressure,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_backlog_is_height_independent() {
        // 20 GiB is ~41k early blocks (0.5 MB) but only ~13.6k at 818k
        // (1.5 MB). The byte watermark is the one that bounds the disk.
        let gib = 1024 * 1024 * 1024u64;
        let high = 20 * gib;
        // Same 20 GiB backlog is ~41k blocks at 512 KiB but only ~14.3k at
        // 1.5 MB: a block-count watermark cannot bound the disk.
        assert!(backlog_bytes(41_000, 512 * 1024) >= high);
        assert!(backlog_bytes(40_000, 512 * 1024) < high);
        assert!(backlog_bytes(14_317, 1_500_000) >= high);
        assert!(backlog_bytes(14_316, 1_500_000) < high);
    }

    #[test]
    fn byte_window_pauses_at_high_and_resumes_at_low() {
        let gib = 1024 * 1024 * 1024u64;
        let high = 30 * gib;
        let low = 7 * gib;
        assert_eq!(byte_window(29 * gib, high, low, false), (false, false));
        assert_eq!(byte_window(30 * gib, high, low, false), (true, false));
        assert_eq!(byte_window(7 * gib, high, low, false), (false, true));
        assert_eq!(byte_window(6 * gib, high, low, false), (false, true));
        // A reader at the prune height forces a pause even below the low mark.
        assert_eq!(byte_window(6 * gib, high, low, true), (true, false));
    }
}
