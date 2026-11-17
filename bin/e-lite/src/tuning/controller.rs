//! The adaptive controller: readers, batch and cache; memtables only shrink,
//! as a safety valve, when memory is critical.

use super::*;

impl Tunables {
    pub(super) fn reader_streak_ok(&self, direction: i8) -> bool {
        let Ok(mut streak) = self.reader_streak.lock() else {
            return false;
        };
        if streak.0 == direction {
            streak.1 = streak.1.saturating_add(1);
        } else {
            *streak = (direction, 1);
        }
        if streak.1 < 2 {
            return false;
        }
        *streak = (0, 0);
        true
    }

    /// One conservative control step. Returns a human-readable reason when a
    /// knob changed.
    pub fn maybe_adapt(
        &self,
        mode: Mode,
        _lag: u64,
        store: &Store,
        stats: Option<&Stats>,
    ) -> Option<String> {
        if !self.adaptive() {
            return None;
        }
        // One owner: a second caller cannot pass the 30 s gate concurrently.
        let Ok(_gate) = self.adapt_gate.lock() else {
            return None;
        };
        {
            let Ok(last) = self.last_change.lock() else {
                return None;
            };
            if last.elapsed() < Duration::from_secs(30) {
                return None;
            }
        }

        let read_ms = self.read_ms.load(Ordering::Relaxed);
        let apply_ms = self.apply_ms.load(Ordering::Relaxed);
        let commit_ms = self.commit_ewma.load(Ordering::Relaxed);
        if let Some(reason) = self.memory_step(store, stats) {
            if let Ok(mut last) = self.last_change.lock() {
                *last = Instant::now();
            }
            return Some(reason);
        }
        let mut action = None;

        // Commit decisions use the last *full* group (same cycle, same cost),
        // never the EWMA against a possibly-partial cycle. A tx-limited group
        // means the batch is not the binding constraint: leave it alone.
        let prune_recent = self.prune_recent();
        let batch_locked = self.batch_locked.load(Ordering::Relaxed);
        let commit_locked = self.commit_locked.load(Ordering::Relaxed);
        if let Some(last) = self.last_commit() {
            let share = if last.cycle_ms > 0 {
                last.ms as f64 / last.cycle_ms as f64
            } else {
                0.0
            };
            let io_bound = last.db_us > last.txids_us.saturating_mul(2);
            let tx_bound = last.reason == CommitReason::Txs;
            let fresh = last.seq > self.last_commit_eval.load(Ordering::Relaxed);
            let mut evaluated = false;
            if last.cycle_ms > 0 && fresh && !prune_recent && !commit_locked {
                if tx_bound {
                    // The transaction limit committed the group. Changing the
                    // batch would only move the binding limit; leave it.
                } else if io_bound {
                    // RocksDB write/compaction dominates. A smaller batch
                    // increases the number of commits without reducing the
                    // per-block cost; keep it and let the operator inspect
                    // /status.rocksdb.cf. Do not consume the single action.
                } else if share > 0.75
                    && last.reason == CommitReason::Blocks
                    && !batch_locked
                    && self.batch_blocks() > self.min_batch.saturating_mul(4)
                {
                    let b = (self.batch_blocks() * 3 / 4).max(self.min_batch);
                    self.batch_blocks.store(b, Ordering::Relaxed);
                    action = Some(format!(
                        "commit {}ms ({:.0}% of cycle, blocks limit): batch -> {b}",
                        last.ms,
                        share * 100.0
                    ));
                } else if share < 0.5 && last.ms > 500 && self.commit_ms() < self.max_commit_ms {
                    // Only the block limit benefits from a larger batch. A
                    // Time-limited group can use a longer interval, but a
                    // Txs/Bytes-limited one gains nothing: do not grow a knob
                    // that is not the binding constraint.
                    let grow_interval =
                        matches!(last.reason, CommitReason::Blocks | CommitReason::Time);
                    let grow_batch = last.reason == CommitReason::Blocks && !batch_locked;
                    if grow_interval || grow_batch {
                        let old_interval = self.commit_ms();
                        let v = if grow_interval {
                            let v = (old_interval * 3 / 2).min(self.max_commit_ms);
                            self.commit_ms.store(v, Ordering::Relaxed);
                            v
                        } else {
                            old_interval
                        };
                        let b = if grow_batch {
                            let b = (self.batch_blocks() * 3 / 2).min(self.max_batch);
                            self.batch_blocks.store(b, Ordering::Relaxed);
                            b
                        } else {
                            self.batch_blocks()
                        };
                        action = Some(format!(
                            "commit {}ms ({:.0}% of cycle, reason {:?}): interval -> {v}ms, \
                             batch -> {b}",
                            last.ms,
                            share * 100.0,
                            last.reason
                        ));
                    }
                }
                evaluated = true;
            }
            if evaluated {
                self.last_commit_eval.store(last.seq, Ordering::Relaxed);
            }
        }
        if action.is_none()
            && !prune_recent
            && !commit_locked
            && mode == Mode::Live
            && commit_ms < 100
            && self.commit_ms() > 1_500
        {
            let v = (self.commit_ms() / 2).max(1_500);
            self.commit_ms.store(v, Ordering::Relaxed);
            action = Some(format!("live mode: commit interval -> {v}ms"));
        }

        // Read/apply balance. On a rotational disk more readers usually hurt.
        // Only add readers when reads are actually slow; low read latency means
        // the bottleneck is elsewhere (apply/commit/compaction).
        let max_readers = self.auto_max_readers().min(self.memory_reader_cap());
        let wait_ms = self.wait_ms();
        let prefetch_ms = self.prefetch_ms();
        if action.is_none() && !self.readers_locked.load(Ordering::Relaxed) {
            // Pipeline starvation (wait_ms) means more concurrency is needed;
            // a high block-read latency with no wait is the block source's
            // limit, and reducing readers there does not help. Require two
            // consecutive samples before changing the pool.
            let direction = if wait_ms > 50 && prefetch_ms > 100 && self.readers() < max_readers {
                1i8
            } else if read_ms > 200
                && wait_ms < 50
                && apply_ms < read_ms
                && self.readers() > self.min_readers
            {
                -1i8
            } else {
                0i8
            };
            if direction != 0 && self.reader_streak_ok(direction) {
                let v = if direction > 0 {
                    (self.readers() + 1).min(max_readers)
                } else {
                    (self.readers() * 3 / 4).max(self.min_readers)
                };
                if v != self.readers() {
                    self.readers.store(v, Ordering::Relaxed);
                    action = Some(format!(
                        "readers {direction:+}: read {read_ms}ms wait {wait_ms}ms prefetch \
                         {prefetch_ms}ms -> {v}"
                    ));
                }
            }
        }

        if let Some(reason) = &action {
            if let Ok(mut last) = self.last_change.lock() {
                *last = Instant::now();
            }
            self.adapt_steps.fetch_add(1, Ordering::Relaxed);
            eprintln!("[e-lite] adaptive tuning: {reason}");
            if let Some(stats) = stats {
                self.publish(store, stats);
            }
        } else if let Some(stats) = stats {
            self.publish(store, stats);
        }
        action
    }

