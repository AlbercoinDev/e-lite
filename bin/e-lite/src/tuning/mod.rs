//! Runtime tuning: startup profiles, hot-reloadable knobs and a small
//! event-driven controller.
//!
//! The goal is to keep one binary usable on a slow HDD and on a fast NVMe
//! without rebuilding or restarting. The controller is deliberately
//! conservative: one change per cooldown, hysteresis, hard bounds, and the
//! disk/prune guards always win.

use crate::config::Config;
use el_model::Chain;
use el_serve::Stats;
use el_store::Store;
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Auto,
    Hdd,
    Ssd,
    Nvme,
    Manual,
}

impl Profile {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Some(Profile::Auto),
            "hdd" | "rotational" => Some(Profile::Hdd),
            "ssd" | "sata" => Some(Profile::Ssd),
            "nvme" => Some(Profile::Nvme),
            "manual" => Some(Profile::Manual),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Profile::Auto => "auto",
            Profile::Hdd => "hdd",
            Profile::Ssd => "ssd",
            Profile::Nvme => "nvme",
            Profile::Manual => "manual",
        }
    }

    fn code(self) -> u8 {
        match self {
            Profile::Auto => 0,
            Profile::Hdd => 1,
            Profile::Ssd => 2,
            Profile::Nvme => 3,
            Profile::Manual => 4,
        }
    }

    /// Resolve `auto` using the Linux rotational hint. Virtual disks can lie;
    /// this is only the initial profile, never a hard classification.
    pub fn resolve(self, rotational: bool) -> Profile {
        match self {
            Profile::Auto => {
                if rotational {
                    Profile::Hdd
                } else {
                    Profile::Ssd
                }
            }
            other => other,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Bulk,
    Live,
}

/// Why the current group was committed. The controller must not shrink the
/// batch when the transaction limit (or a call-boundary partial commit) is the
/// reason: the batch is not the binding constraint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitReason {
    Blocks,
    Txs,
    Bytes,
    Time,
    Boundary,
    Shutdown,
}

