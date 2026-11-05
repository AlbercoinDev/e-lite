//! Runtime tuning: startup profiles, hot-reloadable knobs and a small
//! event-driven controller.
//!
//! The goal is to keep one binary usable on a slow HDD and on a fast NVMe
//! without rebuilding or restarting. The controller is deliberately
//! conservative: one change per cooldown, hysteresis, hard bounds, and the
//! disk/prune guards always win.

use crate::config::Config;
use el_serve::Stats;
use el_store::Store;
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

/// Thread-safe hot knobs. Cold settings (`prune_mib`, block source, schema...)
/// stay in `Config` and require a restart.
pub struct Tunables {
    readers: AtomicUsize,
    commit_ms: AtomicU64,
    batch_blocks: AtomicU32,
    max_group_txs: AtomicUsize,
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
    min_commit_ms: u64,
    max_commit_ms: u64,
    min_cache_mib: u64,
    max_cache_mib: u64,
    min_batch: u32,
    max_batch: u32,

    // Memory split computed at startup.
    total_mib: u64,
    reserve_mib: u64,
    ceiling_mib: u64,
    memtables_mib: u64,
    runtime_mib: u64,
    mem_streak: Mutex<(i8, u32)>,
    resume_shrunk: AtomicBool,

    // Exportable performance signals.
    pub read_ms: AtomicU64,
    pub apply_ms: AtomicU64,
    pub decode_ms: AtomicU64,
    pub wait_ms: AtomicU64,
    pub prefetch_ms: AtomicU64,
    pub commit_ewma: AtomicU64,
    pub tx_per_sec: AtomicU64,
    pub blocks_per_sec: AtomicU64,

    last_change: Mutex<Instant>,
}

impl Tunables {
    pub fn new(cfg: &Config, rotational: bool) -> Self {
        let requested = Profile::parse(&cfg.index.profile).unwrap_or_else(|| {
            eprintln!(
                "[e-lite] unknown [index] profile {:?}; using auto",
                cfg.index.profile
            );
            Profile::Auto
        });
        let profile = requested.resolve(rotational);
        let ram_mib = total_ram_mib().unwrap_or(1024);
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);

        let (mut readers, default_commit, default_batch, default_cache, profile_jobs) =
            match profile {
                Profile::Hdd => (2usize, 5_000u64, 512u32, (ram_mib / 4).min(4096), 4usize),
                Profile::Ssd => (8, 2_000, 256, (ram_mib / 4).min(8192), cpus.min(8)),
                Profile::Nvme => (16, 2_000, 256, (ram_mib / 3).min(16_384), cpus.min(12)),
                Profile::Manual | Profile::Auto => (
                    if cfg.index.readers > 0 {
                        cfg.index.readers
                    } else {
                        cpus.min(8)
                    },
                    2_000,
                    256,
                    if cfg.index.cache_mib > 0 {
                        cfg.index.cache_mib
                    } else {
                        (ram_mib / 4).clamp(64, 2048)
                    },
                    cpus.min(4),
                ),
            };
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
        let cache_mib = if cfg.index.cache_mib > 0 {
            cfg.index.cache_mib
        } else {
            default_cache
        };

        let budget = budget_from(
            cfg,
            ram_mib,
            memory_info().available_mib,
            memory_info().cached_mib,
        );
        let max_cache_mib = budget.cache_max_mib.max(64);
        // Keep a single hard ceiling; the profile only changes the starting
        // point. The controller never raises readers on a rotational disk.
        let max_readers = match profile {
            Profile::Hdd => 4,
            Profile::Ssd => 8,
            Profile::Nvme => 16,
            Profile::Auto | Profile::Manual => 32,
        };
        let cpu_reader_cap = cpus.saturating_mul(2).max(2);
        let readers = if cfg.index.readers > 0 {
            readers.clamp(1, max_readers)
        } else {
            readers.clamp(1, max_readers).min(cpu_reader_cap)
        };
        let cache_mib = cache_mib.clamp(64, max_cache_mib);
        let commit_ms = commit_ms.clamp(500, 30_000);
        let batch_blocks = batch_blocks.clamp(16, 4096);