    /// Hot memory policy: grow the block cache in small steps while page cache
    /// and available RAM stay healthy, and shrink under pressure. Manual
    /// `cache_mib` is treated as a maximum, not as an immutable value.
    /// Hot cache ceiling. `available` already excludes e-lite's resident
    /// memory (memtables, runtime and RocksDB table readers), so subtracting
    /// the startup memtable/runtime budgets here double-counted them and
    /// starved the cache. The only subtraction left is a dynamic OS reserve.
    pub(super) fn dynamic_reserve_mib(&self, mem: &MemoryInfo) -> u64 {
        dynamic_reserve_for(self.reserve_mib, mem.available_mib)
    }

    fn dynamic_cache_max(&self, mem: &MemoryInfo) -> (u64, u64, u64, u64) {
        let reserve = self.dynamic_reserve_mib(mem);
        let effective = self
            .total_mib
            .min(
                mem.available_mib.saturating_add(
                    self.cache_mib()
                        .saturating_add(self.memtables_mib.load(Ordering::Relaxed))
                        .saturating_add(self.runtime_mib),
                ),
            )
            .max(128);
        let max = mem
            .available_mib
            .saturating_sub(reserve)
            .min(self.max_cache_mib)
            .max(self.min_cache_mib.min(self.max_cache_mib));
        let margin = (mem.available_mib / 8).clamp(256, 2048);
        (max, margin, effective, reserve)
    }

