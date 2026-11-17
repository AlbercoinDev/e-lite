//! Progress lines and milestones in the log.

use super::*;

impl Indexer {
    pub(super) fn print_progress(
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

    pub(super) fn maybe_milestone(&self, tip: &IndexedTip) {
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

fn milestone_due(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn milestone_triggers_by_blocks_or_time() {
        let hour = Duration::from_secs(3600);
        assert!(milestone_due(10_000, 20_000, Duration::ZERO, 10_000, 0));
        assert!(!milestone_due(10_000, 19_999, Duration::ZERO, 10_000, 0));
        assert!(milestone_due(10_000, 10_001, hour, 0, 3600));
        assert!(!milestone_due(10_000, 10_001, hour, 0, 0));
        // Both disabled means never.
        assert!(!milestone_due(0, 1_000_000, hour, 0, 0));
    }
}