impl CommitReason {
    pub fn full(self) -> bool {
        !matches!(self, CommitReason::Boundary | CommitReason::Shutdown)
    }
    pub fn code(self) -> u8 {
        match self {
            CommitReason::Blocks => 1,
            CommitReason::Txs => 2,
            CommitReason::Bytes => 3,
            CommitReason::Time => 4,
            CommitReason::Boundary => 5,
            CommitReason::Shutdown => 6,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            CommitReason::Blocks => "blocks",
            CommitReason::Txs => "txs",
            CommitReason::Bytes => "bytes",
            CommitReason::Time => "time",
            CommitReason::Boundary => "boundary",
            CommitReason::Shutdown => "shutdown",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct LastCommit {
    /// Monotonic full-commit id. The controller evaluates each group once.
    pub seq: u64,
    pub ms: u64,
    pub cycle_ms: u64,
    pub blocks: u64,
    pub txs: u64,
    pub bytes: u64,
    pub reason: CommitReason,
    pub txids_us: u64,
    pub db_us: u64,
}

mod controller;
mod machine;
mod memory;

pub use machine::*;
pub use memory::*;

/// Thread-safe hot knobs. Cold settings (`prune_mib`, block source, schema...)
/// stay in `Config` and require a restart.
pub struct Tunables {
    readers: AtomicUsize,
    commit_ms: AtomicU64,
    batch_blocks: AtomicU32,
    max_group_txs: AtomicUsize,
    max_group_bytes: usize,
    cache_mib: AtomicU64,
    prefetch_blocks: AtomicUsize,
    adaptive: AtomicBool,
    readers_locked: AtomicBool,
    commit_locked: AtomicBool,
    batch_locked: AtomicBool,
    profile: AtomicU8,
    rotational: AtomicBool,
    background_jobs: AtomicUsize,

    min_readers: usize,
    max_readers_cap: usize,
    min_commit_ms: u64,
    max_commit_ms: u64,
    /// Bulk and live commit intervals. The controller switches between them
    /// when the mode changes; a bulk commit every 20-60 s is enough for
    /// prune/undo safety and avoids the tiny-commit tax.
    bulk_commit_ms: AtomicU64,
    live_commit_ms: u64,
    commit_min_blocks: u32,
    min_cache_mib: u64,
    max_cache_mib: u64,
    cache_floor_mib: u64,
    page_cache_floor_mib: u64,
    min_batch: u32,
    max_batch: u32,

    // Memory split computed at startup.
    total_mib: u64,
    startup_available_mib: u64,
    effective_mib: u64,
    core_reserve_mib: u64,
    reserve_mib: u64,
    ceiling_mib: u64,
    memtables_mib: AtomicU64,
    runtime_mib: u64,
    mem_streak: Mutex<(i8, u32)>,
    /// Last cache direction change: prevents grow/shrink flapping.
    last_cache_change: Mutex<Option<(Instant, i8)>>,
    /// Previous ZFS ARC hits/misses counters, for delta-based hit rates.
    arc_prev: Mutex<Option<(u64, u64)>>,
    /// Unix seconds of the last uncapped-ARC warning (rate-limited to 1/h).
    arc_warn_at: AtomicU64,
    /// Let e-lite grow/shrink the ZFS ARC cap with the phase (IBD vs live).
    zfs_arc_dynamic: AtomicBool,
    /// ARC cap in MiB during IBD/drain. 0 = half of total RAM.
    zfs_arc_max_mib: AtomicU64,
    /// ARC cap in MiB once fully synced.
    zfs_arc_min_mib: AtomicU64,
    /// True once the dynamic manager wrote the cap successfully; the
    /// uncapped-ARC warning yields to the manager from then on.
    arc_managed: AtomicBool,
    arc_state: Mutex<ArcManagerState>,
    /// Opt-in routine memtable resize. The target must persist for 10 min.
    resume_shrunk: AtomicBool,

    // Exportable performance signals.
    pub read_ms: AtomicU64,
    pub apply_ms: AtomicU64,
    pub decode_ms: AtomicU64,
    pub wait_ms: AtomicU64,
    pub prefetch_ms: AtomicU64,
    /// Recent per-block samples for p50/p95/max; the EWMA above is dominated
    /// by a single slow block and must not drive cache growth.
    prefetch_samples: Mutex<VecDeque<u64>>,
    wait_samples: Mutex<VecDeque<u64>>,
    pub commit_ewma: AtomicU64,
    pub tx_per_sec: AtomicU64,
    pub blocks_per_sec: AtomicU64,
    /// EWMA of the raw serialized block size. The backlog in bytes is
    /// `lag_blocks * avg_block_bytes`; it is the height-independent watermark.
    pub avg_block_bytes: AtomicU64,
    /// Blocks/s over the last commit cycle, used by the drain-time lag policy.
    recent_blocks_per_sec: AtomicU64,
    /// Full commit groups from the last 60 s: `(started, blocks)`. The height
    /// line prints a time-bounded rate from this window, which is honest even
    /// when one group is much larger than its neighbours.
    recent_groups: Mutex<VecDeque<(Instant, u64)>>,
    pub total_read_us: AtomicU64,
    pub total_decode_us: AtomicU64,
    pub total_prefetch_us: AtomicU64,
    pub total_wait_us: AtomicU64,
    pub total_apply_us: AtomicU64,
    pub total_commit_us: AtomicU64,
    /// Commit microseconds divided by the blocks actually in that commit.
    pub commit_per_block_ms: AtomicU64,
    /// Wall-clock between the start of consecutive commits. This is the real
    /// denominator for `commit_share`, not the configured interval.
    pub commit_cycle_ms: AtomicU64,
    /// Periodic controller: number of completed adaptation steps.
    pub adapt_steps: AtomicU64,

    // Core observations so `index` (without a serving Shared) can still tell
    // bulk from live mode.
    core_height: AtomicU64,
    core_ibd: AtomicBool,
    /// Hysteresis for Bulk/Live: leave Bulk only below 500 blocks of lag and
    /// re-enter it above 2,000, so a noisy tip does not flap the mode.
    mode_live: AtomicBool,

    last_change: Mutex<Instant>,
    /// Two consecutive samples are required before changing the reader pool.
    reader_streak: Mutex<(i8, u32)>,
    /// Serialises the 30 s rate-limit check with the mutation so two callers
    /// cannot both pass it (the old code checked under one lock and wrote
    /// under another).
    adapt_gate: Mutex<()>,
    last_commit_start: Mutex<Option<Instant>>,
    /// Last successful prune. Commit decisions are skipped for 30 s after it:
    /// the prune RPC/WAL barrier inflates the cycle and would otherwise look
    /// like commit pain.
    last_prune: Mutex<Option<Instant>>,
    /// Last *full* commit group, with its matching cycle and cost breakdown.
    /// Boundary partial commits are deliberately excluded from adaptation.
    last_commit: Mutex<Option<LastCommit>>,
    /// Id of the last full commit already evaluated by the controller.
    last_commit_eval: AtomicU64,
    /// Monotonic id assigned to each full commit.
    commit_seq: AtomicU64,
}

impl Tunables {
    #[cfg(test)]
    pub fn new(cfg: &Config, rotational: bool) -> Self {
        Self::new_with_plan(cfg, rotational, None)
    }

    /// Build the runtime knobs from a detected machine plan. Explicit config
    /// values still win; the plan is the machine-derived default.
    pub fn new_with_plan(cfg: &Config, rotational: bool, plan: Option<&MachinePlan>) -> Self {
        let requested = Profile::parse(&cfg.index.profile).unwrap_or_else(|| {
            eprintln!(
                "[e-lite] unknown [index] profile {:?}; using auto",
                cfg.index.profile
            );
            Profile::Auto
        });
        let cpu = plan.map(|p| p.cpu.clone()).unwrap_or_else(CpuInfo::detect);
        let cpus = cpu.effective.max(1);
        let disk_rotational = plan
            .map(|p| p.data_disk.kind == DiskKind::Hdd)
            .unwrap_or(rotational);
        let profile = requested.resolve(disk_rotational);
        let ram_mib = plan
            .and_then(|p| p.mem.total_mib)
            .or_else(total_ram_mib)
            .unwrap_or(1024)
            .max(256);

        let (
            mut readers,
            mut default_commit,
            mut default_batch,
            mut default_cache,
            mut profile_jobs,
        ) = match profile {
            Profile::Hdd => (
                2usize,
                45_000u64,
                512u32,
                (ram_mib / 8).clamp(512, 4096),
                2usize,
            ),
            Profile::Ssd => (
                cpus.clamp(2, 4),
                30_000,
                512,
                (ram_mib / 4).clamp(1024, 8192),
                (cpus / 2).clamp(2, 4),
            ),
            Profile::Nvme => (
                cpus.clamp(2, 8),
                20_000,
                512,
                (ram_mib / 4).clamp(1024, 8192),
                (cpus / 2).clamp(2, 4),
            ),
            Profile::Manual | Profile::Auto => (
                if cfg.index.readers > 0 {
                    cfg.index.readers
                } else {
                    cpus.min(8)
                },
                30_000,
                512,
                if cfg.index.cache_mib > 0 {
                    cfg.index.cache_mib
                } else {
                    (ram_mib / 4).clamp(1024, 8192)
                },
                cpus.min(4),
            ),
        };
        if let Some(p) = plan {
            readers = p.readers;
            default_commit = p.commit_ms;
            default_batch = p.batch_blocks;
            default_cache = p.cache_mib;
            profile_jobs = p.jobs;
        }
        let requested_jobs = if cfg.index.background_jobs > 0 {
            cfg.index.background_jobs
        } else {
            profile_jobs
        };
        // Never over-subscribe a small machine with RocksDB workers.
        let jobs = requested_jobs.clamp(1, 32).min(cpus.max(1));

        if cfg.index.readers > 0 {
            readers = cfg.index.readers;
        }
        let commit_ms = if cfg.index.commit_interval_ms > 0 {
            cfg.index.commit_interval_ms
        } else {
            default_commit
        };
        let batch_blocks = if cfg.index.batch_blocks > 0 && cfg.index.batch_blocks != 256 {
            cfg.index.batch_blocks
        } else {
            default_batch
        };
        let mem_now = memory_info();
        let core_reserve = plan.map(|p| p.core_reserve_mib).unwrap_or_else(|| {
            if cfg.core.manage == crate::config::CoreManage::External {
                0
            } else {
                fitted_dbcache_mib(ram_mib, mem_now.available_mib)
            }
        });
        let budget = budget_with(
            cfg,
            ram_mib,
            mem_now.available_mib,
            mem_now.cached_mib,
            None,
            None,
            core_reserve,
        );
        // The startup cache is fitted to what the OS can give now; the
        // profile only caps how far the hot controller may grow.
        let max_cache_mib = budget.cache_max_mib.min(default_cache.max(8)).max(8);
        let cache_floor_mib = budget.cache_floor_mib.min(max_cache_mib).max(8);
        let cache_mib = if cfg.index.cache_mib > 0 {
            cfg.index.cache_mib.min(max_cache_mib)
        } else {
            budget.cache_start_mib.min(max_cache_mib)
        };
        if budget.over_budget {
            eprintln!(
                "[e-lite] WARNING: memory plan ({} MiB) exceeds the {} MiB ceiling; using \
                 fitted values",
                budget.planned_mib, budget.ceiling_mib
            );
        }
        // Keep a single hard ceiling; the profile only changes the starting
        // point. The controller never raises readers on a rotational disk.
        // The plan's reader count is the cap for both the initial value and
        // the controller. On a 4C/8T box that is 4, not 8 or 16.
        let max_readers = plan
            .map(|p| p.readers.max(1))
            .unwrap_or_else(|| match profile {
                Profile::Hdd => 4,
                Profile::Ssd => 8,
                Profile::Nvme => 16,
                Profile::Auto | Profile::Manual => 32,
            });
        let min_readers = plan
            .map(|p| (p.cpu.physical_effective / 2).max(2))
            .unwrap_or(2)
            .min(max_readers);
        let cpu_reader_cap = cpus.max(2);
        let readers = if cfg.index.readers > 0 {
            readers.clamp(1, max_readers)
        } else {
            readers.clamp(1, max_readers).min(cpu_reader_cap)
        };
        // `clamp` panics when min > max; a tight budget can leave less than
        // 64 MiB, and that must degrade instead of aborting the process.
        let cache_mib = cache_mib.min(max_cache_mib).max(8.min(max_cache_mib));
        let commit_ms = commit_ms.clamp(500, 60_000);
        let batch_blocks = batch_blocks.clamp(16, 512);
        let max_group_txs = plan
            .map(|p| p.max_group_txs)
            .unwrap_or(if profile == Profile::Hdd {
                500_000
            } else {
                250_000
            });
        let max_group_bytes = plan.map(|p| p.max_group_bytes).unwrap_or(256 * 1024 * 1024);

        Tunables {
            readers: AtomicUsize::new(readers),
            commit_ms: AtomicU64::new(commit_ms),
            batch_blocks: AtomicU32::new(batch_blocks),
            max_group_txs: AtomicUsize::new(max_group_txs),
            max_group_bytes,
            cache_mib: AtomicU64::new(cache_mib),
            // 0 means "auto": the pipeline keeps its default window.
            prefetch_blocks: AtomicUsize::new(cfg.index.prefetch_blocks),
            adaptive: AtomicBool::new(cfg.index.adaptive),
            readers_locked: AtomicBool::new(cfg.index.readers > 0),
            commit_locked: AtomicBool::new(cfg.index.commit_interval_ms > 0),
            batch_locked: AtomicBool::new(
                cfg.index.batch_blocks > 0 && cfg.index.batch_blocks != 256,
            ),
            profile: AtomicU8::new(profile.code()),
            rotational: AtomicBool::new(rotational),
            background_jobs: AtomicUsize::new(jobs.clamp(1, 32)),
            min_readers,
            max_readers_cap: max_readers,
            min_commit_ms: 500,
            max_commit_ms: 60_000,
            bulk_commit_ms: AtomicU64::new(commit_ms),
            live_commit_ms: 1_500,
            commit_min_blocks: 8,
            min_cache_mib: 8,
            max_cache_mib,
            cache_floor_mib,
            page_cache_floor_mib: budget.page_cache_floor_mib,
            min_batch: 16,
            // The catch-up checkpoint commits every 512 blocks. A larger
            // batch can never be filled and only makes the controller look
            // like it is tuning something it cannot control.
            max_batch: 512,
            total_mib: ram_mib,
            startup_available_mib: mem_now.available_mib,
            effective_mib: budget.effective_mib,
            core_reserve_mib: budget.core_reserve_mib,
            reserve_mib: budget.reserve_mib,
            ceiling_mib: budget.ceiling_mib,
            memtables_mib: AtomicU64::new(budget.memtables_mib),
            runtime_mib: budget.runtime_mib,
            mem_streak: Mutex::new((0, 0)),
            last_cache_change: Mutex::new(None),
            arc_prev: Mutex::new(None),
            arc_warn_at: AtomicU64::new(0),
            zfs_arc_dynamic: AtomicBool::new(cfg.index.zfs_arc_dynamic),
            zfs_arc_max_mib: AtomicU64::new(cfg.index.zfs_arc_max_mib),
            zfs_arc_min_mib: AtomicU64::new(cfg.index.zfs_arc_min_mib),
            arc_managed: AtomicBool::new(false),
            arc_state: Mutex::new(ArcManagerState::default()),
            reader_streak: Mutex::new((0, 0)),
            resume_shrunk: AtomicBool::new(false),
            read_ms: AtomicU64::new(0),
            apply_ms: AtomicU64::new(0),
            decode_ms: AtomicU64::new(0),
            wait_ms: AtomicU64::new(0),
            prefetch_ms: AtomicU64::new(0),
            prefetch_samples: Mutex::new(VecDeque::with_capacity(256)),
            wait_samples: Mutex::new(VecDeque::with_capacity(256)),
            commit_ewma: AtomicU64::new(0),
            tx_per_sec: AtomicU64::new(0),
            blocks_per_sec: AtomicU64::new(0),
            avg_block_bytes: AtomicU64::new(1_500_000),
            recent_blocks_per_sec: AtomicU64::new(0),
            recent_groups: Mutex::new(VecDeque::with_capacity(128)),
            total_read_us: AtomicU64::new(0),
            total_decode_us: AtomicU64::new(0),
            total_prefetch_us: AtomicU64::new(0),
            total_wait_us: AtomicU64::new(0),
            total_apply_us: AtomicU64::new(0),
            total_commit_us: AtomicU64::new(0),
            commit_per_block_ms: AtomicU64::new(0),
            commit_cycle_ms: AtomicU64::new(0),
            adapt_steps: AtomicU64::new(0),
            core_height: AtomicU64::new(0),
            core_ibd: AtomicBool::new(false),
            mode_live: AtomicBool::new(false),
            last_change: Mutex::new(
                Instant::now()
                    .checked_sub(Duration::from_secs(120))
                    .unwrap_or_else(Instant::now),
            ),
            adapt_gate: Mutex::new(()),
            last_commit_start: Mutex::new(None),
            last_prune: Mutex::new(None),
            last_commit: Mutex::new(None),
            last_commit_eval: AtomicU64::new(0),
            commit_seq: AtomicU64::new(0),
        }
    }

    pub fn readers(&self) -> usize {
        self.readers.load(Ordering::Relaxed)
    }
    pub fn commit_interval(&self) -> Duration {
        Duration::from_millis(self.commit_ms.load(Ordering::Relaxed))
    }
    pub fn commit_ms(&self) -> u64 {
        self.commit_ms.load(Ordering::Relaxed)
    }
    /// Configured bulk commit time limit (20-60 s per disk class). The
    /// commit-spike warn uses it to tell a normal full-interval group from a
    /// real overshoot.
    pub fn bulk_commit_ms_limit(&self) -> u64 {
        self.bulk_commit_ms.load(Ordering::Relaxed)
    }
    pub fn commit_min_blocks(&self) -> u32 {
        self.commit_min_blocks
    }
    pub fn batch_blocks(&self) -> u32 {
        self.batch_blocks.load(Ordering::Relaxed)
    }
    pub fn max_group_txs(&self) -> usize {
        self.max_group_txs.load(Ordering::Relaxed)
    }
    pub fn max_group_bytes(&self) -> usize {
        self.max_group_bytes
    }
    pub fn cache_mib(&self) -> u64 {
        self.cache_mib.load(Ordering::Relaxed)
    }
    pub fn prefetch_blocks(&self) -> usize {
        self.prefetch_blocks.load(Ordering::Relaxed)
    }
    pub fn decode_ms(&self) -> u64 {
        self.decode_ms.load(Ordering::Relaxed)
    }
    pub fn wait_ms(&self) -> u64 {
        self.wait_ms.load(Ordering::Relaxed)
    }
    pub fn prefetch_ms(&self) -> u64 {
        self.prefetch_ms.load(Ordering::Relaxed)
    }
    pub fn prefetch_percentiles(&self) -> (u64, u64, u64) {
        percentiles_ms(&self.prefetch_samples)
    }
    pub fn wait_percentiles(&self) -> (u64, u64) {
        let (p50, p95, _) = percentiles_ms(&self.wait_samples);
        (p50, p95)
    }

    /// Apply cache capacity and mutable RocksDB options to an open store.
    pub fn apply_store(&self, store: &Store) {
        store.set_cache_bytes(self.cache_mib().saturating_mul(1024 * 1024) as usize);
    }
    pub fn background_jobs(&self) -> usize {
        self.background_jobs.load(Ordering::Relaxed)
    }
    pub fn adaptive(&self) -> bool {
        self.adaptive.load(Ordering::Relaxed)
    }
    pub fn profile(&self) -> Profile {
        match self.profile.load(Ordering::Relaxed) {
            1 => Profile::Hdd,
            2 => Profile::Ssd,
            3 => Profile::Nvme,
            4 => Profile::Manual,
            _ => Profile::Auto,
        }
    }
    pub fn rotational(&self) -> bool {
        self.rotational.load(Ordering::Relaxed)
    }

    fn auto_max_readers(&self) -> usize {
        self.max_readers_cap.max(1)
    }

    fn memory_reader_cap(&self) -> usize {
        // Leave memory for Core, RocksDB and the OS. 128 MiB per worker is a
        // conservative allowance for a block buffer plus queue slack.
        let avail = mem_available_mib().max(256);
        ((avail / 128) as usize).max(1)
    }

    pub fn record_read(&self, d: Duration) {
        let us = d.as_micros() as u64;
        ewma(&self.read_ms, d.as_millis() as u64);
        self.total_read_us.fetch_add(us, Ordering::Relaxed);
    }
    pub fn record_apply(&self, d: Duration) {
        let us = d.as_micros() as u64;
        ewma(&self.apply_ms, d.as_millis() as u64);
        self.total_apply_us.fetch_add(us, Ordering::Relaxed);
    }
    pub fn record_decode(&self, d: Duration) {
        let us = d.as_micros() as u64;
        ewma(&self.decode_ms, d.as_millis() as u64);
        self.total_decode_us.fetch_add(us, Ordering::Relaxed);
    }
    pub fn record_wait(&self, d: Duration) {
        let us = d.as_micros() as u64;
        ewma(&self.wait_ms, d.as_millis() as u64);
        push_sample(&self.wait_samples, us);
        self.total_wait_us.fetch_add(us, Ordering::Relaxed);
    }
    pub fn record_prefetch(&self, d: Duration) {
        let us = d.as_micros() as u64;
        ewma(&self.prefetch_ms, d.as_millis() as u64);
        push_sample(&self.prefetch_samples, us);
        self.total_prefetch_us.fetch_add(us, Ordering::Relaxed);
    }
    pub fn record_commit(&self, d: Duration) {
        let us = d.as_micros() as u64;
        ewma(&self.commit_ewma, d.as_millis() as u64);
        self.total_commit_us.fetch_add(us, Ordering::Relaxed);
    }

    /// Record a commit together with the number of blocks it actually
    /// contained. `commit/blk` must not divide by the configured batch size:
    /// transaction limits and time-based commits produce smaller groups.
    #[allow(clippy::too_many_arguments)]
    pub fn record_commit_group(
        &self,
        started: Instant,
        d: Duration,
        blocks: u64,
        txs: u64,
        bytes: u64,
        reason: CommitReason,
        txids_us: u64,
        db_us: u64,
    ) {
        if !reason.full() {
            // Boundary/partial commits are ignored by the controller: a
            // 1-block boundary commit used to deflate the EWMA and make the
            // next share look like 76-118%.
            return;
        }
        self.record_commit(d);
        let per_block = (d.as_millis() as u64) / blocks.max(1);
        self.commit_per_block_ms.store(per_block, Ordering::Relaxed);
        if let Ok(mut groups) = self.recent_groups.lock() {
            groups.push_back((started, blocks));
            let cutoff = Instant::now()
                .checked_sub(Duration::from_secs(60))
                .unwrap_or_else(Instant::now);
            while groups.len() > 1 && groups.front().map(|(at, _)| *at < cutoff).unwrap_or(false) {
                groups.pop_front();
            }
            while groups.len() > 128 {
                groups.pop_front();
            }
        }
        let cycle = if let Ok(mut last) = self.last_commit_start.lock() {
            let cycle = last
                .map(|prev| started.saturating_duration_since(prev).as_millis() as u64)
                .unwrap_or(0);
            *last = Some(started);
            cycle
        } else {
            0
        };
        if cycle > 0 {
            self.commit_cycle_ms.store(cycle, Ordering::Relaxed);
            self.recent_blocks_per_sec.store(
                blocks.saturating_mul(1_000).checked_div(cycle).unwrap_or(0),
                Ordering::Relaxed,
            );
        }
        if let Ok(mut slot) = self.last_commit.lock() {
            let seq = self.commit_seq.fetch_add(1, Ordering::Relaxed) + 1;
            *slot = Some(LastCommit {
                seq,
                ms: d.as_millis() as u64,
                cycle_ms: cycle,
                blocks,
                txs,
                bytes,
                reason,
                txids_us,
                db_us,
            });
        }
    }

    pub fn last_commit(&self) -> Option<LastCommit> {
        self.last_commit.lock().ok().and_then(|slot| *slot)
    }

    /// Blocks/s over the last 60 s of full commit groups. Unlike the
    /// cumulative `blocks/s` and the phase average, this tracks the real
    /// current rate: a 10k-block signet interval at 37 b/s reports 37, not
    /// the session average of 113.
    pub fn window_rate(&self) -> u64 {
        let Ok(groups) = self.recent_groups.lock() else {
            return 0;
        };
        let now = Instant::now();
        let cutoff = now
            .checked_sub(Duration::from_secs(60))
            .unwrap_or_else(Instant::now);
        let mut blocks = 0u64;
        let mut oldest: Option<Instant> = None;
        for (at, count) in groups.iter() {
            if *at < cutoff {
                continue;
            }
            blocks = blocks.saturating_add(*count);
            oldest = Some(match oldest {
                Some(current) if current <= *at => current,
                _ => *at,
            });
        }
        let Some(oldest) = oldest else {
            return 0;
        };
        let span = now.saturating_duration_since(oldest).as_secs_f64().max(1.0);
        (blocks as f64 / span).round() as u64
    }

    /// Blocks/s over the last commit cycle; falls back to the since-start
    /// average only until a cycle is available. Do not take the maximum: an
    /// optimistic rate would allow a larger backlog than the target.
    pub fn recent_blocks_rate(&self) -> u64 {
        let recent = self.recent_blocks_per_sec.load(Ordering::Relaxed);
        if recent > 0 {
            recent
        } else {
            self.blocks_per_sec.load(Ordering::Relaxed)
        }
    }

    /// Called after a successful prune so the controller ignores the stall.
    pub fn note_prune(&self) {
        if let Ok(mut last) = self.last_prune.lock() {
            *last = Some(Instant::now());
        }
    }

    fn prune_recent(&self) -> bool {
        self.last_prune
            .lock()
            .ok()
            .and_then(|last| *last)
            .map(|at| at.elapsed() < Duration::from_secs(30))
            .unwrap_or(false)
    }

    /// Update the Core observations used to classify bulk vs live mode.
    pub fn set_core_state(&self, height: u64, ibd: bool) {
        self.core_height.store(height, Ordering::Relaxed);
        self.core_ibd.store(ibd, Ordering::Relaxed);
    }

    pub fn core_height(&self) -> u64 {
        self.core_height.load(Ordering::Relaxed)
    }

    pub fn core_ibd(&self) -> bool {
        self.core_ibd.load(Ordering::Relaxed)
    }

    pub fn mode_for(&self, committed: u64) -> (Mode, u64) {
        let core = self.core_height();
        let lag = core.saturating_sub(committed);
        let ibd = self.core_ibd.load(Ordering::Relaxed);
        let mut live = self.mode_live.load(Ordering::Relaxed);
        if ibd || lag > 2_000 {
            live = false;
        } else if lag <= 500 {
            live = true;
        }
        let was_live = self.mode_live.swap(live, Ordering::Relaxed);
        if live != was_live && !self.commit_locked.load(Ordering::Relaxed) {
            // Bulk can afford 20-60 s between commits; live must stay short
            // for wallet latency and strict sync.
            self.commit_ms.store(
                if live {
                    self.live_commit_ms
                } else {
                    self.bulk_commit_ms.load(Ordering::Relaxed)
                },
                Ordering::Relaxed,
            );
        }
        (if live { Mode::Live } else { Mode::Bulk }, lag)
    }
    pub fn record_tx_rate(&self, tx_per_sec: u64) {
        ewma(&self.tx_per_sec, tx_per_sec);
    }
    pub fn record_blocks_rate(&self, blocks_per_sec: u64) {
        ewma(&self.blocks_per_sec, blocks_per_sec);
    }
    pub fn record_block_bytes(&self, bytes: usize) {
        if bytes > 0 {
            ewma_long(&self.avg_block_bytes, bytes as u64);
        }
    }
    pub fn avg_block_bytes(&self) -> u64 {
        self.avg_block_bytes.load(Ordering::Relaxed).max(1)
    }

    fn apply_profile_defaults(&self, profile: Profile) {
        let phys = CpuInfo::detect().physical_effective.max(1);
        let reader_cap = self.max_readers_cap.max(1);
        match profile {
            Profile::Hdd => {
                self.readers
                    .store(2usize.min(reader_cap), Ordering::Relaxed);
                self.commit_ms.store(45_000, Ordering::Relaxed);
                self.bulk_commit_ms.store(45_000, Ordering::Relaxed);
                self.batch_blocks.store(512, Ordering::Relaxed);
                self.max_group_txs.store(500_000, Ordering::Relaxed);
                self.background_jobs.store(2.min(phys), Ordering::Relaxed);
            }
            Profile::Ssd => {
                self.readers
                    .store(phys.clamp(2, 4).min(reader_cap), Ordering::Relaxed);
                self.commit_ms.store(30_000, Ordering::Relaxed);
                self.bulk_commit_ms.store(30_000, Ordering::Relaxed);
                self.batch_blocks.store(512, Ordering::Relaxed);
                self.max_group_txs.store(250_000, Ordering::Relaxed);
                // The pool splits between flush and compaction, so `phys/2`
                // gave a 4-core host a single compaction thread. Bulk ingest is
                // I/O-bound (CPU is mostly idle) and a single thread let
                // `pending_compaction` grow ~70 GiB/h at drain rates; give
                // compaction half of the physical cores (2 flush + N/2
                // compaction).
                self.background_jobs
                    .store(phys.clamp(2, 8), Ordering::Relaxed);
            }
            Profile::Nvme => {
                self.readers
                    .store(phys.clamp(2, 8).min(reader_cap), Ordering::Relaxed);
                self.commit_ms.store(20_000, Ordering::Relaxed);
                self.bulk_commit_ms.store(20_000, Ordering::Relaxed);
                self.batch_blocks.store(512, Ordering::Relaxed);
                self.max_group_txs.store(250_000, Ordering::Relaxed);
                self.background_jobs
                    .store(phys.clamp(2, 8), Ordering::Relaxed);
            }
            Profile::Auto | Profile::Manual => {}
        }
    }

    /// Update the hot fields from a reloaded config. Cold fields are ignored.
    pub fn reload(&self, cfg: &Config) -> Vec<&'static str> {
        let mut changed = Vec::new();
        let requested = Profile::parse(&cfg.index.profile).unwrap_or(Profile::Auto);
        let resolved = requested.resolve(self.rotational());
        if resolved.code() != self.profile.load(Ordering::Relaxed) {
            self.profile.store(resolved.code(), Ordering::Relaxed);
            self.apply_profile_defaults(resolved);
            changed.push("profile");
        }
        self.adaptive.store(cfg.index.adaptive, Ordering::Relaxed);
        self.readers_locked
            .store(cfg.index.readers > 0, Ordering::Relaxed);
        self.commit_locked
            .store(cfg.index.commit_interval_ms > 0, Ordering::Relaxed);
        self.batch_locked.store(
            cfg.index.batch_blocks > 0 && cfg.index.batch_blocks != 256,
            Ordering::Relaxed,
        );
        if cfg.index.background_jobs > 0 {
            let cpus = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4);
            let v = cfg.index.background_jobs.clamp(1, 32).min(cpus.max(1));
            if v != self.background_jobs() {
                self.background_jobs.store(v, Ordering::Relaxed);
                changed.push("background_jobs");
            }
        }
        if self.zfs_arc_dynamic.load(Ordering::Relaxed) != cfg.index.zfs_arc_dynamic {
            self.zfs_arc_dynamic
                .store(cfg.index.zfs_arc_dynamic, Ordering::Relaxed);
            changed.push("zfs_arc_dynamic");
        }
        if self.zfs_arc_max_mib.load(Ordering::Relaxed) != cfg.index.zfs_arc_max_mib {
            self.zfs_arc_max_mib
                .store(cfg.index.zfs_arc_max_mib, Ordering::Relaxed);
            changed.push("zfs_arc_max_mib");
        }
        if self.zfs_arc_min_mib.load(Ordering::Relaxed) != cfg.index.zfs_arc_min_mib {
            self.zfs_arc_min_mib
                .store(cfg.index.zfs_arc_min_mib, Ordering::Relaxed);
            changed.push("zfs_arc_min_mib");
        }
        if cfg.index.readers > 0 {
            let v = cfg
                .index
                .readers
                .clamp(self.min_readers, self.max_readers_cap);
            if v != self.readers() {
                self.readers.store(v, Ordering::Relaxed);
                changed.push("readers");
            }
        }
        if cfg.index.commit_interval_ms > 0 {
            let v = cfg
                .index
                .commit_interval_ms
                .clamp(self.min_commit_ms, self.max_commit_ms);
            if v != self.commit_ms() {
                self.commit_ms.store(v, Ordering::Relaxed);
                self.bulk_commit_ms.store(v, Ordering::Relaxed);
                changed.push("commit_interval_ms");
            }
        }
        if cfg.index.batch_blocks > 0 && cfg.index.batch_blocks != 256 {
            let v = cfg.index.batch_blocks.clamp(self.min_batch, self.max_batch);
            if v != self.batch_blocks() {
                self.batch_blocks.store(v, Ordering::Relaxed);
                changed.push("batch_blocks");
            }
        }
        if cfg.index.cache_mib > 0 {
            let v = cfg
                .index
                .cache_mib
                .clamp(self.min_cache_mib, self.max_cache_mib);
            if v != self.cache_mib() {
                self.cache_mib.store(v, Ordering::Relaxed);
                changed.push("cache_mib");
            }
        }
        changed
    }

    pub fn memory_numbers(&self) -> (u64, u64, u64, u64, u64, u64, u64, u64) {
        (
            self.total_mib,
            self.startup_available_mib,
            self.effective_mib,
            self.core_reserve_mib,
            self.reserve_mib,
            self.ceiling_mib,
            self.memtables_mib.load(Ordering::Relaxed),
            self.runtime_mib,
        )
    }

    /// Export current hot values and metrics to the shared status. The store
    /// provides the RocksDB-side memory that does not live in the block cache
    /// (memtable, table readers); without it the memory plan is not a real
    /// budget.
    pub fn publish(&self, store: &Store, stats: &Stats) {
        stats
            .tuning_readers
            .store(self.readers() as u64, Ordering::Relaxed);
        stats
            .tuning_commit_ms
            .store(self.commit_ms(), Ordering::Relaxed);
        stats
            .tuning_batch_blocks
            .store(self.batch_blocks() as u64, Ordering::Relaxed);
        stats
            .tuning_cache_mib
            .store(self.cache_mib(), Ordering::Relaxed);
        stats
            .tuning_read_ms
            .store(self.read_ms.load(Ordering::Relaxed), Ordering::Relaxed);
        stats
            .tuning_apply_ms
            .store(self.apply_ms.load(Ordering::Relaxed), Ordering::Relaxed);
        stats
            .tuning_decode_ms
            .store(self.decode_ms.load(Ordering::Relaxed), Ordering::Relaxed);
        stats
            .tuning_wait_ms
            .store(self.wait_ms.load(Ordering::Relaxed), Ordering::Relaxed);
        let (prefetch_p50, prefetch_p95, prefetch_max) = self.prefetch_percentiles();
        stats
            .tuning_prefetch_p50_ms
            .store(prefetch_p50, Ordering::Relaxed);
        stats
            .tuning_prefetch_p95_ms
            .store(prefetch_p95, Ordering::Relaxed);
        stats
            .tuning_prefetch_max_ms
            .store(prefetch_max, Ordering::Relaxed);
        let (wait_p50, wait_p95) = self.wait_percentiles();
        stats.tuning_wait_p50_ms.store(wait_p50, Ordering::Relaxed);
        stats.tuning_wait_p95_ms.store(wait_p95, Ordering::Relaxed);
        stats
            .tuning_commit_ms_ewma
            .store(self.commit_ewma.load(Ordering::Relaxed), Ordering::Relaxed);
        stats.tuning_commit_per_block_ms.store(
            self.commit_per_block_ms.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        stats.tuning_commit_cycle_ms.store(
            self.commit_cycle_ms.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        if let Some(last) = self.last_commit() {
            stats
                .tuning_commit_last_ms
                .store(last.ms, Ordering::Relaxed);
            stats
                .tuning_commit_last_cycle_ms
                .store(last.cycle_ms, Ordering::Relaxed);
            stats
                .tuning_commit_last_blocks
                .store(last.blocks, Ordering::Relaxed);
            stats
                .tuning_commit_last_txs
                .store(last.txs, Ordering::Relaxed);
            stats
                .tuning_commit_last_bytes
                .store(last.bytes, Ordering::Relaxed);
            stats
                .tuning_commit_last_txids_us
                .store(last.txids_us, Ordering::Relaxed);
            stats
                .tuning_commit_last_db_us
                .store(last.db_us, Ordering::Relaxed);
            stats
                .tuning_commit_last_reason
                .store(last.reason.code() as u64, Ordering::Relaxed);
        }
        stats
            .tuning_adapt_steps
            .store(self.adapt_steps.load(Ordering::Relaxed), Ordering::Relaxed);
        stats
            .tuning_tx_per_sec
            .store(self.tx_per_sec.load(Ordering::Relaxed), Ordering::Relaxed);
        stats.tuning_blocks_per_sec.store(
            self.blocks_per_sec.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        stats.stage_read_us.store(
            self.total_read_us.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        stats.stage_decode_us.store(
            self.total_decode_us.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        stats.stage_prefetch_us.store(
            self.total_prefetch_us.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        stats.stage_wait_us.store(
            self.total_wait_us.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        stats.stage_apply_us.store(
            self.total_apply_us.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        stats.stage_commit_us.store(
            self.total_commit_us.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        stats
            .tuning_profile
            .store(self.profile().code() as u64, Ordering::Relaxed);
        stats
            .tuning_adaptive
            .store(self.adaptive(), Ordering::Relaxed);
        stats
            .tuning_background_jobs
            .store(self.background_jobs() as u64, Ordering::Relaxed);
        let mem = memory_info();
        stats
            .mem_total_mib
            .store(mem.total_mib.unwrap_or(0), Ordering::Relaxed);
        stats
            .mem_available_mib
            .store(mem.available_mib, Ordering::Relaxed);
        stats
            .mem_cgroup_limit_mib
            .store(mem.cgroup_limit_mib.unwrap_or(0), Ordering::Relaxed);
        stats
            .mem_cgroup_used_mib
            .store(mem.cgroup_used_mib.unwrap_or(0), Ordering::Relaxed);
        stats
            .mem_reclaimable_mib
            .store(mem.reclaimable_mib.unwrap_or(0), Ordering::Relaxed);
        stats
            .mem_cached_mib
            .store(mem.cached_mib, Ordering::Relaxed);
        stats
            .mem_cache_max_mib
            .store(self.max_cache_mib, Ordering::Relaxed);
        stats.mem_memtables_mib.store(
            self.memtables_mib.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        let rs = store.compaction_stats();
        // The planned memtable budget and the memory RocksDB actually holds
        // are not the same value, and table readers live outside the block
        // cache. Both must be visible or the cache looks over-budgeted.
        stats
            .mem_memtable_bytes_mib
            .store(rs.memtable_bytes / (1024 * 1024), Ordering::Relaxed);
        stats
            .mem_table_readers_mib
            .store(rs.table_readers_bytes / (1024 * 1024), Ordering::Relaxed);
        let mem = memory_info();
        stats
            .mem_effective_mib
            .store(self.total_mib.min(mem.available_mib), Ordering::Relaxed);
        stats
            .mem_dynamic_reserve_mib
            .store(self.dynamic_reserve_mib(&mem), Ordering::Relaxed);
        stats
            .mem_reserve_mib
            .store(self.reserve_mib, Ordering::Relaxed);
        stats
            .mem_budget_mib
            .store(self.ceiling_mib, Ordering::Relaxed);
    }
}

fn ewma(slot: &AtomicU64, sample: u64) {
    let old = slot.load(Ordering::Relaxed);
    let next = if old == 0 {
        sample
    } else {
        (old.saturating_mul(7).saturating_add(sample)) / 8
    };
    slot.store(next, Ordering::Relaxed);
}

/// Slow average for the block-size watermark. A 1/8 EWMA reacts to a handful
/// of large blocks and made the byte backlog jump by 3-4x between pause and
/// resume; 1/1024 spans ~1000 blocks, enough to follow a real size trend
/// without chasing a burst.
fn ewma_long(slot: &AtomicU64, sample: u64) {
    let old = slot.load(Ordering::Relaxed);
    let next = if old == 0 {
        sample
    } else {
        old.saturating_mul(1023)
            .saturating_add(sample)
            .saturating_div(1024)
    };
    slot.store(next, Ordering::Relaxed);
}

fn push_sample(slot: &Mutex<VecDeque<u64>>, sample_us: u64) {
    if let Ok(mut samples) = slot.lock() {
        if samples.len() >= 256 {
            samples.pop_front();
        }
        samples.push_back(sample_us);
    }
}

fn percentiles_ms(slot: &Mutex<VecDeque<u64>>) -> (u64, u64, u64) {
    let Ok(samples) = slot.lock() else {
        return (0, 0, 0);
    };
    if samples.is_empty() {
        return (0, 0, 0);
    }
    let mut v: Vec<u64> = samples.iter().copied().collect();
    v.sort_unstable();
    let p50 = v[((v.len() * 50).div_ceil(100)).saturating_sub(1)];
    let p95 = v[((v.len() * 95).div_ceil(100)).saturating_sub(1)];
    let max = *v.last().unwrap_or(&0);
    (p50 / 1000, p95 / 1000, max / 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_parse_and_resolve() {
        assert_eq!(Profile::parse("HDD"), Some(Profile::Hdd));
        assert_eq!(Profile::Auto.resolve(true), Profile::Hdd);
        assert_eq!(Profile::Auto.resolve(false), Profile::Ssd);
        assert_eq!(Profile::Nvme.resolve(true), Profile::Nvme);
    }

    #[test]
    fn ewma_moves_towards_sample() {
        let slot = AtomicU64::new(0);
        ewma(&slot, 100);
        assert_eq!(slot.load(Ordering::Relaxed), 100);
        ewma(&slot, 200);
        assert_eq!(slot.load(Ordering::Relaxed), 112);
    }

    #[test]
    fn long_block_average_ignores_a_short_burst() {
        let slot = AtomicU64::new(700_000);
        for _ in 0..8 {
            ewma_long(&slot, 4_000_000);
        }
        let after = slot.load(Ordering::Relaxed);
        // Eight huge blocks move the watermark by <1 %, so a burst cannot
        // pause Core months early by itself.
        assert!(after < 730_000, "average jumped to {after}");
        for _ in 0..4096 {
            ewma_long(&slot, 700_000);
        }
        assert!(slot.load(Ordering::Relaxed) < 710_000);
    }

    #[test]
    fn percentiles_are_taken_from_recent_samples() {
        let slot = Mutex::new(VecDeque::new());
        for v in 1..=100 {
            push_sample(&slot, v * 1_000);
        }
        let (p50, p95, max) = percentiles_ms(&slot);
        assert_eq!(p50, 50);
        assert_eq!(p95, 95);
        assert_eq!(max, 100);
    }

    #[test]
    fn reload_applies_hot_fields_and_profile() {
        let cfg = Config::default();
        let t = Tunables::new(&cfg, true);
        let mut next = cfg.clone();
        next.index.readers = 4;
        next.index.commit_interval_ms = 6000;
        next.index.cache_mib = 128;
        next.index.adaptive = false;
        let changed = t.reload(&next);
        assert!(changed.contains(&"readers"));
        assert!(changed.contains(&"commit_interval_ms"));
        assert!(changed.contains(&"cache_mib"));
        assert_eq!(t.readers(), 4);
        assert_eq!(t.commit_ms(), 6000);
        assert_eq!(t.cache_mib(), 128);
        assert!(!t.adaptive());

        let mut nvme = cfg.clone();
        nvme.index.profile = "nvme".to_string();
        let changed = t.reload(&nvme);
        assert!(changed.contains(&"profile"));
        assert_eq!(t.profile(), Profile::Nvme);
    }

    #[test]
    fn tunables_respect_hard_bounds() {
        let mut cfg = Config::default();
        cfg.index.readers = 999;
        cfg.index.commit_interval_ms = 1;
        cfg.index.cache_mib = 999_999;
        let t = Tunables::new(&cfg, true);
        assert!(t.readers() >= 1);
        assert!(t.commit_ms() >= 500);
        assert!(t.cache_mib() <= t.max_cache_mib);
        assert!(t.batch_blocks() <= 512);
    }

    #[test]
    fn explicit_profile_and_jobs_are_applied() {
        let mut cfg = Config::default();
        cfg.index.profile = "nvme".to_string();
        cfg.index.background_jobs = 64;
        let t = Tunables::new(&cfg, false);
        assert_eq!(t.profile(), Profile::Nvme);
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        assert!(t.background_jobs() <= cpus.max(1));
    }

    #[test]
    fn explicit_readers_are_not_cpu_capped() {
        let mut cfg = Config::default();
        cfg.index.readers = 6;
        let t = Tunables::new(&cfg, false);
        assert_eq!(t.readers(), 6);
        let _ = t.reload(&cfg);
        assert_eq!(t.readers(), 6);
    }

    #[test]
    fn boundary_commit_does_not_feed_the_controller() {
        let t = Tunables::new(&Config::default(), false);
        let t0 = Instant::now();
        t.record_commit_group(
            t0,
            Duration::from_millis(1_000),
            100,
            10_000,
            100 << 20,
            CommitReason::Blocks,
            100_000,
            800_000,
        );
        let ewma = t.commit_ms();
        let last = t.last_commit().expect("full commit recorded");
        assert_eq!(last.reason, CommitReason::Blocks);
        assert_eq!(last.seq, 1);
        assert_eq!(last.blocks, 100);
        assert_eq!(last.txs, 10_000);
        assert_eq!(last.bytes, 100 << 20);
        assert_eq!(last.cycle_ms, 0, "first full commit has no previous start");

        // A boundary partial commit is 1 ms / 1 block. It must not replace the
        // full commit nor deflate the EWMA the controller reads.
        t.record_commit_group(
            t0 + Duration::from_secs(10),
            Duration::from_millis(1),
            1,
            1,
            1_000,
            CommitReason::Boundary,
            100,
            100,
        );
        assert_eq!(t.commit_ms(), ewma);
        let last = t.last_commit().expect("full commit still recorded");
        assert_eq!(last.reason, CommitReason::Blocks);
        assert_eq!(last.blocks, 100);

        // A tx-limited group is a full commit and is reported as such so the
        // controller can leave the batch alone.
        t.record_commit_group(
            t0 + Duration::from_secs(20),
            Duration::from_millis(2_000),
            75,
            250_000,
            120 << 20,
            CommitReason::Txs,
            100_000,
            1_900_000,
        );
        let last = t.last_commit().expect("tx-limited commit recorded");
        assert_eq!(last.reason, CommitReason::Txs);
        assert_eq!(last.seq, 2);
        assert_eq!(last.cycle_ms, 20_000);
    }

    #[test]
    fn reader_changes_require_two_samples() {
        let t = Tunables::new(&Config::default(), false);
        assert!(!t.reader_streak_ok(1));
        assert!(t.reader_streak_ok(1));
        assert!(!t.reader_streak_ok(-1));
        assert!(t.reader_streak_ok(-1));
        assert!(!t.reader_streak_ok(1), "direction change resets the streak");
    }
}