    fn memory_step(&self, store: &Store, stats: Option<&Stats>) -> Option<String> {
        let mem = memory_info();
        // Growing the cache only because RAM is free is not evidence that it
        // helps. Require measured UTXO-prefetch pressure; the prefetch EWMA is
        // the closest signal we have to "the block cache would avoid I/O".
        let (max_cache, grow_margin, effective, dynamic_reserve) = self.dynamic_cache_max(&mem);
        let (_, prefetch_p95, _) = self.prefetch_percentiles();
        let prefetch_signal = if prefetch_p95 > 0 {
            prefetch_p95
        } else {
            self.prefetch_ms()
        };
        let action = cache_action(
            mem.available_mib,
            dynamic_reserve,
            self.cache_mib(),
            max_cache,
            grow_margin,
            prefetch_signal,
        );
        let dir = match action {
            CacheAction::Grow => 1i8,
            CacheAction::Shrink => -1,
            CacheAction::Hold => 0,
        };
        if dir == 0 {
            if let Ok(mut streak) = self.mem_streak.lock() {
                *streak = (0, 0);
            }
            return None;
        }
        let critical = mem.available_mib < 256;
        let pressure = mem.available_mib < 1024;
        let safety = self.cache_mib() > max_cache || mem.available_mib < dynamic_reserve;
        if !pressure && !safety {
            {
                let Ok(mut streak) = self.mem_streak.lock() else {
                    return None;
                };
                if streak.0 == dir {
                    streak.1 = streak.1.saturating_add(1);
                } else {
                    *streak = (dir, 1);
                }
                if streak.1 < 2 {
                    return None;
                }
                *streak = (0, 0);
            }
            // Do not reverse the cache within five minutes unless memory is
            // already critical: grow/shrink flapping evicts the UTXO set.
            if let Ok(last) = self.last_cache_change.lock() {
                if let Some((at, previous)) = *last {
                    if previous != dir && at.elapsed() < Duration::from_secs(300) {
                        return None;
                    }
                }
            }
        }
        let old = self.cache_mib();
        let new = match action {
            CacheAction::Grow => (old + 256).min(max_cache),
            CacheAction::Shrink => {
                if old > max_cache {
                    max_cache.max(self.min_cache_mib)
                } else {
                    // Keep a working set for prevout lookups; collapsing to a
                    // few hundred MiB turns every block into random disk
                    // reads. Under real pressure the floor must not block
                    // returning memory.
                    let floor = if critical {
                        self.min_cache_mib
                    } else {
                        self.cache_floor_mib.min(old.max(1))
                    };
                    // Small steps avoid the 8 GiB -> 4.5 GiB oscillation that
                    // starves the page cache and then grows back.
                    (old * 7 / 8)
                        .max(floor.min(old))
                        .min(max_cache.max(self.min_cache_mib))
                }
            }
            CacheAction::Hold => old,
        };
        let mut readers = self.readers();
        if critical && readers > self.min_readers {
            readers = (readers * 3 / 4).max(self.min_readers);
            self.readers.store(readers, Ordering::Relaxed);
        }
        let mut memtables_shrunk = false;
        let memtables_now = self.memtables_mib.load(Ordering::Relaxed);
        if critical && memtables_now > 512 {
            let target = (memtables_now / 2).max(256);
            self.memtables_mib.store(target, Ordering::Relaxed);
            if let Err(e) = store.set_write_buffer_bytes(target as usize * 1024 * 1024) {
                eprintln!("[e-lite] cannot shrink memtables: {e}");
            }
            memtables_shrunk = true;
            eprintln!(
                "[e-lite] memory: critical available {} MiB; memtable budget {} -> {} MiB",
                mem.available_mib, memtables_now, target
            );
        }
        if new == old && readers == self.readers() && !memtables_shrunk {
            // Nothing changed; keep the streak so the next sample can act.
            if let Ok(mut streak) = self.mem_streak.lock() {
                *streak = (dir, 1);
            }
            return None;
        }
        if new != old {
            self.cache_mib.store(new, Ordering::Relaxed);
            store.set_cache_bytes(new as usize * 1024 * 1024);
            if let Ok(mut last) = self.last_cache_change.lock() {
                *last = Some((Instant::now(), dir));
            }
        }
        let rs = store.compaction_stats();
        eprintln!(
            "[e-lite] memory: available {} MiB, buff/cache {} MiB (sreclaim {}), \
             arc {}, rss {}, effective {} MiB, reserve {} MiB (startup {} MiB), \
             page floor {} MiB, cache {} -> {} MiB (max {} dynamic, profile {}, \
             memtables {} planned / \
             {} real MiB, table readers {} MiB, pending compaction {} MiB, \
             iterative readers {})",
            mem.available_mib,
            mem.cached_mib,
            mem.sreclaimable_mib,
            mem.arc_mib
                .map(|v| v.to_string())
                .unwrap_or_else(|| "none".to_string()),
            process_rss_mib(),
            effective,
            dynamic_reserve,
            self.reserve_mib,
            self.page_cache_floor_mib,
            old,
            new,
            max_cache,
            self.max_cache_mib,
            self.memtables_mib.load(Ordering::Relaxed),
            rs.memtable_bytes / (1024 * 1024),
            rs.table_readers_bytes / (1024 * 1024),
            rs.pending_compaction_bytes / (1024 * 1024),
            readers
        );
        if let Some(stats) = stats {
            self.publish(store, stats);
        }
        // Alpha logs at `debug`: the controller decision, I/O pressure and ARC
        // effectiveness go next to the summary line so a log alone explains
        // every cache change.
        let reason = if critical {
            "critical (<256 MiB available)"
        } else if safety {
            "safety (cache above ceiling or available below reserve)"
        } else if pressure {
            "pressure (<1024 MiB available)"
        } else if dir < 0 {
            "shrink streak (2 samples)"
        } else {
            "grow streak (2 samples)"
        };
        crate::debug_log!(
            "cache-controller: {} {} -> {} MiB ({}; available {} MiB, reserve {} MiB, \
             dynamic ceiling {} MiB, profile cap {} MiB)",
            action.name(),
            old,
            new,
            reason,
            mem.available_mib,
            dynamic_reserve,
            max_cache,
            self.max_cache_mib
        );
        let mut arc_rate: Option<Option<u64>> = None;
        if let Some(now) = zfs_arc_counters() {
            if let Ok(mut prev) = self.arc_prev.lock() {
                arc_rate = Some(match *prev {
                    Some((h, m)) => {
                        let dh = now.0.saturating_sub(h);
                        let dm = now.1.saturating_sub(m);
                        dh.checked_mul(100).and_then(|n| n.checked_div(dh + dm))
                    }
                    None => None,
                });
                *prev = Some(now);
            }
        }
        let pressure_now = io_pressure();
        if arc_rate.is_some() || pressure_now.is_some() {
            let arc_text = match arc_rate {
                None => "n/a".to_string(),
                Some(None) => "hit n/a (no traffic or first sample)".to_string(),
                Some(Some(p)) => format!("hit {p}%"),
            };
            let pressure_text = pressure_now
                .map(|(s10, s60, f10, f60)| {
                    format!("some {s10:.2}/{s60:.2} full {f10:.2}/{f60:.2} avg10/avg60")
                })
                .unwrap_or_else(|| "n/a".to_string());
            crate::debug_log!("io-pressure: {} | arc {}", pressure_text, arc_text);
        }
        self.maybe_warn_uncapped_arc(&mem);
        Some(format!(
            "memory: cache {old} -> {new} MiB, readers {readers}"
        ))
    }