        Tunables {
            readers: AtomicUsize::new(readers),
            commit_ms: AtomicU64::new(commit_ms),
            batch_blocks: AtomicU32::new(batch_blocks),
            max_group_txs: AtomicUsize::new(if profile == Profile::Hdd {
                500_000
            } else {
                250_000
            }),
            cache_mib: AtomicU64::new(cache_mib),
            prefetch_blocks: AtomicUsize::new(cfg.index.prefetch_blocks.max(1)),
            adaptive: AtomicBool::new(cfg.index.adaptive),
            readers_locked: AtomicBool::new(cfg.index.readers > 0),
            commit_locked: AtomicBool::new(cfg.index.commit_interval_ms > 0),
            batch_locked: AtomicBool::new(
                cfg.index.batch_blocks > 0 && cfg.index.batch_blocks != 256,
            ),
            profile: AtomicU8::new(profile.code()),
            rotational: AtomicBool::new(rotational),
            background_jobs: AtomicUsize::new(jobs.clamp(1, 32)),
            min_readers: 1,
            min_commit_ms: 500,
            max_commit_ms: 30_000,
            min_cache_mib: 64,
            max_cache_mib,
            min_batch: 16,
            max_batch: 4096,
            total_mib: ram_mib,
            reserve_mib: budget.reserve_mib,
            ceiling_mib: budget.ceiling_mib,
            memtables_mib: budget.memtables_mib,
            runtime_mib: budget.runtime_mib,
            mem_streak: Mutex::new((0, 0)),
            resume_shrunk: AtomicBool::new(false),
            read_ms: AtomicU64::new(0),
            apply_ms: AtomicU64::new(0),
            decode_ms: AtomicU64::new(0),
            wait_ms: AtomicU64::new(0),
            prefetch_ms: AtomicU64::new(0),
            commit_ewma: AtomicU64::new(0),
            tx_per_sec: AtomicU64::new(0),
            blocks_per_sec: AtomicU64::new(0),
            last_change: Mutex::new(
                Instant::now()
                    .checked_sub(Duration::from_secs(120))
                    .unwrap_or_else(Instant::now),
            ),
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
    pub fn batch_blocks(&self) -> u32 {
        self.batch_blocks.load(Ordering::Relaxed)
    }
    pub fn max_group_txs(&self) -> usize {
        self.max_group_txs.load(Ordering::Relaxed)
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

    fn profile_ceiling(&self) -> usize {
        match self.profile() {
            Profile::Hdd => 4,
            Profile::Ssd => 8,
            Profile::Nvme => 16,
            Profile::Auto | Profile::Manual => 32,
        }
    }

    fn auto_max_readers(&self) -> usize {
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        self.profile_ceiling().min(cpus.saturating_mul(2).max(2))
    }

    fn memory_reader_cap(&self) -> usize {
        // Leave memory for Core, RocksDB and the OS. 128 MiB per worker is a
        // conservative allowance for a block buffer plus queue slack.
        let avail = mem_available_mib().max(256);
        ((avail / 128) as usize).max(1)
    }

    pub fn record_read(&self, d: Duration) {
        ewma(&self.read_ms, d.as_millis() as u64);
    }
    pub fn record_apply(&self, d: Duration) {
        ewma(&self.apply_ms, d.as_millis() as u64);
    }
    pub fn record_decode(&self, d: Duration) {
        ewma(&self.decode_ms, d.as_millis() as u64);
    }
    pub fn record_wait(&self, d: Duration) {
        ewma(&self.wait_ms, d.as_millis() as u64);
    }
    pub fn record_prefetch(&self, d: Duration) {
        ewma(&self.prefetch_ms, d.as_millis() as u64);
    }
    pub fn record_commit(&self, d: Duration) {
        ewma(&self.commit_ewma, d.as_millis() as u64);
    }
    pub fn record_tx_rate(&self, tx_per_sec: u64) {
        ewma(&self.tx_per_sec, tx_per_sec);
    }
    pub fn record_blocks_rate(&self, blocks_per_sec: u64) {
        ewma(&self.blocks_per_sec, blocks_per_sec);
    }

    fn apply_profile_defaults(&self, profile: Profile) {
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        let reader_cap = cpus.saturating_mul(2).max(2);
        match profile {
            Profile::Hdd => {
                self.readers
                    .store(2usize.min(reader_cap), Ordering::Relaxed);
                self.commit_ms.store(5_000, Ordering::Relaxed);
                self.batch_blocks.store(512, Ordering::Relaxed);
                self.max_group_txs.store(500_000, Ordering::Relaxed);
                self.background_jobs
                    .store(4usize.min(cpus.max(1)), Ordering::Relaxed);
            }
            Profile::Ssd => {
                self.readers
                    .store(8usize.min(reader_cap), Ordering::Relaxed);
                self.commit_ms.store(2_000, Ordering::Relaxed);
                self.batch_blocks.store(256, Ordering::Relaxed);
                self.max_group_txs.store(250_000, Ordering::Relaxed);
                self.background_jobs
                    .store(8usize.min(cpus.max(1)), Ordering::Relaxed);
            }
            Profile::Nvme => {
                self.readers
                    .store(16usize.min(reader_cap), Ordering::Relaxed);
                self.commit_ms.store(2_000, Ordering::Relaxed);
                self.batch_blocks.store(256, Ordering::Relaxed);
                self.max_group_txs.store(250_000, Ordering::Relaxed);
                self.background_jobs
                    .store(12usize.min(cpus.max(1)), Ordering::Relaxed);
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
        if cfg.index.readers > 0 {
            let v = cfg
                .index
                .readers
                .clamp(self.min_readers, self.profile_ceiling());
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
                changed.push("commit_interval_ms");
            }
        }
        if cfg.index.batch_blocks > 0 {
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

        // Commit/fsync pain: amortise with larger groups on slow storage. Avoid
        // growing the group when commit already dominates the interval; in that
        // case shrink the group to reduce the per-commit stall.
        let commit_share = commit_ms as f64 / self.commit_ms().max(1) as f64;
        if !self.commit_locked.load(Ordering::Relaxed)
            && commit_ms > 500
            && commit_share < 0.5
            && self.commit_ms() < self.max_commit_ms
        {
            let v = (self.commit_ms() * 3 / 2).min(self.max_commit_ms);
            self.commit_ms.store(v, Ordering::Relaxed);
            let b = (self.batch_blocks() * 3 / 2).min(self.max_batch);
            self.batch_blocks.store(b, Ordering::Relaxed);
            action = Some(format!(
                "commit {commit_ms}ms ({:.0}% of interval): interval -> {v}ms, batch -> {b}",
                commit_share * 100.0
            ));
        } else if !self.commit_locked.load(Ordering::Relaxed)
            && commit_share > 0.75
            && self.batch_blocks() > self.min_batch.saturating_mul(4)
        {
            let b = (self.batch_blocks() * 3 / 4).max(self.min_batch);
            self.batch_blocks.store(b, Ordering::Relaxed);
            action = Some(format!(
                "commit {commit_ms}ms ({:.0}% of interval): batch -> {b}",
                commit_share * 100.0
            ));
        } else if !self.commit_locked.load(Ordering::Relaxed)
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
        if action.is_none()
            && !self.readers_locked.load(Ordering::Relaxed)
            && read_ms > 200
            && apply_ms < read_ms
        {
            let v = (self.readers() * 3 / 4).max(self.min_readers);
            if v != self.readers() {
                self.readers.store(v, Ordering::Relaxed);
                action = Some(format!("read latency {read_ms}ms: readers -> {v}"));
            }
        } else if action.is_none()
            && !self.readers_locked.load(Ordering::Relaxed)
            && mode == Mode::Bulk
            && !self.rotational()
            && read_ms > 120
            && apply_ms < read_ms
            && self.readers() < max_readers
        {
            let v = (self.readers() + 1).min(max_readers);
            self.readers.store(v, Ordering::Relaxed);
            action = Some(format!("read latency {read_ms}ms: readers -> {v}"));
        }

        if let Some(reason) = &action {
            if let Ok(mut last) = self.last_change.lock() {
                *last = Instant::now();
            }
            eprintln!("[e-lite] adaptive tuning: {reason}");
            if let Some(stats) = stats {
                self.publish(stats);
            }
        } else if let Some(stats) = stats {
            self.publish(stats);
        }
        action
    }

    /// Hot memory policy: grow the block cache in small steps while page cache
    /// and available RAM stay healthy, and shrink under pressure. Manual
    /// `cache_mib` is treated as a maximum, not as an immutable value.
    fn memory_step(&self, store: &Store, stats: Option<&Stats>) -> Option<String> {
        let mem = memory_info();
        let action = cache_action(
            mem.available_mib,
            mem.cached_mib,
            self.reserve_mib,
            self.cache_mib(),
            self.max_cache_mib,
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
        let old = self.cache_mib();
        let new = match action {
            CacheAction::Grow => (old + 256).min(self.max_cache_mib),
            CacheAction::Shrink => {
                let floor = 256u64.min(old.max(1));
                (old * 3 / 4).max(floor)
            }
            CacheAction::Hold => old,
        };
        if new == old {
            return None;
        }
        self.cache_mib.store(new, Ordering::Relaxed);
        store.set_cache_bytes(new as usize * 1024 * 1024);
        // Memory is truly critical: also give back a reader.
        let mut readers = self.readers();
        if mem.available_mib < 256 && readers > self.min_readers {
            readers = (readers * 3 / 4).max(self.min_readers);
            self.readers.store(readers, Ordering::Relaxed);
        }
        eprintln!(
            "[e-lite] memory: available {} MiB, buff/cache {} MiB, reserve {} MiB: \
             cache {} -> {} MiB (max {}, memtables {} MiB, readers {})",
            mem.available_mib,
            mem.cached_mib,
            self.reserve_mib,
            old,
            new,
            self.max_cache_mib,
            self.memtables_mib,
            readers
        );
        if let Some(stats) = stats {
            self.publish(stats);
        }
        Some(format!("memory: cache {} -> {} MiB", old, new))
    }

    /// Give RAM back before Core resumes downloads. Called once per bulk pause:
    /// Core's block index and verification grow after this point.
    pub fn shrink_for_core_resume(&self, store: &Store) {
        if self.resume_shrunk.swap(true, Ordering::SeqCst) {
            return;
        }
        let live = (self.total_mib / 4)
            .max(1024)
            .min(self.max_cache_mib)
            .max(self.min_cache_mib);
        let old = self.cache_mib();
        if old <= live {
            return;
        }
        self.cache_mib.store(live, Ordering::Relaxed);
        store.set_cache_bytes(live as usize * 1024 * 1024);
        eprintln!(
            "[e-lite] memory: Core is about to resume; cache {} -> {} MiB \
             (available {}, buff/cache {})",
            old,
            live,
            memory_info().available_mib,
            memory_info().cached_mib
        );
    }

    /// Arm the one-shot pre-resume shrink again after a new bulk pause.
    pub fn reset_resume_shrink(&self) {
        self.resume_shrunk.store(false, Ordering::SeqCst);
    }

    pub fn memory_numbers(&self) -> (u64, u64, u64, u64, u64) {
        (
            self.total_mib,
            self.reserve_mib,
            self.ceiling_mib,
            self.memtables_mib,
            self.runtime_mib,
        )
    }

    /// Export current hot values and metrics to the shared status.
    pub fn publish(&self, stats: &Stats) {
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
        stats
            .tuning_commit_ms_ewma
            .store(self.commit_ewma.load(Ordering::Relaxed), Ordering::Relaxed);
        stats
            .tuning_tx_per_sec
            .store(self.tx_per_sec.load(Ordering::Relaxed), Ordering::Relaxed);
        stats.tuning_blocks_per_sec.store(
            self.blocks_per_sec.load(Ordering::Relaxed),
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
        stats
            .mem_memtables_mib
            .store(self.memtables_mib, Ordering::Relaxed);
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

/// Memory figures used for cache/reader decisions and diagnostics.
#[derive(Debug, Clone, Copy, Default)]
pub struct MemoryInfo {
    pub total_mib: Option<u64>,
    /// Memory that can actually be used before reclaim/OOM.
    pub available_mib: u64,
    /// `/proc/meminfo` value before applying a cgroup limit.
    pub host_available_mib: u64,
    pub cgroup_limit_mib: Option<u64>,
    pub cgroup_used_mib: Option<u64>,
    /// Page cache in the cgroup that the kernel can reclaim under pressure.
    pub reclaimable_mib: Option<u64>,
    /// `MemFree`, useful for diagnostics.
    pub free_mib: u64,
    /// `Buffers + Cached`: page cache that a growing anonymous cache evicts.
    pub cached_mib: u64,
}

pub fn total_ram_mib() -> Option<u64> {
    memory_info().total_mib
}

pub fn mem_available_mib() -> u64 {
    memory_info().available_mib
}

pub fn memory_info() -> MemoryInfo {
    let host_total = meminfo_mib("MemTotal");
    let cg_limit = cgroup_limit_mib();
    let total = match (host_total, cg_limit) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };
    let host_available = meminfo_mib("MemAvailable")
        .or_else(|| meminfo_mib("MemFree"))
        .unwrap_or(0);
    let cg_used = cgroup_used_mib();
    let reclaimable = cgroup_reclaimable_mib();
    let cg_available = cgroup_available_mib();
    let available = match cg_available {
        Some(cg) => host_available.min(cg),
        None => host_available,
    };
    let free = meminfo_mib("MemFree").unwrap_or(0);
    let cached = meminfo_mib("Buffers")
        .unwrap_or(0)
        .saturating_add(meminfo_mib("Cached").unwrap_or(0));
    MemoryInfo {
        total_mib: total,
        available_mib: available,
        host_available_mib: host_available,
        cgroup_limit_mib: cg_limit,
        cgroup_used_mib: cg_used,
        reclaimable_mib: reclaimable,
        free_mib: free,
        cached_mib: cached,
    }
}

/// Parse a `/proc/meminfo` line such as `MemAvailable:    7677888 kB`.
/// The colon is part of the format; leaving it in the value is what made
/// older versions report 0 MiB available on every machine.
fn meminfo_from(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let (name, rest) = line.split_once(':')?;
        if name.trim() != key {
            return None;
        }
        rest.split_whitespace()
            .next()?
            .parse::<u64>()
            .ok()
            .map(|kb| kb / 1024)
    })
}

fn meminfo_mib(key: &str) -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    meminfo_from(&text, key)
}

fn read_trim(path: &str) -> Option<String> {
    Some(std::fs::read_to_string(path).ok()?.trim().to_string())
}

fn parse_mib(value: &str) -> Option<u64> {
    value.parse::<u64>().ok().map(|v| v / 1024 / 1024)
}

fn cgroup_file(name: &str) -> Option<String> {
    read_trim(&format!("/sys/fs/cgroup/{name}"))
        .or_else(|| read_trim(&format!("/sys/fs/cgroup/memory/{name}")))
}

/// cgroup v2 or v1 memory limit in MiB. `max`/unlimited yields None.
fn cgroup_limit_mib() -> Option<u64> {
    if let Some(raw) = read_trim("/sys/fs/cgroup/memory.max") {
        if raw != "max" {
            return parse_mib(&raw);
        }
        return None;
    }
    let v = read_trim("/sys/fs/cgroup/memory/memory.limit_in_bytes")?
        .parse::<u64>()
        .ok()?;
    // v1 uses a huge sentinel for unlimited.
    if v >= (1u64 << 60) {
        None
    } else {
        Some(v / 1024 / 1024)
    }
}

fn cgroup_used_mib() -> Option<u64> {
    cgroup_file("memory.current")
        .and_then(|v| parse_mib(&v))
        .or_else(|| cgroup_file("memory.usage_in_bytes").and_then(|v| parse_mib(&v)))
}

/// Reclaimable page cache from `memory.stat`. `memory.current` includes this
/// cache, so subtracting it avoids false "0 MiB available" pressure on hosts
/// with a cgroup limit.
fn cgroup_reclaimable_mib() -> Option<u64> {
    let text = std::fs::read_to_string("/sys/fs/cgroup/memory.stat")
        .or_else(|_| std::fs::read_to_string("/sys/fs/cgroup/memory/memory.stat"))
        .ok()?;
    cgroup_stat_from(&text, "inactive_file")
}

fn cgroup_stat_from(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let (name, value) = line.split_once(' ')?;
        if name.trim() != key {
            return None;
        }
        value
            .trim()
            .parse::<u64>()
            .ok()
            .map(|bytes| bytes / 1024 / 1024)
    })
}

fn cgroup_available_mib() -> Option<u64> {
    let limit = cgroup_limit_mib()?;
    let used = cgroup_used_mib()?;
    let reclaimable = cgroup_reclaimable_mib().unwrap_or(0);
    Some(limit.saturating_sub(used.saturating_sub(reclaimable)))
}

/// Memory split for the indexer. Kept as a pure calculation so it can be
/// tested without touching the host.
#[derive(Debug, Clone, Copy)]
pub struct MemoryBudget {
    pub total_mib: u64,
    pub available_mib: u64,
    pub cached_mib: u64,
    pub reserve_mib: u64,
    pub ceiling_mib: u64,
    pub memtables_mib: u64,
    pub runtime_mib: u64,
    pub cache_max_mib: u64,
    pub cache_start_mib: u64,
}

pub fn memory_budget(cfg: &Config) -> MemoryBudget {
    let mem = memory_info();
    budget_from(
        cfg,
        mem.total_mib.unwrap_or(1024),
        mem.available_mib,
        mem.cached_mib,
    )
}

pub fn budget_from(
    cfg: &Config,
    total_mib: u64,
    available_mib: u64,
    cached_mib: u64,
) -> MemoryBudget {
    let reserve_mib = if cfg.index.memory_reserve_mib > 0 {
        cfg.index.memory_reserve_mib
    } else {
        (total_mib / 8).max(1024)
    };
    let fraction = if cfg.index.memory_fraction > 0.0 {
        cfg.index.memory_fraction.clamp(0.1, 1.0)
    } else {
        0.75
    };
    let ceiling_mib = ((total_mib as f64 * fraction) as u64)
        .max(reserve_mib.saturating_add(256))
        .min(total_mib);
    let memtables_mib = if cfg.index.write_buffer_mib > 0 {
        cfg.index.write_buffer_mib
    } else {
        (total_mib / 8).clamp(256, 2048)
    };
    let runtime_mib = (total_mib / 16).clamp(256, 1024);
    let cache_ceiling = ceiling_mib
        .saturating_sub(memtables_mib.saturating_add(runtime_mib))
        .max(256);
    let cache_max_mib = if cfg.index.cache_mib > 0 {
        cfg.index.cache_mib.min(cache_ceiling)
    } else {
        cache_ceiling
    };
    let cache_start_mib = if cfg.index.cache_mib > 0 {
        cache_max_mib
    } else {
        (total_mib / 4).clamp(256, cache_max_mib)
    };
    MemoryBudget {
        total_mib,
        available_mib,
        cached_mib,
        reserve_mib,
        ceiling_mib,
        memtables_mib,
        runtime_mib,
        cache_max_mib,
        cache_start_mib,
    }
}

pub fn auto_memtables_mib(cfg: &Config) -> u64 {
    memory_budget(cfg).memtables_mib
}

pub fn initial_cache_mib(cfg: &Config) -> u64 {
    memory_budget(cfg).cache_start_mib
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheAction {
    Grow,
    Hold,
    Shrink,
}

/// One sample of the hot memory policy. `cached_mib` is page cache: growing the
/// RocksDB cache evicts it, so it is the guard that keeps block reads fast.
pub fn cache_action(
    available_mib: u64,
    cached_mib: u64,
    reserve_mib: u64,
    cache_mib: u64,
    cache_max_mib: u64,
) -> CacheAction {
    if available_mib < reserve_mib || cached_mib < reserve_mib / 2 {
        CacheAction::Shrink
    } else if available_mib >= reserve_mib + reserve_mib / 2
        && cached_mib >= reserve_mib
        && cache_mib < cache_max_mib
    {
        CacheAction::Grow
    } else {
        CacheAction::Hold
    }
}

/// `rotational` hint for the device holding `path`. Falls back to false.
/// Uses mountinfo's major:minor and walks sysfs to the parent block device, so
/// partitions and device-mapper volumes are handled too.
pub fn is_rotational(path: &std::path::Path) -> bool {
    let Ok(info) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };
    let mut best: Option<(usize, String)> = None;
    for line in info.lines() {
        let Some((pre, _post)) = line.split_once(" - ") else {
            continue;
        };
        let fields: Vec<&str> = pre.split_whitespace().collect();
        if fields.len() < 5 {
            continue;
        }
        let mount = fields[4];
        if path.starts_with(mount) && best.as_ref().map(|(n, _)| mount.len() > *n).unwrap_or(true) {
            best = Some((mount.len(), fields[2].to_string()));
        }
    }
    let Some((_, dev)) = best else {
        return false;
    };
    let Ok(link) = std::fs::canonicalize(format!("/sys/dev/block/{dev}")) else {
        return false;
    };
    for dir in link.ancestors() {
        if let Ok(v) = std::fs::read_to_string(dir.join("queue/rotational")) {
            return v.trim() == "1";
        }
    }
    false
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
    fn meminfo_parser_reads_free_and_page_cache() {
        let text =
            "MemTotal: 8131584 kB\nMemFree: 163840 kB\nBuffers: 10240 kB\nCached: 2772992 kB\n";
        assert_eq!(meminfo_from(text, "MemFree"), Some(160));
        assert_eq!(meminfo_from(text, "Buffers"), Some(10));
        assert_eq!(meminfo_from(text, "Cached"), Some(2708));
    }

    #[test]
    fn memory_budget_scales_with_ram_and_respects_overrides() {
        let mut cfg = Config::default();
        let b = budget_from(&cfg, 7941, 2583, 2708);
        assert_eq!(b.reserve_mib, 1024);
        assert_eq!(b.ceiling_mib, 5955);
        assert_eq!(b.memtables_mib, 992);
        assert!(b.cache_max_mib > b.cache_start_mib || b.cache_start_mib == b.cache_max_mib);

        cfg.index.memory_fraction = 0.5;
        cfg.index.memory_reserve_mib = 2048;
        cfg.index.write_buffer_mib = 512;
        let b = budget_from(&cfg, 8192, 3000, 1000);
        assert_eq!(b.reserve_mib, 2048);
        assert_eq!(b.ceiling_mib, 4096);
        assert_eq!(b.memtables_mib, 512);

        // Manual cache is a maximum, not a fixed value.
        cfg.index.cache_mib = 128;
        let b = budget_from(&cfg, 8192, 3000, 1000);
        assert_eq!(b.cache_max_mib, 128);
        assert_eq!(b.cache_start_mib, 128);
    }

    #[test]
    fn cache_action_grows_and_shrinks_with_hysteresis_inputs() {
        // Healthy: available > 1.5*reserve and page cache >= reserve.
        assert_eq!(
            cache_action(2583, 2708, 1024, 1985, 4000),
            CacheAction::Grow
        );
        // No room above the ceiling: hold.
        assert_eq!(
            cache_action(2583, 2708, 1024, 4000, 4000),
            CacheAction::Hold
        );
        // Available below reserve: shrink.
        assert_eq!(
            cache_action(800, 2708, 1024, 2000, 4000),
            CacheAction::Shrink
        );
        // Page cache below half the reserve: shrink even with free RAM.
        assert_eq!(
            cache_action(4000, 400, 1024, 2000, 4000),
            CacheAction::Shrink
        );
        // Between 1x and 1.5x reserve: hold.
        assert_eq!(
            cache_action(1200, 2000, 1024, 2000, 4000),
            CacheAction::Hold
        );
    }

    #[test]
    fn meminfo_parser_handles_the_colon() {
        let text = "MemTotal:        8131584 kB\nMemAvailable:    7677888 kB\nMemFree:  2048 kB\n";
        assert_eq!(meminfo_from(text, "MemTotal"), Some(7941));
        assert_eq!(meminfo_from(text, "MemAvailable"), Some(7497));
        assert_eq!(meminfo_from(text, "MemFree"), Some(2));
        assert_eq!(meminfo_from(text, "Missing"), None);
    }

    #[test]
    fn cgroup_stat_parser_reads_reclaimable_cache() {
        let text =
            "anon 35291136\nfile 5722951680\ninactive_file 1931116544\nactive_file 3787128832\n";
        assert_eq!(cgroup_stat_from(text, "inactive_file"), Some(1841));
        assert_eq!(cgroup_stat_from(text, "active_file"), Some(3611));
        assert_eq!(cgroup_stat_from(text, "missing"), None);
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
}
