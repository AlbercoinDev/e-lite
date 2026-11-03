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
    cache_locked: AtomicBool,
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

    // Exportable performance signals.
    pub read_ms: AtomicU64,
    pub apply_ms: AtomicU64,
    pub commit_ewma: AtomicU64,
    pub tx_per_sec: AtomicU64,
    pub cache_hit_pct: AtomicU64,

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

        let (mut readers, default_commit, default_batch, default_cache, jobs) = match profile {
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

        let max_cache_mib = (ram_mib / 2).clamp(256, 16_384);
        // Keep a single hard ceiling; the profile only changes the starting
        // point. The controller never raises readers on a rotational disk.
        let max_readers = match profile {
            Profile::Hdd => 4,
            Profile::Ssd => 8,
            Profile::Nvme => 16,
            Profile::Auto | Profile::Manual => 32,
        };
        let readers = readers.clamp(1, max_readers);
        let cache_mib = cache_mib.clamp(64, max_cache_mib);
        let commit_ms = commit_ms.clamp(500, 10_000);
        let batch_blocks = batch_blocks.clamp(16, 2048);

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
            cache_locked: AtomicBool::new(cfg.index.cache_mib > 0),
            commit_locked: AtomicBool::new(cfg.index.commit_interval_ms > 0),
            batch_locked: AtomicBool::new(
                cfg.index.batch_blocks > 0 && cfg.index.batch_blocks != 256,
            ),
            profile: AtomicU8::new(profile.code()),
            rotational: AtomicBool::new(rotational),
            background_jobs: AtomicUsize::new(jobs.clamp(1, 32)),
            min_readers: 1,
            min_commit_ms: 500,
            max_commit_ms: 10_000,
            min_cache_mib: 64,
            max_cache_mib,
            min_batch: 16,
            max_batch: 2048,
            read_ms: AtomicU64::new(0),
            apply_ms: AtomicU64::new(0),
            commit_ewma: AtomicU64::new(0),
            tx_per_sec: AtomicU64::new(0),
            cache_hit_pct: AtomicU64::new(0),
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

    fn profile_max_readers(&self) -> usize {
        match self.profile() {
            Profile::Hdd => 4,
            Profile::Ssd => 8,
            Profile::Nvme => 16,
            Profile::Auto | Profile::Manual => 32,
        }
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
    pub fn record_commit(&self, d: Duration) {
        ewma(&self.commit_ewma, d.as_millis() as u64);
    }
    pub fn record_tx_rate(&self, tx_per_sec: u64) {
        ewma(&self.tx_per_sec, tx_per_sec);
    }
    pub fn record_cache_hit_pct(&self, pct: u64) {
        ewma(&self.cache_hit_pct, pct);
    }

    fn apply_profile_defaults(&self, profile: Profile) {
        match profile {
            Profile::Hdd => {
                self.readers.store(2, Ordering::Relaxed);
                self.commit_ms.store(5_000, Ordering::Relaxed);
                self.batch_blocks.store(512, Ordering::Relaxed);
                self.max_group_txs.store(500_000, Ordering::Relaxed);
                self.background_jobs.store(4, Ordering::Relaxed);
            }
            Profile::Ssd => {
                self.readers.store(8, Ordering::Relaxed);
                self.commit_ms.store(2_000, Ordering::Relaxed);
                self.batch_blocks.store(256, Ordering::Relaxed);
                self.max_group_txs.store(250_000, Ordering::Relaxed);
                self.background_jobs.store(8, Ordering::Relaxed);
            }
            Profile::Nvme => {
                self.readers.store(16, Ordering::Relaxed);
                self.commit_ms.store(2_000, Ordering::Relaxed);
                self.batch_blocks.store(256, Ordering::Relaxed);
                self.max_group_txs.store(250_000, Ordering::Relaxed);
                self.background_jobs.store(12, Ordering::Relaxed);
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
        self.cache_locked
            .store(cfg.index.cache_mib > 0, Ordering::Relaxed);
        self.commit_locked
            .store(cfg.index.commit_interval_ms > 0, Ordering::Relaxed);
        self.batch_locked.store(
            cfg.index.batch_blocks > 0 && cfg.index.batch_blocks != 256,
            Ordering::Relaxed,
        );
        if cfg.index.readers > 0 {
            let v = cfg
                .index
                .readers
                .clamp(self.min_readers, self.profile_max_readers());
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
        let mem_available = mem_available_mib();
        if mem_available < 512 {
            // Cache first: it is reclaimable and usually the cheapest thing to
            // shrink. Do not collapse the indexer to one reader on a temporary
            // dip; keep a floor unless memory is truly critical.
            let mut c = self.cache_mib();
            if !self.cache_locked.load(Ordering::Relaxed) {
                c = (c * 3 / 4).max(128);
            }
            let mut r = self.readers();
            if mem_available < 256 {
                r = (r * 3 / 4).max(2).min(self.readers());
            }
            let changed = c != self.cache_mib() || r != self.readers();
            if c != self.cache_mib() {
                self.cache_mib.store(c, Ordering::Relaxed);
                store.set_cache_bytes(c as usize * 1024 * 1024);
            }
            if r != self.readers() {
                self.readers.store(r, Ordering::Relaxed);
            }
            if changed {
                eprintln!(
                    "[e-lite] adaptive tuning: memory pressure ({mem_available} MiB):                      readers -> {r}, cache -> {c} MiB"
                );
                if let Some(stats) = stats {
                    self.publish(stats);
                }
                return Some("memory pressure".to_string());
            }
        }
        if let Some(pct) = store.cache_hit_pct() {
            self.record_cache_hit_pct(pct);
        }

        let mut action = None;

        // Commit/fsync pain: amortise with larger groups on slow storage.
        if !self.commit_locked.load(Ordering::Relaxed)
            && commit_ms > 500
            && self.commit_ms() < self.max_commit_ms
        {
            let v = (self.commit_ms() * 3 / 2).min(self.max_commit_ms);
            self.commit_ms.store(v, Ordering::Relaxed);
            let b = (self.batch_blocks() * 3 / 2).min(self.max_batch);
            self.batch_blocks.store(b, Ordering::Relaxed);
            action = Some(format!(
                "commit {commit_ms}ms: interval -> {v}ms, batch -> {b}"
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
        let max_readers = self.profile_max_readers().min(self.memory_reader_cap());
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

        // Cache misses and memory headroom.
        if action.is_none() && !self.cache_locked.load(Ordering::Relaxed) {
            let hit = self.cache_hit_pct.load(Ordering::Relaxed);
            if hit > 0 && hit < 60 && mem_available > 1024 && self.cache_mib() < self.max_cache_mib
            {
                let v = (self.cache_mib() * 5 / 4).min(self.max_cache_mib);
                self.cache_mib.store(v, Ordering::Relaxed);
                store.set_cache_bytes(v.saturating_mul(1024 * 1024) as usize);
                action = Some(format!("cache hit {hit}%: cache -> {v} MiB"));
            } else if mem_available < 512 && self.cache_mib() > self.min_cache_mib {
                let v = (self.cache_mib() * 3 / 4).max(self.min_cache_mib);
                self.cache_mib.store(v, Ordering::Relaxed);
                store.set_cache_bytes(v.saturating_mul(1024 * 1024) as usize);
                action = Some(format!("low RAM: cache -> {v} MiB"));
            }
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
            .tuning_commit_ms_ewma
            .store(self.commit_ewma.load(Ordering::Relaxed), Ordering::Relaxed);
        stats
            .tuning_tx_per_sec
            .store(self.tx_per_sec.load(Ordering::Relaxed), Ordering::Relaxed);
        stats.tuning_cache_hit_pct.store(
            self.cache_hit_pct.load(Ordering::Relaxed),
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

pub fn total_ram_mib() -> Option<u64> {
    let mem = meminfo_mib("MemTotal");
    let cg = cgroup_limit_mib();
    match (mem, cg) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

pub fn mem_available_mib() -> u64 {
    let mem = meminfo_mib("MemAvailable").unwrap_or(0);
    let cg = cgroup_available_mib().unwrap_or(u64::MAX);
    mem.min(cg)
}

fn meminfo_mib(key: &str) -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix(key))
        .and_then(|l| l.split_whitespace().next())
        .and_then(|v| v.parse::<u64>().ok())
        .map(|kb| kb / 1024)
}

fn read_u64(path: &str) -> Option<u64> {
    std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
}

/// cgroup v2 or v1 memory limit in MiB. `max`/unlimited yields None.
fn cgroup_limit_mib() -> Option<u64> {
    if let Some(v) = read_u64("/sys/fs/cgroup/memory.max") {
        return Some(v / 1024 / 1024);
    }
    let v = read_u64("/sys/fs/cgroup/memory/memory.limit_in_bytes")?;
    // v1 uses a huge sentinel for unlimited.
    if v >= (1u64 << 60) {
        None
    } else {
        Some(v / 1024 / 1024)
    }
}

fn cgroup_available_mib() -> Option<u64> {
    let limit = cgroup_limit_mib()?;
    let used = read_u64("/sys/fs/cgroup/memory.current")
        .or_else(|| read_u64("/sys/fs/cgroup/memory/memory.usage_in_bytes"))?
        / 1024
        / 1024;
    Some(limit.saturating_sub(used))
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
}