    /// ZFS ARC is invisible to `MemAvailable`; when it grows past a quarter of
    /// RAM the cache controller shrinks on a false signal. Warn with the
    /// recommended cap, rate-limited to once per hour.
    fn maybe_warn_uncapped_arc(&self, mem: &MemoryInfo) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let last = self.arc_warn_at.load(Ordering::Relaxed);
        if last != 0 && now.saturating_sub(last) < 3600 {
            return;
        }
        if warn_uncapped_arc(mem, "[e-lite] WARNING:") {
            self.arc_warn_at.store(now.max(1), Ordering::Relaxed);
        }
    }

    /// Give RAM back before Core resumes downloads. Called once per bulk pause:
    /// Core's block index and verification grow after this point.
    pub fn shrink_for_core_resume(&self, store: &Store) {
        if self.resume_shrunk.swap(true, Ordering::SeqCst) {
            return;
        }
        let mem = memory_info();
        let reserve = self.dynamic_reserve_mib(&mem);
        let old = self.cache_mib();
        // Only give memory back when the OS is actually short. Shrinking
        // unconditionally before Core resumes reduces the cache exactly when
        // the index is about to compete for random reads.
        if mem.available_mib >= reserve.saturating_add(old / 4) {
            return;
        }
        let live = (old * 3 / 4)
            .max(self.cache_floor_mib)
            .min(self.max_cache_mib)
            .max(self.min_cache_mib);
        if old <= live {
            return;
        }
        self.cache_mib.store(live, Ordering::Relaxed);
        store.set_cache_bytes(live as usize * 1024 * 1024);
        eprintln!(
            "[e-lite] memory: Core is about to resume; cache {} -> {} MiB \
             (available {}, buff/cache {}, reserve {})",
            old, live, mem.available_mib, mem.cached_mib, reserve
        );
    }

    /// Arm the one-shot pre-resume shrink again after a new bulk pause.
    pub fn reset_resume_shrink(&self) {
        self.resume_shrunk.store(false, Ordering::SeqCst);
    }
}
