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

// ------------------------------------------------------- machine detection

/// CPU budget derived from physical cores, affinity and the cgroup quota.
/// Logical SMT threads are not treated as independent cores: a 4C/8T laptop
/// CPU must not get `-par=7` and eight index readers.
#[derive(Debug, Clone)]
pub struct CpuInfo {
    pub physical: usize,
    pub logical: usize,
    /// Logical CPUs after affinity and cgroup quota.
    pub effective: usize,
    /// Physical cores after the quota, never more than `effective`.
    pub physical_effective: usize,
    pub smt: bool,
    pub quota_cpus: Option<f64>,
    pub source: String,
}

fn physical_core_pairs() -> HashSet<(String, String)> {
    let Ok(text) = std::fs::read_to_string("/proc/cpuinfo") else {
        return HashSet::new();
    };
    let mut pairs = HashSet::new();
    let mut phys: Option<String> = None;
    let mut core: Option<String> = None;
    for line in text.lines() {
        if line.trim().is_empty() {
            if let (Some(p), Some(c)) = (phys.take(), core.take()) {
                pairs.insert((p, c));
            }
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            match k.trim() {
                "physical id" => phys = Some(v.trim().to_string()),
                "core id" => core = Some(v.trim().to_string()),
                _ => {}
            }
        }
    }
    if let (Some(p), Some(c)) = (phys, core) {
        pairs.insert((p, c));
    }
    pairs
}

fn parse_cpu_max(text: &str) -> Option<f64> {
    let mut it = text.split_whitespace();
    let quota = it.next()?;
    if quota == "max" {
        return None;
    }
    let quota: f64 = quota.parse().ok()?;
    let period: f64 = it.next()?.parse().ok()?;
    if quota <= 0.0 || period <= 0.0 {
        return None;
    }
    Some(quota / period)
}

fn cpu_quota_cpus() -> Option<f64> {
    let text = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let mut best: Option<f64> = None;
    let mut consider = |v: f64| {
        best = Some(best.map_or(v, |b| b.min(v)));
    };
    // cgroup v2: one unified hierarchy.
    if let Some(path) = text.lines().find_map(|l| l.strip_prefix("0::")) {
        let rel = path.trim_start_matches('/');
        let mut cur = PathBuf::from("/sys/fs/cgroup").join(rel);
        loop {
            if let Ok(s) = std::fs::read_to_string(cur.join("cpu.max")) {
                if let Some(v) = parse_cpu_max(&s) {
                    consider(v);
                }
            }
            if cur == Path::new("/sys/fs/cgroup") || !cur.pop() {
                break;
            }
        }
    }
    // cgroup v1: cpu controller mounted under /sys/fs/cgroup/cpu.
    for line in text.lines() {
        let mut it = line.splitn(3, ':');
        let _ = it.next();
        let controller = it.next().unwrap_or("");
        let path = it.next().unwrap_or("");
        if !controller.split(',').any(|c| c == "cpu") {
            continue;
        }
        let mut cur = PathBuf::from("/sys/fs/cgroup/cpu").join(path.trim_start_matches('/'));
        loop {
            let quota = std::fs::read_to_string(cur.join("cpu.cfs_quota_us"))
                .ok()
                .and_then(|s| s.trim().parse::<i64>().ok());
            let period = std::fs::read_to_string(cur.join("cpu.cfs_period_us"))
                .ok()
                .and_then(|s| s.trim().parse::<i64>().ok());
            if let (Some(q), Some(p)) = (quota, period) {
                if q > 0 && p > 0 {
                    consider(q as f64 / p as f64);
                }
            }
            if cur == Path::new("/sys/fs/cgroup/cpu") || !cur.pop() {
                break;
            }
        }
    }
    best
}

impl CpuInfo {
    pub fn detect() -> Self {
        let logical = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .max(1);
        let pairs = physical_core_pairs();
        let physical = if pairs.is_empty() {
            logical
        } else {
            pairs.len()
        };
        let quota = cpu_quota_cpus();
        let effective = quota
            .map(|q| (q.floor() as usize).max(1))
            .unwrap_or(logical)
            .min(logical)
            .max(1);
        let physical_effective = physical.min(effective).max(1);
        let source = match (pairs.is_empty(), quota.is_some()) {
            (false, true) => "cpuinfo+cgroup",
            (false, false) => "cpuinfo",
            (true, true) => "logical+cgroup",
            (true, false) => "logical",
        };
        Self {
            physical,
            logical,
            effective,
            physical_effective,
            smt: logical > physical,
            quota_cpus: quota,
            source: source.to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskKind {
    Nvme,
    SataSsd,
    Hdd,
    Virtual,
    Network,
    Zfs,
    Overlay,
    Unknown,
}

impl DiskKind {
    pub fn name(self) -> &'static str {
        match self {
            DiskKind::Nvme => "nvme",
            DiskKind::SataSsd => "sata-ssd",
            DiskKind::Hdd => "hdd",
            DiskKind::Virtual => "virtual",
            DiskKind::Network => "network",
            DiskKind::Zfs => "zfs",
            DiskKind::Overlay => "overlay",
            DiskKind::Unknown => "unknown",
        }
    }
    /// Point-read concurrency for prevout prefetch.
    pub fn reader_cap(self) -> usize {
        match self {
            DiskKind::Nvme => 8,
            DiskKind::SataSsd => 4,
            DiskKind::Hdd => 2,
            DiskKind::Virtual | DiskKind::Unknown | DiskKind::Zfs | DiskKind::Overlay => 4,
            DiskKind::Network => 2,
        }
    }
    pub fn jobs_cap(self) -> usize {
        match self {
            DiskKind::Hdd | DiskKind::Network => 2,
            // Fast local disks: half the cores can compact in parallel
            // without starving the single writer. Matches the Ssd/Nvme
            // profile defaults (8 = 2 flush + 6 compaction threads).
            DiskKind::Nvme | DiskKind::SataSsd => 8,
            DiskKind::Virtual | DiskKind::Unknown | DiskKind::Zfs | DiskKind::Overlay => 4,
        }
    }
    /// Block cache maximum for a machine with `ram_mib`. The page-cache floor
    /// is reserved separately; this is only the upper bound.
    pub fn cache_cap_mib(self, ram_mib: u64) -> u64 {
        match self {
            DiskKind::Nvme => (ram_mib / 4).clamp(1024, 8192),
            // Local SATA/unknown/virtual: a quarter of RAM is a safe ceiling
            // when the budget is fitted to MemAvailable. 8 GiB -> 2 GiB,
            // 16 GiB -> 4 GiB, 32 GiB -> 8 GiB.
            DiskKind::SataSsd | DiskKind::Virtual | DiskKind::Unknown | DiskKind::Overlay => {
                (ram_mib / 4).clamp(1024, 8192)
            }
            DiskKind::Zfs => (ram_mib / 4).clamp(1024, 8192),
            DiskKind::Hdd => (ram_mib / 8).clamp(512, 4096),
            DiskKind::Network => (ram_mib / 8).clamp(512, 2048),
        }
    }
    /// Bulk commit interval. Fewer, larger groups amortise fsync and locks;
    /// live mode uses a short interval set by the controller.
    pub fn bulk_commit_ms(self) -> u64 {
        match self {
            DiskKind::Hdd => 45_000,
            DiskKind::Network => 60_000,
            DiskKind::SataSsd => 30_000,
            DiskKind::Nvme => 20_000,
            DiskKind::Virtual | DiskKind::Unknown | DiskKind::Zfs | DiskKind::Overlay => 30_000,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DiskInfo {
    pub kind: DiskKind,
    pub device: Option<String>,
    /// Raw mountinfo `major:minor`, used for `same_device` when the device
    /// name cannot be resolved (bind mounts, overlay, btrfs subvolumes).
    pub dev: Option<String>,
    pub mount: String,
    pub fstype: String,
    pub mount_source: String,
    pub free_bytes: u64,
    pub total_bytes: u64,
    pub source: String,
}

fn unescape_mount(s: &str) -> String {
    s.replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

fn mount_for(path: &Path) -> Option<(String, String, String, String)> {
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let text = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    let mut best: Option<(usize, String, String, String, String)> = None;
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 5 {
            continue;
        }
        let mount = unescape_mount(fields[4]);
        if !canon.starts_with(&mount) {
            continue;
        }
        let sep = fields.iter().position(|f| *f == "-");
        let fstype = sep
            .and_then(|i| fields.get(i + 1))
            .copied()
            .unwrap_or("")
            .to_string();
        let mount_source = sep
            .and_then(|i| fields.get(i + 2))
            .copied()
            .unwrap_or("")
            .to_string();
        let len = mount.len();
        if best.as_ref().map(|b| len >= b.0).unwrap_or(true) {
            best = Some((len, fields[2].to_string(), mount, fstype, mount_source));
        }
    }
    best.map(|(_, majmin, mount, fstype, source)| (majmin, mount, fstype, source))
}

/// Resolve a mountinfo `source` (`/dev/...`, `UUID=...`, `LABEL=...`) to its
/// parent block device. Needed when `/sys/dev/block/<major:minor>` is absent
/// (bind mounts and some virtual filesystems).
fn source_base(source: &str) -> Option<String> {
    let path = if let Some(rest) = source.strip_prefix("UUID=") {
        PathBuf::from("/dev/disk/by-uuid").join(rest)
    } else if let Some(rest) = source.strip_prefix("LABEL=") {
        PathBuf::from("/dev/disk/by-label").join(rest)
    } else if source.starts_with("/dev/") {
        PathBuf::from(source)
    } else {
        return None;
    };
    let canon = std::fs::canonicalize(&path).ok()?;
    let name = canon.file_name()?.to_string_lossy().to_string();
    if name.starts_with("dm-") || name.starts_with("md") || name.starts_with("loop") {
        let dev = std::fs::read_to_string(format!("/sys/block/{name}/dev")).ok()?;
        return device_base(dev.trim(), 0);
    }
    Some(strip_partition(&name))
}

fn strip_partition(name: &str) -> String {
    if name.starts_with("nvme") {
        if let Some(pos) = name.rfind('p') {
            if name[pos + 1..].chars().all(|c| c.is_ascii_digit()) {
                return name[..pos].to_string();
            }
        }
        return name.to_string();
    }
    if name.starts_with("mmcblk") {
        if let Some(pos) = name.rfind('p') {
            return name[..pos].to_string();
        }
        return name.to_string();
    }
    if (name.starts_with("sd")
        || name.starts_with("vd")
        || name.starts_with("hd")
        || name.starts_with("xvd"))
        && name.ends_with(|c: char| c.is_ascii_digit())
    {
        return name
            .trim_end_matches(|c: char| c.is_ascii_digit())
            .to_string();
    }
    name.to_string()
}

fn device_base(majmin: &str, depth: u8) -> Option<String> {
    if depth > 4 {
        return None;
    }
    let link = std::fs::read_link(format!("/sys/dev/block/{majmin}")).ok()?;
    let name = link.file_name()?.to_string_lossy().to_string();
    if name.starts_with("dm-") || name.starts_with("md") || name.starts_with("loop") {
        let slaves = std::fs::read_dir(format!("/sys/block/{name}/slaves")).ok()?;
        for entry in slaves.flatten() {
            let child = entry.file_name().to_string_lossy().to_string();
            let child_majmin = std::fs::read_to_string(format!("/sys/block/{child}/dev")).ok()?;
            if let Some(base) = device_base(child_majmin.trim(), depth + 1) {
                return Some(base);
            }
        }
        return Some(name);
    }
    Some(strip_partition(&name))
}

fn disk_kind_for(base: &str, fstype: &str) -> (DiskKind, bool) {
    if fstype == "zfs" {
        return (DiskKind::Zfs, false);
    }
    if fstype == "overlay" || fstype == "overlay2" {
        return (DiskKind::Overlay, false);
    }
    if matches!(
        fstype,
        "nfs" | "nfs4" | "cifs" | "smb3" | "fuse" | "fuse.sshfs"
    ) {
        return (DiskKind::Network, false);
    }
    if base.starts_with("dm-") || base.starts_with("md") || base.starts_with("loop") {
        return (DiskKind::Virtual, false);
    }
    let rotational = std::fs::read_to_string(format!("/sys/block/{base}/queue/rotational"))
        .ok()
        .map(|s| s.trim() == "1")
        .unwrap_or(false);
    if base.starts_with("nvme") {
        (DiskKind::Nvme, false)
    } else if rotational {
        (DiskKind::Hdd, true)
    } else {
        (DiskKind::SataSsd, false)
    }
}

fn statvfs_bytes(path: &Path) -> Option<(u64, u64)> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    let total = (s.f_blocks as u64).saturating_mul(s.f_frsize as u64);
    let free = (s.f_bavail as u64).saturating_mul(s.f_frsize as u64);
    Some((free, total))
}

impl DiskInfo {
    pub fn detect(path: &Path) -> Self {
        let (kind, device, dev, mount, fstype, mount_source, source) = match mount_for(path) {
            Some((majmin, mount, fstype, mount_source)) => {
                let base = device_base(&majmin, 0).or_else(|| source_base(&mount_source));
                let kind = match base.as_deref() {
                    Some(b) => disk_kind_for(b, &fstype).0,
                    None if fstype == "zfs" => DiskKind::Zfs,
                    None if fstype == "overlay" || fstype == "overlay2" => DiskKind::Overlay,
                    None => DiskKind::Unknown,
                };
                (
                    kind,
                    base,
                    Some(majmin),
                    mount,
                    fstype,
                    mount_source,
                    "mountinfo+sysfs".to_string(),
                )
            }
            None => (
                DiskKind::Unknown,
                None,
                None,
                path.display().to_string(),
                String::new(),
                String::new(),
                "fallback".to_string(),
            ),
        };
        let (free_bytes, total_bytes) = statvfs_bytes(path).unwrap_or((0, 0));
        Self {
            kind,
            device,
            dev,
            mount,
            fstype,
            mount_source,
            free_bytes,
            total_bytes,
            source,
        }
    }
}

/// Decide whether the data and Core disks are the same physical device.
/// Prefer the resolved parent block device (so two partitions or LVs of one
/// disk count as shared), then the raw `major:minor` (bind mounts), then the
/// mount source and mount point.
fn disk_same(a: &DiskInfo, b: &DiskInfo) -> bool {
    if let (Some(x), Some(y)) = (&a.device, &b.device) {
        return x == y;
    }
    if let (Some(x), Some(y)) = (&a.dev, &b.dev) {
        return x == y;
    }
    if !a.mount_source.is_empty() && a.mount_source == b.mount_source {
        return true;
    }
    !a.mount.is_empty() && a.mount == b.mount
}

/// Startup plan derived from CPU, cgroup, RAM and the data/Core disks.
#[derive(Debug, Clone)]
pub struct MachinePlan {
    pub cpu: CpuInfo,
    pub mem: MemoryInfo,
    pub data_disk: DiskInfo,
    pub core_disk: DiskInfo,
    pub same_device: bool,
    /// Budget basis: `min(total, available - core_reserve)`.
    pub effective_mib: u64,
    /// Memory reserved for the Core that e-lite will start (0 for external).
    pub core_reserve_mib: u64,
    pub readers: usize,
    pub jobs: usize,
    /// Cache ceiling for this profile. The initial value comes from
    /// `cache_start_mib` and the hot controller grows towards this cap only
    /// while the OS has memory available.
    pub cache_mib: u64,
    pub cache_start_mib: u64,
    pub memtables_mib: u64,
    pub reserve_mib: u64,
    pub page_cache_floor_mib: u64,
    pub commit_ms: u64,
    pub batch_blocks: u32,
    pub max_group_txs: usize,
    /// Raw-block byte bound for one commit group. Scales down on small RAM.
    pub max_group_bytes: usize,
    pub core_par: u64,
    pub core_rpcthreads: u64,
    pub core_dbcache_mib: u64,
    pub prune_timeout_seconds: u64,
}

impl MachinePlan {
    pub fn detect(cfg: &Config, chain: Chain) -> Self {
        let mem = memory_info();
        let core_reserve = if cfg.core.manage == crate::config::CoreManage::External {
            0
        } else {
            fitted_dbcache_mib(mem.total_mib.unwrap_or(1024), mem.available_mib)
        };
        Self::detect_with_core_reserve(cfg, chain, core_reserve)
    }

    /// Detect the machine while telling the budget whether e-lite will start
    /// Core. Pass 0 when Core is external or already running: its `-dbcache`
    /// is already reflected in `MemAvailable`.
    pub fn detect_with_core_reserve(cfg: &Config, chain: Chain, core_reserve_mib: u64) -> Self {
        let cpu = CpuInfo::detect();
        let mem = memory_info();
        let data_disk = DiskInfo::detect(&cfg.data_dir(chain));
        let core_disk = DiskInfo::detect(&cfg.core_datadir(chain));
        Self::from_parts_with_core_reserve(cpu, mem, data_disk, core_disk, cfg, core_reserve_mib)
    }

    /// Pure mapping from detected parts to knobs, testable with a synthetic
    /// 4C/8T machine. Conservative: reserves the fitted Core cache when e-lite
    /// manages Core; callers that know Core is already running use
    /// `from_parts_with_core_reserve`.
    #[allow(dead_code)]
    pub fn from_parts(
        cpu: CpuInfo,
        mem: MemoryInfo,
        data_disk: DiskInfo,
        core_disk: DiskInfo,
        cfg: &Config,
    ) -> Self {
        let core_reserve = if cfg.core.manage == crate::config::CoreManage::External {
            0
        } else {
            fitted_dbcache_mib(mem.total_mib.unwrap_or(1024), mem.available_mib)
        };
        Self::from_parts_with_core_reserve(cpu, mem, data_disk, core_disk, cfg, core_reserve)
    }

    pub fn from_parts_with_core_reserve(
        cpu: CpuInfo,
        mem: MemoryInfo,
        data_disk: DiskInfo,
        core_disk: DiskInfo,
        cfg: &Config,
        core_reserve_mib: u64,
    ) -> Self {
        let same_device = disk_same(&data_disk, &core_disk);
        let ram_mib = mem.total_mib.unwrap_or(1024).max(256);
        let physical = cpu.physical_effective.max(1);

        let readers = if cfg.index.readers > 0 {
            cfg.index.readers
        } else {
            physical.clamp(2, data_disk.kind.reader_cap())
        };
        let jobs = if cfg.index.background_jobs > 0 {
            cfg.index.background_jobs
        } else {
            // Same rule as the profile reload: the pool splits between flush
            // and compaction, so `physical/2` gave a 4-core host a single
            // compaction thread and the compaction debt grew at drain rates.
            physical.clamp(2, data_disk.kind.jobs_cap())
        };
        let core_dbcache_mib = fitted_dbcache_mib(ram_mib, mem.available_mib);
        let core_reserve_mib = core_reserve_mib.min(mem.available_mib.saturating_sub(128));
        // The startup budget is fitted to what the OS can give now, minus the
        // Core cache if e-lite is about to start Core. Installed RAM only caps
        // the result; it is not the allocation basis.
        let budget = budget_with(
            cfg,
            ram_mib,
            mem.available_mib,
            mem.cached_mib,
            None,
            None,
            core_reserve_mib,
        );
        let profile_cap = data_disk.kind.cache_cap_mib(ram_mib);
        let cache_mib = if cfg.index.cache_mib > 0 {
            budget.cache_max_mib
        } else {
            budget.cache_max_mib.min(profile_cap.max(8))
        };
        let cache_start_mib = budget.cache_start_mib.min(cache_mib);
        let reserve_mib = budget.reserve_mib;
        let page_cache_floor_mib = budget.page_cache_floor_mib;
        let memtables_mib = budget.memtables_mib;
        let commit_ms = if cfg.index.commit_interval_ms > 0 {
            cfg.index.commit_interval_ms
        } else {
            data_disk.kind.bulk_commit_ms()
        };
        let batch_blocks = if cfg.index.batch_blocks > 0 && cfg.index.batch_blocks != 256 {
            cfg.index.batch_blocks.min(512)
        } else {
            512
        };
        let max_group_txs = if data_disk.kind == DiskKind::Hdd {
            500_000
        } else {
            250_000
        };
        let max_group_bytes =
            (((ram_mib / 128) as usize) * 1024 * 1024).clamp(64 * 1024 * 1024, 256 * 1024 * 1024);

        Self {
            cpu,
            mem,
            data_disk,
            core_disk,
            same_device,
            effective_mib: budget.effective_mib,
            core_reserve_mib,
            readers,
            jobs,
            cache_mib,
            cache_start_mib,
            memtables_mib,
            reserve_mib,
            page_cache_floor_mib,
            commit_ms,
            batch_blocks,
            max_group_txs,
            max_group_bytes,
            core_par: physical.saturating_sub(1).clamp(1, 8) as u64,
            core_rpcthreads: physical.clamp(4, 16) as u64,
            core_dbcache_mib,
            prune_timeout_seconds: 120,
        }
    }

    pub fn describe(&self) -> String {
        format!(
            "cpu {}C/{}T (eff {}, {}{}) | mem total {} avail {} effective {} MiB \
             (cgroup {}, core reserve {}, reserve {}, page floor {}) | \
             data disk {} ({}, fstype {}, source {}, dev {}, same={}) | core par={} rpcthreads={} \
             dbcache={} | index readers={} jobs={} cache {}..{} memtables={} commit={}ms batch={} \
             txs={} bytes={}MiB",
            self.cpu.physical,
            self.cpu.logical,
            self.cpu.effective,
            self.cpu.source,
            if self.cpu.smt { ", smt" } else { "" },
            self.mem.total_mib.unwrap_or(0),
            self.mem.available_mib,
            self.effective_mib,
            self.mem
                .cgroup_limit_mib
                .map(|v| v.to_string())
                .unwrap_or_else(|| "none".to_string()),
            self.core_reserve_mib,
            self.reserve_mib,
            self.page_cache_floor_mib,
            self.data_disk.kind.name(),
            self.data_disk.mount,
            self.data_disk.fstype,
            self.data_disk.source,
            self.data_disk
                .device
                .clone()
                .or_else(|| self.data_disk.dev.clone())
                .unwrap_or_else(|| "-".to_string()),
            self.same_device,
            self.core_par,
            self.core_rpcthreads,
            self.core_dbcache_mib,
            self.readers,
            self.jobs,
            self.cache_start_mib,
            self.cache_mib,
            self.memtables_mib,
            self.commit_ms,
            self.batch_blocks,
            self.max_group_txs,
            self.max_group_bytes / (1024 * 1024)
        )
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "cpu": {
                "physical": self.cpu.physical,
                "logical": self.cpu.logical,
                "effective": self.cpu.effective,
                "physical_effective": self.cpu.physical_effective,
                "smt": self.cpu.smt,
                "quota_cpus": self.cpu.quota_cpus,
                "source": self.cpu.source,
            },
            "memory": {
                "total_mib": self.mem.total_mib,
                "available_mib": self.mem.available_mib,
                "effective_mib": self.effective_mib,
                "core_reserve_mib": self.core_reserve_mib,
                "cgroup_limit_mib": self.mem.cgroup_limit_mib,
                "cgroup_used_mib": self.mem.cgroup_used_mib,
                "reserve_mib": self.reserve_mib,
                "page_cache_floor_mib": self.page_cache_floor_mib,
                "sreclaimable_mib": self.mem.sreclaimable_mib,
                "shmem_mib": self.mem.shmem_mib,
                "arc_mib": self.mem.arc_mib,
            },
            "disk": {
                "data": {
                    "kind": self.data_disk.kind.name(),
                    "mount": self.data_disk.mount,
                    "device": self.data_disk.device,
                    "dev": self.data_disk.dev,
                    "mount_source": self.data_disk.mount_source,
                    "fstype": self.data_disk.fstype,
                    "free_bytes": self.data_disk.free_bytes,
                    "total_bytes": self.data_disk.total_bytes,
                    "source": self.data_disk.source,
                },
                "core": {
                    "kind": self.core_disk.kind.name(),
                    "mount": self.core_disk.mount,
                    "device": self.core_disk.device,
                    "dev": self.core_disk.dev,
                    "mount_source": self.core_disk.mount_source,
                    "fstype": self.core_disk.fstype,
                    "free_bytes": self.core_disk.free_bytes,
                    "total_bytes": self.core_disk.total_bytes,
                    "source": self.core_disk.source,
                },
                "same_device": self.same_device,
            },
            "plan": {
                "readers": self.readers,
                "jobs": self.jobs,
                "cache_mib": self.cache_mib,
                "cache_start_mib": self.cache_start_mib,
                "memtables_mib": self.memtables_mib,
                "commit_ms": self.commit_ms,
                "batch_blocks": self.batch_blocks,
                "max_group_txs": self.max_group_txs,
                "max_group_bytes": self.max_group_bytes,
                "core_par": self.core_par,
                "core_rpcthreads": self.core_rpcthreads,
                "core_dbcache_mib": self.core_dbcache_mib,
                "prune_timeout_seconds": self.prune_timeout_seconds,
            },
        })
    }
}

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
    adaptive_memtables: AtomicBool,
    memtables_locked: bool,
    memtable_target_since: Mutex<Option<(u64, Instant)>>,
    last_memtable_change: Mutex<Option<Instant>>,
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
    #[allow(dead_code)]
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
            adaptive_memtables: AtomicBool::new(
                cfg.index.adaptive_memtables && cfg.index.write_buffer_mib == 0,
            ),
            memtables_locked: cfg.index.write_buffer_mib > 0,
            memtable_target_since: Mutex::new(None),
            last_memtable_change: Mutex::new(None),
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

    /// Blocks/s over the last commit cycle; falls back to the since-start
    /// average only until a cycle is available. Do not take the maximum: an
    /// optimistic rate would allow a larger backlog than the target.
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
        self.adaptive_memtables.store(
            cfg.index.adaptive_memtables && cfg.index.write_buffer_mib == 0,
            Ordering::Relaxed,
        );
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

    fn reader_streak_ok(&self, direction: i8) -> bool {
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
    fn dynamic_reserve_mib(&self, mem: &MemoryInfo) -> u64 {
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

    fn memtable_step(&self, store: &Store, mem: &MemoryInfo) -> Option<String> {
        if !self.adaptive_memtables.load(Ordering::Relaxed) || self.memtables_locked {
            return None;
        }
        let current = self
            .cache_mib()
            .saturating_add(self.memtables_mib.load(Ordering::Relaxed))
            .saturating_add(self.runtime_mib);
        let effective = self
            .total_mib
            .min(mem.available_mib.saturating_add(current))
            .max(128);
        let target = (effective / 16).clamp(512, 2048);
        let current_mem = self.memtables_mib.load(Ordering::Relaxed);
        if target.abs_diff(current_mem) < 128 {
            if let Ok(mut since) = self.memtable_target_since.lock() {
                *since = None;
            }
            return None;
        }
        let grow = target > current_mem;
        let margin = (effective / 8).clamp(256, 2048);
        let reserve = self.dynamic_reserve_mib(mem);
        let allowed = if grow {
            mem.available_mib
                >= reserve
                    .saturating_add(self.cache_mib())
                    .saturating_add(target)
                    .saturating_add(margin)
        } else {
            mem.available_mib < reserve.saturating_add(512)
                || self.cache_mib() <= self.cache_floor_mib
        };
        if !allowed {
            if let Ok(mut since) = self.memtable_target_since.lock() {
                *since = None;
            }
            return None;
        }
        {
            let Ok(mut since) = self.memtable_target_since.lock() else {
                return None;
            };
            match *since {
                Some((t, at)) if t == target => {
                    if at.elapsed() < Duration::from_secs(600) {
                        return None;
                    }
                }
                _ => {
                    *since = Some((target, Instant::now()));
                    return None;
                }
            }
        }
        if let Ok(last) = self.last_memtable_change.lock() {
            if let Some(at) = *last {
                if at.elapsed() < Duration::from_secs(600) {
                    return None;
                }
            }
        }
        if let Err(e) = store.set_write_buffer_bytes(target as usize * 1024 * 1024) {
            eprintln!("[e-lite] cannot adjust memtables to {target} MiB: {e}");
            return None;
        }
        self.memtables_mib.store(target, Ordering::Relaxed);
        if let Ok(mut since) = self.memtable_target_since.lock() {
            *since = None;
        }
        if let Ok(mut last) = self.last_memtable_change.lock() {
            *last = Some(Instant::now());
        }
        let direction = if grow { "up" } else { "down" };
        eprintln!(
            "[e-lite] memory: memtable budget {current_mem} -> {target} MiB ({direction}); \
             available {} MiB, effective {} MiB",
            mem.available_mib, effective
        );
        Some(format!(
            "memtables {direction}: {current_mem} -> {target} MiB"
        ))
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
            if let Some(reason) = self.memtable_step(store, &mem) {
                if let Some(stats) = stats {
                    self.publish(store, stats);
                }
                return Some(reason);
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
        // The dynamic manager owns the cap once it wrote it: no warning.
        if self.arc_managed.load(Ordering::Relaxed) {
            return;
        }
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

    /// Dynamic ZFS ARC manager. While Core downloads or the index drains, a
    /// big ARC makes pool reads nearly free (97-98 % hit rate); once the
    /// index is fully synced the working set is tiny and the RAM is better
    /// left visible to the memory plan. Writes
    /// `/sys/module/zfs/parameters/zfs_arc_max` only when e-lite can (root or
    /// a writable parameter); without permission it keeps the warn-only mode.
    /// Runs on the 5 s `el-tune` tick, with a 5 min dwell per change.
    pub fn arc_tick(&self, lag: u64) {
        if !self.zfs_arc_dynamic.load(Ordering::Relaxed) {
            return;
        }
        if zfs_arc_mib().is_none() {
            return; // no ZFS ARC on this host
        }
        let mut state = match self.arc_state.lock() {
            Ok(state) => state,
            Err(_) => return,
        };
        let Some(current_mib) = std::fs::read_to_string(ARC_PARAM_PATH)
            .ok()
            .and_then(|text| text.trim().parse::<u64>().ok())
            .map(|bytes| bytes / (1024 * 1024))
        else {
            return;
        };
        if state.writable.is_none() || state.writable == Some(false) {
            // Probe by writing the current value back: it changes nothing on
            // success and fails with EACCES/EPERM without permission. A
            // negative result is retried every 10 minutes, so granting the
            // permissions (see `e-lite arc-setup`) enables the manager
            // without restarting.
            if let Some(at) = state.last_probe {
                if at.elapsed() < Duration::from_secs(600) {
                    return;
                }
            }
            state.last_probe = Some(Instant::now());
            let previous = state.writable;
            match std::fs::write(ARC_PARAM_PATH, (current_mib * 1024 * 1024).to_string()) {
                Ok(()) => {
                    state.writable = Some(true);
                    if previous == Some(false) {
                        eprintln!(
                            "[e-lite] arc: dynamic management enabled (zfs_arc_max is \
                             writable now)"
                        );
                    }
                }
                Err(e) => {
                    let was = previous;
                    state.writable = Some(false);
                    if was.is_none() {
                        eprintln!(
                            "[e-lite] arc: dynamic management unavailable (cannot write \
                             zfs_arc_max: {e}); run `e-lite arc-setup` or keep the manual cap \
                             from the startup warning"
                        );
                    }
                    return;
                }
            }
        }
        if state.writable != Some(true) {
            return;
        }
        let ibd_like = self.core_ibd() || lag > 1000;
        let mem = memory_info();
        let target = if ibd_like {
            let configured = self.zfs_arc_max_mib.load(Ordering::Relaxed);
            let auto = mem.total_mib.unwrap_or(0) / 2;
            if configured > 0 {
                configured
            } else {
                auto
            }
        } else {
            self.zfs_arc_min_mib.load(Ordering::Relaxed)
        }
        .max(512);
        // Hysteresis: a phase flip that lasts one block must not flap the cap.
        if state.last_written_mib == Some(target)
            || current_mib == target
            || state
                .last_change
                .map(|at| at.elapsed() < Duration::from_secs(300))
                .unwrap_or(false)
        {
            return;
        }
        match std::fs::write(ARC_PARAM_PATH, (target * 1024 * 1024).to_string()) {
            Ok(()) => {
                self.arc_managed.store(true, Ordering::Relaxed);
                state.last_written_mib = Some(target);
                state.last_change = Some(Instant::now());
                eprintln!(
                    "[e-lite] arc: cap {current_mib} -> {target} MiB ({phase}; ZFS ARC \
                     managed dynamically)",
                    phase = if ibd_like {
                        "Core IBD or drain"
                    } else {
                        "fully synced"
                    }
                );
            }
            Err(e) => {
                state.writable = Some(false);
                eprintln!("[e-lite] arc: cannot write zfs_arc_max anymore: {e}; warn-only");
            }
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

/// Where the effective memory limit came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LimitSource {
    #[default]
    None,
    Host,
    CgroupV2,
    CgroupV1,
}

/// Memory figures used for cache/reader decisions and diagnostics.
///
/// `total_mib` is already the minimum of the host and every visible cgroup
/// limit, and `available_mib` pairs each limit with the usage of that same
/// cgroup. Never subtract leaf usage from an ancestor's limit.
#[derive(Debug, Clone, Copy, Default)]
pub struct MemoryInfo {
    pub total_mib: Option<u64>,
    /// Memory that can actually be used before reclaim/OOM.
    pub available_mib: u64,
    pub host_total_mib: Option<u64>,
    /// `/proc/meminfo` value before applying a cgroup limit.
    pub host_available_mib: u64,
    pub cgroup_limit_mib: Option<u64>,
    pub cgroup_used_mib: Option<u64>,
    /// Page cache in the cgroup that the kernel can reclaim under pressure.
    pub reclaimable_mib: Option<u64>,
    /// `MemFree`, useful for diagnostics.
    pub free_mib: u64,
    /// `Buffers + Cached + SReclaimable`: page cache that a growing anonymous
    /// cache evicts.
    pub cached_mib: u64,
    pub sreclaimable_mib: u64,
    pub shmem_mib: u64,
    /// ZFS ARC size, when `/proc/spl/kstat/zfs/arcstats` is available.
    pub arc_mib: Option<u64>,
    pub source: LimitSource,
    /// True when a memory controller was found and read successfully.
    pub cgroup_detected: bool,
    /// True when the controller exists but reports no numeric limit.
    pub cgroup_unlimited: bool,
}

pub fn total_ram_mib() -> Option<u64> {
    memory_info().total_mib
}

pub fn mem_available_mib() -> u64 {
    memory_info().available_mib
}

pub fn memory_info() -> MemoryInfo {
    let host_total = meminfo_mib("MemTotal");
    let host_available = meminfo_mib("MemAvailable")
        .or_else(|| meminfo_mib("MemFree"))
        .unwrap_or(0);
    let cg = cgroup_memory();
    let total = match (host_total, cg.limit_mib) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };
    let available = match cg.available_mib {
        Some(cg_av) => host_available.min(cg_av),
        None => host_available,
    };
    let free = meminfo_mib("MemFree").unwrap_or(0);
    let sreclaimable = meminfo_mib("SReclaimable").unwrap_or(0);
    let shmem = meminfo_mib("Shmem").unwrap_or(0);
    let cached = meminfo_mib("Buffers")
        .unwrap_or(0)
        .saturating_add(meminfo_mib("Cached").unwrap_or(0))
        .saturating_add(sreclaimable);
    let arc_mib = zfs_arc_mib();
    let source = if cg.source == LimitSource::None && host_total.is_some() {
        LimitSource::Host
    } else {
        cg.source
    };
    MemoryInfo {
        total_mib: total,
        available_mib: available,
        host_total_mib: host_total,
        host_available_mib: host_available,
        cgroup_limit_mib: cg.limit_mib,
        cgroup_used_mib: cg.used_mib,
        reclaimable_mib: cg.reclaimable_mib,
        free_mib: free,
        cached_mib: cached,
        sreclaimable_mib: sreclaimable,
        shmem_mib: shmem,
        arc_mib,
        source,
        cgroup_detected: cg.detected,
        cgroup_unlimited: cg.detected && cg.limit_mib.is_none(),
    }
}

/// ZFS ARC `size` in MiB from the kstat file, when present. On ZFS this is
/// memory that `/proc/meminfo` does not show as `Cached`, which is why a low
/// buff/cache on a 32 GiB host can still mean "ARC is using the RAM".
fn zfs_arc_mib() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/spl/kstat/zfs/arcstats").ok()?;
    parse_zfs_arc_mib(&text)
}

/// kstat rows are `name type value`; the second column is the data type
/// (4 = uint64), the value is the third. Taking the second column made
/// `arc` print 0 MiB on every ZFS host (4 bytes / 1024 / 1024).
fn parse_zfs_arc_mib(text: &str) -> Option<u64> {
    zfs_arc_field(text, "size").map(|bytes| bytes / 1024 / 1024)
}

/// Where the dynamic ARC manager stands for this run.
#[derive(Debug, Default)]
struct ArcManagerState {
    /// None = not probed yet. The probe writes the *current* value back, so
    /// it changes nothing when it succeeds.
    writable: Option<bool>,
    /// Cap actually written by this run, so unchanged targets are skipped.
    last_written_mib: Option<u64>,
    last_change: Option<Instant>,
    /// Last permission probe (a negative result is retried, so the operator
    /// can fix the sysfs permissions without restarting e-lite).
    last_probe: Option<Instant>,
}

const ARC_PARAM_PATH: &str = "/sys/module/zfs/parameters/zfs_arc_max";

/// Public path of the ZFS ARC cap parameter (used by `e-lite arc-setup`).
pub const ARC_CAP_PATH: &str = ARC_PARAM_PATH;

/// Read one numeric field from `/proc/spl/kstat/zfs/arcstats` content.
fn zfs_arc_field(text: &str, field: &str) -> Option<u64> {
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        if fields.next() != Some(field) {
            continue;
        }
        fields.next()?; // kstat type column
        return fields.next()?.parse::<u64>().ok();
    }
    None
}

/// Cumulative ZFS ARC `hits`/`misses` counters, for delta-based hit rates.
pub fn zfs_arc_counters() -> Option<(u64, u64)> {
    let text = std::fs::read_to_string("/proc/spl/kstat/zfs/arcstats").ok()?;
    parse_zfs_arc_counters(&text)
}

fn parse_zfs_arc_counters(text: &str) -> Option<(u64, u64)> {
    Some((zfs_arc_field(text, "hits")?, zfs_arc_field(text, "misses")?))
}

/// Warn about an apparently uncapped ZFS ARC. A cap at the recommended 25%
/// never triggers: `arc * 4 <= total` at exactly 25%. Returns true when it
/// printed, so callers can rate-limit. `prefix` is the log prefix.
pub fn warn_uncapped_arc(mem: &MemoryInfo, prefix: &str) -> bool {
    let (Some(arc), Some(total)) = (mem.arc_mib, mem.total_mib) else {
        return false;
    };
    if total == 0 || arc * 4 <= total {
        return false;
    }
    let pct = arc * 100 / total;
    let suggest = (total / 4).min(8192);
    eprintln!(
        "{prefix} ZFS ARC is using {arc} MiB ({pct}% of RAM) without an apparent cap; it is \
         invisible to MemAvailable and squeezes the cache plan. Cap it, e.g. \
         zfs_arc_max={suggest} MiB (min(25% RAM, 8 GiB)) in /sys/module/zfs/parameters/\
         zfs_arc_max and /etc/modprobe.d",
    );
    true
}

/// Linux PSI pressure for I/O: `(some avg10, some avg60, full avg10,
/// full avg60)`. `None` when PSI is unavailable (file missing, `psi=0`).
pub fn io_pressure() -> Option<(f64, f64, f64, f64)> {
    let text = std::fs::read_to_string("/proc/pressure/io").ok()?;
    parse_io_pressure(&text)
}

fn parse_io_pressure(text: &str) -> Option<(f64, f64, f64, f64)> {
    let mut some = None;
    let mut full = None;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let kind = parts.next()?;
        let mut avg10 = None;
        let mut avg60 = None;
        for kv in parts {
            let (k, v) = kv.split_once('=')?;
            match k {
                "avg10" => avg10 = v.parse::<f64>().ok(),
                "avg60" => avg60 = v.parse::<f64>().ok(),
                _ => {}
            }
        }
        match kind {
            "some" => some = Some((avg10?, avg60?)),
            "full" => full = Some((avg10?, avg60?)),
            _ => {}
        }
    }
    let (s10, s60) = some?;
    let (f10, f60) = full?;
    Some((s10, s60, f10, f60))
}

pub fn process_rss_mib() -> u64 {
    let Ok(text) = std::fs::read_to_string("/proc/self/status") else {
        return 0;
    };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest
                .split_whitespace()
                .next()
                .and_then(|v| v.parse::<u64>().ok())
                .map(|kb| kb / 1024)
                .unwrap_or(0);
        }
    }
    0
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

fn read_trim(path: &std::path::Path) -> Option<String> {
    Some(std::fs::read_to_string(path).ok()?.trim().to_string())
}

fn parse_mib(value: &str) -> Option<u64> {
    value.parse::<u64>().ok().map(|v| v / 1024 / 1024)
}

fn read_u64(path: &std::path::Path) -> Option<u64> {
    read_trim(path)?.parse::<u64>().ok()
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

/// One level of the cgroup tree that the process belongs to.
#[derive(Debug, Clone, Copy, Default)]
struct CgLevel {
    limit_mib: Option<u64>,
    used_mib: Option<u64>,
    reclaimable_mib: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default)]
struct CgMemory {
    limit_mib: Option<u64>,
    used_mib: Option<u64>,
    reclaimable_mib: Option<u64>,
    /// Headroom computed per level: `limit - (used - reclaimable)`.
    available_mib: Option<u64>,
    detected: bool,
    source: LimitSource,
}

/// Parse `/proc/self/cgroup` for the unified (v2) hierarchy.
fn cgroup_v2_path() -> Option<String> {
    let text = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix("0::").map(|v| v.trim().to_string()))
}

/// Parse `/proc/self/cgroup` for the `memory` controller of a v1 hierarchy.
fn cgroup_v1_path() -> Option<String> {
    let text = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    text.lines().find_map(|line| {
        let mut parts = line.splitn(3, ':');
        let _hierarchy = parts.next()?;
        let controllers = parts.next()?;
        let path = parts.next()?;
        if controllers.split(',').any(|c| c == "memory") {
            Some(path.trim().to_string())
        } else {
            None
        }
    })
}

/// `(mount point, root)` for a cgroup filesystem from `/proc/self/mountinfo`.
fn cgroup_mount(v2: bool) -> Option<(String, String)> {
    let info = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    for line in info.lines() {
        let Some((pre, post)) = line.split_once(" - ") else {
            continue;
        };
        let fields: Vec<&str> = pre.split_whitespace().collect();
        if fields.len() < 5 {
            continue;
        }
        let mut after = post.split_whitespace();
        let fstype = after.next().unwrap_or("");
        let _source = after.next();
        let super_opts = after.next().unwrap_or("");
        let wanted = if v2 {
            fstype == "cgroup2"
        } else {
            fstype == "cgroup" && super_opts.split(',').any(|o| o == "memory")
        };
        if wanted {
            return Some((fields[4].to_string(), fields[3].to_string()));
        }
    }
    None
}

fn read_cg_level(dir: &std::path::Path, v1: bool) -> CgLevel {
    if v1 {
        let limit = read_u64(&dir.join("memory/memory.limit_in_bytes")).and_then(|v| {
            // v1 uses a huge sentinel for "unlimited".
            if v >= (1u64 << 60) {
                None
            } else {
                Some(v / 1024 / 1024)
            }
        });
        CgLevel {
            limit_mib: limit,
            used_mib: read_u64(&dir.join("memory/memory.usage_in_bytes")).map(|v| v / 1024 / 1024),
            reclaimable_mib: std::fs::read_to_string(dir.join("memory/memory.stat"))
                .ok()
                .and_then(|t| cgroup_stat_from(&t, "inactive_file")),
        }
    } else {
        let limit = read_trim(&dir.join("memory.max")).and_then(|raw| {
            if raw == "max" {
                None
            } else {
                parse_mib(&raw)
            }
        });
        CgLevel {
            limit_mib: limit,
            used_mib: read_u64(&dir.join("memory.current")).map(|v| v / 1024 / 1024),
            reclaimable_mib: std::fs::read_to_string(dir.join("memory.stat"))
                .ok()
                .and_then(|t| cgroup_stat_from(&t, "inactive_file")),
        }
    }
}

/// Resolve the process's cgroup directory and every visible ancestor up to the
/// mount root, then combine their limits. A limit closer to the root can never
/// be exceeded by a descendant, so the effective limit is the smallest one.
fn cgroup_memory() -> CgMemory {
    let (v2, path, mount) = match (cgroup_v2_path(), cgroup_mount(true)) {
        (Some(path), Some(mount)) => (true, path, mount),
        _ => match (cgroup_v1_path(), cgroup_mount(false)) {
            (Some(path), Some(mount)) => (false, path, mount),
            _ => return cgroup_memory_fallback(),
        },
    };
    let (mount_point, root) = mount;
    let base = std::path::PathBuf::from(&mount_point);
    let rel = path
        .strip_prefix(&root)
        .unwrap_or(path.as_str())
        .trim_start_matches('/');
    let leaf = if rel.is_empty() {
        base.clone()
    } else {
        base.join(rel)
    };

    let mut levels = Vec::new();
    let mut dir = leaf.clone();
    loop {
        let level = read_cg_level(&dir, !v2);
        let exists = dir
            .join(if v2 {
                "memory.max"
            } else {
                "memory/memory.limit_in_bytes"
            })
            .exists()
            || dir
                .join(if v2 {
                    "memory.current"
                } else {
                    "memory/memory.usage_in_bytes"
                })
                .exists();
        if exists || level.limit_mib.is_some() || level.used_mib.is_some() {
            levels.push(level);
        }
        if dir == base || !dir.pop() {
            break;
        }
        if !dir.starts_with(&base) {
            break;
        }
    }
    if levels.is_empty() {
        return cgroup_memory_fallback();
    }

    let mut out = CgMemory {
        detected: true,
        source: if v2 {
            LimitSource::CgroupV2
        } else {
            LimitSource::CgroupV1
        },
        ..Default::default()
    };
    for level in &levels {
        if let Some(limit) = level.limit_mib {
            out.limit_mib = Some(out.limit_mib.map_or(limit, |cur| cur.min(limit)));
        }
        if let Some(used) = level.used_mib {
            out.used_mib = Some(out.used_mib.map_or(used, |cur| cur.max(used)));
        }
        if let Some(reclaim) = level.reclaimable_mib {
            out.reclaimable_mib = Some(out.reclaimable_mib.map_or(reclaim, |cur| cur.max(reclaim)));
        }
        // Pair this level's limit with this same level's usage.
        if let (Some(limit), Some(used)) = (level.limit_mib, level.used_mib) {
            let reclaim = level.reclaimable_mib.unwrap_or(0).min(used);
            let headroom = limit.saturating_sub(used.saturating_sub(reclaim));
            out.available_mib = Some(out.available_mib.map_or(headroom, |cur| cur.min(headroom)));
        }
    }
    out
}

/// Containers with an unusual mount layout still get the legacy fixed paths,
/// which is better than silently using the host's total RAM.
fn cgroup_memory_fallback() -> CgMemory {
    let v2 = std::path::Path::new("/sys/fs/cgroup/memory.max").exists();
    let v1 = std::path::Path::new("/sys/fs/cgroup/memory/memory.limit_in_bytes").exists();
    if !v2 && !v1 {
        return CgMemory::default();
    }
    let level = read_cg_level(std::path::Path::new("/sys/fs/cgroup"), v1 && !v2);
    let available = match (level.limit_mib, level.used_mib) {
        (Some(limit), Some(used)) => {
            let reclaim = level.reclaimable_mib.unwrap_or(0).min(used);
            Some(limit.saturating_sub(used.saturating_sub(reclaim)))
        }
        _ => None,
    };
    CgMemory {
        limit_mib: level.limit_mib,
        used_mib: level.used_mib,
        reclaimable_mib: level.reclaimable_mib,
        available_mib: available,
        detected: true,
        source: if v2 {
            LimitSource::CgroupV2
        } else {
            LimitSource::CgroupV1
        },
    }
}

/// Minimum RAM for a full core + e-lite setup. Machines sold as 8 GiB report
/// slightly less after firmware/kernel reservations, so the check uses the
/// usable class rather than an exact 8192 MiB.
pub const MIN_RAM_MAINNET_MIB: u64 = 7_500;
pub const MIN_RAM_OTHER_MIB: u64 = 3_800;

pub fn minimum_ram_mib(chain: Chain) -> u64 {
    match chain {
        Chain::Mainnet => MIN_RAM_MAINNET_MIB,
        _ => MIN_RAM_OTHER_MIB,
    }
}

/// Pure form of the minimum-RAM rule, testable without the host.
pub fn ram_requirement_error(total_mib: u64, chain: Chain) -> Option<String> {
    let need = minimum_ram_mib(chain);
    if total_mib >= need {
        return None;
    }
    Some(format!(
        "only {total_mib} MiB of usable RAM detected; e-lite on {} needs at least {need} MiB          ({}) because Bitcoin Core and the index run together. Free memory, raise the          container limit, or use a larger machine. `e-lite doctor` shows the detected limits.",
        chain.name(),
        if chain == Chain::Mainnet {
            "8 GB class"
        } else {
            "4 GB class"
        }
    ))
}

/// Fail before touching certificates, databases or Core when the machine is
/// below the supported minimum. The check uses the cgroup-aware total.
pub fn check_minimum_ram(cfg: &Config, chain: Chain) -> Result<(), String> {
    let env_override = std::env::var("E_LITE_ALLOW_LOW_MEMORY")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if cfg.index.allow_low_memory || env_override {
        eprintln!(
            "[e-lite] WARNING: allow_low_memory is set; the {} MiB minimum for {} is not enforced",
            minimum_ram_mib(chain),
            chain.name()
        );
        return Ok(());
    }
    let mem = memory_info();
    let total = mem.total_mib.unwrap_or(0);
    match ram_requirement_error(total, chain) {
        Some(mut msg) => {
            let where_ = match mem.source {
                LimitSource::CgroupV2 => "cgroup v2 limit",
                LimitSource::CgroupV1 => "cgroup v1 limit",
                LimitSource::Host | LimitSource::None => "host RAM",
            };
            msg = msg.replace(
                "of usable RAM detected;",
                &format!("of usable RAM detected ({where_});"),
            );
            Err(msg)
        }
        None => Ok(()),
    }
}

/// Memory split for the indexer. Kept as a pure calculation so it can be
/// tested without touching the host.
#[derive(Debug, Clone, Copy)]
pub struct MemoryBudget {
    pub total_mib: u64,
    pub available_mib: u64,
    /// Budget actually used: `min(total, available - core_reserve)`. The
    /// machine's total RAM is only a ceiling; e-lite plans from what the OS
    /// can give it now.
    pub effective_mib: u64,
    /// Memory reserved for a Core that e-lite is about to start.
    pub core_reserve_mib: u64,
    pub cached_mib: u64,
    pub reserve_mib: u64,
    pub ceiling_mib: u64,
    pub memtables_mib: u64,
    pub runtime_mib: u64,
    pub cache_max_mib: u64,
    pub cache_start_mib: u64,
    pub cache_floor_mib: u64,
    /// Page cache the block cache must not evict under normal pressure.
    pub page_cache_floor_mib: u64,
    /// Sum of the components the controller plans to keep resident.
    pub planned_mib: u64,
    /// True when the floor requirements could not fit inside the ceiling.
    pub over_budget: bool,
}

const MIN_CACHE_MIB: u64 = 64;
const MIN_RUNTIME_MIB: u64 = 96;

fn auto_reserve_mib(total_mib: u64) -> u64 {
    if total_mib <= 2048 {
        (total_mib / 4).max(256)
    } else {
        // 32 GiB -> 4 GiB: the page cache must keep room for Core's block
        // files and the index's SST reads, not just for the OS.
        (total_mib / 8).clamp(2048, 4096)
    }
}

/// Hot reserve for the block cache. `MemAvailable` already excludes e-lite's
/// resident memory; the reserve is only for the OS/page cache, so it must
/// shrink with available memory instead of staying at the startup value.
fn dynamic_reserve_for(startup_reserve_mib: u64, available_mib: u64) -> u64 {
    (available_mib / 2)
        .max(1024)
        .min(startup_reserve_mib.max(1))
}

/// Core's `-dbcache` from what the OS can give it, not from the installed RAM.
/// 8 GiB boxes get ~1 GiB, 16 GiB ~2 GiB and 32 GiB 4 GiB, but a machine with
/// little free memory gets a smaller cache instead of swapping.
pub fn fitted_dbcache_mib(total_mib: u64, available_mib: u64) -> u64 {
    let total = total_mib.max(512);
    let available = if available_mib == 0 {
        total
    } else {
        available_mib
    };
    let total_cap = (total / 8).clamp(450, 4096).min((total / 3).max(300));
    total_cap.min((available / 2).max(300))
}

pub fn memory_budget(cfg: &Config) -> MemoryBudget {
    let mem = memory_info();
    let total = mem.total_mib.unwrap_or(1024);
    let core_reserve = if cfg.core.manage == crate::config::CoreManage::External {
        0
    } else {
        fitted_dbcache_mib(total, mem.available_mib)
    };
    budget_with(
        cfg,
        total,
        mem.available_mib,
        mem.cached_mib,
        None,
        None,
        core_reserve,
    )
}

#[allow(dead_code)]
pub fn budget_from(
    cfg: &Config,
    total_mib: u64,
    available_mib: u64,
    cached_mib: u64,
) -> MemoryBudget {
    budget_with(cfg, total_mib, available_mib, cached_mib, None, None, 0)
}

pub fn budget_with(
    cfg: &Config,
    total_mib: u64,
    available_mib: u64,
    cached_mib: u64,
    reserve_override: Option<u64>,
    memtables_override: Option<u64>,
    core_reserve_mib: u64,
) -> MemoryBudget {
    let total_mib = total_mib.max(128);
    // `MemAvailable` is the OS's own estimate of what can be allocated
    // without swapping. Plan from it, not from installed RAM: an 8 GiB box
    // running other services may only have 4 GiB to give.
    let available_mib = if available_mib == 0 {
        total_mib
    } else {
        available_mib
    };
    let core_reserve_mib = core_reserve_mib.min(available_mib.saturating_sub(128));
    let effective_mib = total_mib
        .min(available_mib.saturating_sub(core_reserve_mib))
        .max(128);

    let reserve_mib = reserve_override.unwrap_or_else(|| {
        if cfg.index.memory_reserve_mib > 0 {
            cfg.index.memory_reserve_mib
        } else {
            auto_reserve_mib(effective_mib)
        }
    });
    let usable = effective_mib.saturating_sub(reserve_mib).max(128);
    // An explicit fraction is a hard request: never raised to satisfy floors.
    let fraction = if cfg.index.memory_fraction > 0.0 {
        cfg.index.memory_fraction.clamp(0.1, 1.0)
    } else {
        0.75
    };
    let ceiling_mib = ((effective_mib as f64 * fraction) as u64)
        .min(usable)
        .max((MIN_RUNTIME_MIB + MIN_CACHE_MIB).min(usable));

    let runtime_mib = (effective_mib / 32)
        .clamp(MIN_RUNTIME_MIB, 1024)
        .min((ceiling_mib / 4).max(MIN_RUNTIME_MIB));

    let mut memtables_mib = memtables_override.unwrap_or_else(|| {
        if cfg.index.write_buffer_mib > 0 {
            cfg.index.write_buffer_mib
        } else {
            (effective_mib / 16).clamp(512, 2048)
        }
    });
    // Leave at least a small cache before honouring the memtable size.
    let room_for_mem = ceiling_mib
        .saturating_sub(runtime_mib)
        .saturating_sub(MIN_CACHE_MIB);
    if memtables_mib > room_for_mem {
        memtables_mib = room_for_mem;
    }
    let room = ceiling_mib
        .saturating_sub(runtime_mib)
        .saturating_sub(memtables_mib);

    // The profile cap is a ceiling, not an allocation: at 8 GiB the cache may
    // reach 2 GiB, at 16 GiB 4 GiB and at 32 GiB 8 GiB, but only when the OS
    // actually has that memory available.
    let auto_cap = (total_mib / 4).max(1024);
    let cache_max_mib = if cfg.index.cache_mib > 0 {
        cfg.index.cache_mib.min(room)
    } else {
        room.min(auto_cap)
    };
    let cache_start_mib = if cfg.index.cache_mib > 0 {
        cache_max_mib
    } else {
        (effective_mib / 4).min(cache_max_mib)
    };
    let cache_floor_mib = (effective_mib / 8).clamp(512, 2048).min(cache_max_mib);
    let page_cache_floor_mib = (effective_mib / 16).clamp(512, 4096);
    let planned_mib = runtime_mib
        .saturating_add(memtables_mib)
        .saturating_add(cache_start_mib);
    MemoryBudget {
        total_mib,
        available_mib,
        effective_mib,
        core_reserve_mib,
        cached_mib,
        reserve_mib,
        ceiling_mib,
        memtables_mib,
        runtime_mib,
        cache_max_mib,
        cache_start_mib,
        cache_floor_mib,
        page_cache_floor_mib,
        planned_mib,
        over_budget: planned_mib > ceiling_mib
            || planned_mib.saturating_add(reserve_mib) > effective_mib,
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

impl CacheAction {
    /// Short label for the cache-controller debug line.
    pub fn name(self) -> &'static str {
        match self {
            CacheAction::Grow => "grow",
            CacheAction::Hold => "hold",
            CacheAction::Shrink => "shrink",
        }
    }
}

/// One sample of the hot memory policy. `cached_mib` is page cache: growing the
/// RocksDB cache evicts it, so it is the guard that keeps block reads fast.
pub fn cache_action(
    available_mib: u64,
    reserve_mib: u64,
    cache_mib: u64,
    cache_max_mib: u64,
    grow_margin_mib: u64,
    prefetch_ms: u64,
) -> CacheAction {
    // `MemAvailable` already accounts for reclaimable page cache, so it is the
    // right signal. The reserve is the only policy floor: adding the
    // page-cache floor on top double-counted it and caused the 2048<->3997
    // oscillation seen on a 32 GiB mainnet host.
    if cache_mib > cache_max_mib || available_mib < reserve_mib {
        CacheAction::Shrink
    } else if cache_mib < cache_max_mib
        && available_mib
            >= reserve_mib
                .saturating_add(cache_mib)
                .saturating_add(grow_margin_mib)
        && prefetch_ms >= 50
    {
        // Only grow when prevout lookups are actually slow and the OS has
        // room for the next step plus a margin; free RAM alone is not a
        // reason to enlarge the cache.
        CacheAction::Grow
    } else {
        CacheAction::Hold
    }
}

/// `rotational` hint for the device holding `path`. Falls back to false.
/// Uses mountinfo's major:minor and walks sysfs to the parent block device, so
/// partitions and device-mapper volumes are handled too.
#[allow(dead_code)]
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
    fn zfs_arc_size_reads_the_value_column() {
        // Realistic kstat layout: header rows, then `name type value`.
        let text = "2 3 0x01 93 23200 23200 9223372036854775807 0 2.2.2\n\
                    name                            type data\n\
                    hits                            4    12345\n\
                    size                            4    4294967296\n";
        assert_eq!(parse_zfs_arc_mib(text), Some(4096));
        assert_eq!(parse_zfs_arc_mib("no size here\n"), None);
    }

    #[test]
    fn parse_arc_counters_and_psi() {
        let arc = "size                            4    21474836480\n\
                   hits                            4    100\n\
                   misses                          4    25\n";
        assert_eq!(parse_zfs_arc_counters(arc), Some((100, 25)));
        let psi = "some avg10=1.23 avg60=0.45 avg300=0.12 total=123\n\
                   full avg10=0.00 avg60=0.10 avg300=0.02 total=4\n";
        assert_eq!(parse_io_pressure(psi), Some((1.23, 0.45, 0.0, 0.10)));
        assert_eq!(parse_io_pressure("some avg10=1.0\n"), None);
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
    fn meminfo_parser_reads_free_and_page_cache() {
        let text =
            "MemTotal: 8131584 kB\nMemFree: 163840 kB\nBuffers: 10240 kB\nCached: 2772992 kB\n";
        assert_eq!(meminfo_from(text, "MemFree"), Some(160));
        assert_eq!(meminfo_from(text, "Buffers"), Some(10));
        assert_eq!(meminfo_from(text, "Cached"), Some(2708));
    }

    #[test]
    fn cpu_detection_is_sane() {
        let c = CpuInfo::detect();
        assert!(c.logical >= 1);
        assert!(c.physical >= 1);
        assert!(c.physical <= c.logical);
        assert!(c.effective >= 1);
        assert!(c.physical_effective <= c.effective);
    }

    #[test]
    fn disk_detection_returns_a_class_and_size() {
        let d = DiskInfo::detect(std::path::Path::new("."));
        assert!(!d.mount.is_empty());
        assert!(d.total_bytes > 0);
        assert!(!d.kind.name().is_empty());
    }

    #[test]
    fn machine_plan_maps_4c8t_sata_to_bounded_values() {
        let cpu = CpuInfo {
            physical: 4,
            logical: 8,
            effective: 8,
            physical_effective: 4,
            smt: true,
            quota_cpus: None,
            source: "test".to_string(),
        };
        let mem = MemoryInfo {
            total_mib: Some(32_768),
            available_mib: 16_384,
            host_total_mib: Some(32_768),
            host_available_mib: 16_384,
            cgroup_limit_mib: None,
            cgroup_used_mib: None,
            reclaimable_mib: None,
            free_mib: 2048,
            cached_mib: 8192,
            sreclaimable_mib: 512,
            shmem_mib: 128,
            arc_mib: None,
            source: LimitSource::Host,
            cgroup_detected: false,
            cgroup_unlimited: false,
        };
        let disk = DiskInfo {
            kind: DiskKind::SataSsd,
            device: Some("sda".to_string()),
            dev: Some("8:0".to_string()),
            mount: "/".to_string(),
            fstype: "ext4".to_string(),
            mount_source: "/dev/sda1".to_string(),
            free_bytes: 100,
            total_bytes: 200,
            source: "test".to_string(),
        };
        let p = MachinePlan::from_parts(cpu, mem, disk.clone(), disk, &Config::default());
        assert_eq!(p.readers, 4);
        // Same rule as the profile reload: fast disks give compaction half
        // the physical cores; `physical/2` left a 4-core host with one
        // compaction thread and a growing compaction debt.
        assert_eq!(p.jobs, 4);
        assert_eq!(p.core_par, 3);
        assert_eq!(p.core_rpcthreads, 4);
        assert_eq!(p.core_dbcache_mib, 4096);
        assert_eq!(p.cache_mib, 8064);
        assert_eq!(p.cache_start_mib, 3072);
        assert_eq!(p.commit_ms, 30_000);
        assert_eq!(p.reserve_mib, 2048);
        assert_eq!(p.page_cache_floor_mib, 768);
        assert_eq!(p.effective_mib, 12_288);
        assert_eq!(p.max_group_bytes, 256 * 1024 * 1024);
        assert!(p.same_device);
    }

    #[test]
    fn memory_budget_fits_typical_8_and_16_gib_machines() {
        let cfg = Config::default();
        // 8 GiB box, 6 GiB available, 1 GiB reserved for the Core e-lite starts.
        let b = budget_with(&cfg, 8192, 6144, 2708, None, None, 1024);
        assert_eq!(b.effective_mib, 5120);
        assert_eq!(b.reserve_mib, 2048);
        assert_eq!(b.memtables_mib, 512);
        assert_eq!(b.cache_max_mib, 2048);
        assert_eq!(b.cache_start_mib, 1280);
        assert!(b.planned_mib.saturating_add(b.reserve_mib) <= b.effective_mib);
        assert!(!b.over_budget);

        // 4 GiB-class network: 3.5 GiB available, 512 MiB for Core.
        let b = budget_with(&cfg, 4096, 3584, 1024, None, None, 512);
        assert_eq!(b.effective_mib, 3072);
        assert!(b.planned_mib.saturating_add(b.reserve_mib) <= b.effective_mib);
        assert!(!b.over_budget);

        // 16 GiB box, 12 GiB available, 2 GiB for Core.
        let b = budget_with(&cfg, 16384, 12288, 4096, None, None, 2048);
        assert_eq!(b.effective_mib, 10240);
        assert_eq!(b.memtables_mib, 640);
        assert_eq!(b.cache_max_mib, 4096);
        assert_eq!(b.cache_start_mib, 2560);
        assert!(b.planned_mib <= b.ceiling_mib);
    }

    #[test]
    fn machine_plan_has_bounded_knobs() {
        let cfg = Config::default();
        let p = MachinePlan::detect(&cfg, Chain::Regtest);
        assert!(p.readers >= 1);
        assert!(p.jobs >= 1);
        assert!(p.cache_mib >= 8);
        assert!(p.commit_ms >= 500);
        assert!(p.batch_blocks >= 16);
        assert!(p.page_cache_floor_mib >= 256);
        assert!(p.core_par >= 1);
        assert!(!p.describe().is_empty());
    }

    #[test]
    fn memory_budget_scales_with_available_ram_and_respects_overrides() {
        let mut cfg = Config::default();
        let b = budget_from(&cfg, 7941, 2583, 2708);
        // 8 GiB class with only 2.5 GiB available: the plan fits that, not the
        // installed 7.9 GiB.
        assert_eq!(b.effective_mib, 2583);
        assert_eq!(b.reserve_mib, 2048);
        assert_eq!(b.ceiling_mib, 535);
        assert!(b.memtables_mib <= 512);
        assert!(b.page_cache_floor_mib >= 256);
        assert!(b.cache_max_mib >= b.cache_start_mib);

        cfg.index.memory_fraction = 0.5;
        cfg.index.memory_reserve_mib = 2048;
        cfg.index.write_buffer_mib = 512;
        let b = budget_from(&cfg, 8192, 3000, 1000);
        assert_eq!(b.effective_mib, 3000);
        assert_eq!(b.reserve_mib, 2048);
        assert_eq!(b.ceiling_mib, 952);
        assert_eq!(b.memtables_mib, 512);

        // Manual cache is a maximum, not a fixed value.
        cfg.index.cache_mib = 128;
        let b = budget_from(&cfg, 8192, 3000, 1000);
        assert_eq!(b.cache_max_mib, 128);
        assert_eq!(b.cache_start_mib, 128);
    }

    #[test]
    fn the_plan_always_fits_inside_the_ceiling() {
        let mut cfg = Config::default();
        // The reported 1581 MiB case and a sweep of small/large machines.
        for total in [1024u64, 2048, 3800, 4096, 7907, 8192, 16384, 32768, 65536] {
            for pct in 10..=100u64 {
                cfg.index.memory_fraction = pct as f64 / 100.0;
                let b = budget_from(&cfg, total, total / 2, total / 8);
                assert!(
                    b.planned_mib <= b.ceiling_mib,
                    "total {total} fraction {pct}%: planned {} > ceiling {}",
                    b.planned_mib,
                    b.ceiling_mib
                );
                assert!(!b.over_budget);
            }
        }
        // The exact case from the report: 20% of the *effective* 4000 MiB is
        // a hard request and must not be raised to fit memtables.
        cfg.index.memory_fraction = 0.20;
        let b = budget_from(&cfg, 7907, 4000, 2000);
        assert_eq!(b.ceiling_mib, 800);
        assert!(b.planned_mib <= 800, "planned {}", b.planned_mib);
    }

    #[test]
    fn the_minimum_ram_rule_rejects_small_machines() {
        assert!(ram_requirement_error(7_400, Chain::Mainnet).is_some());
        assert!(ram_requirement_error(7_600, Chain::Mainnet).is_none());
        // The user-facing classes: 8 GB mainnet, 4 GB for every other network.
        assert!(ram_requirement_error(3_700, Chain::Regtest).is_some());
        assert!(ram_requirement_error(3_900, Chain::Regtest).is_none());
        assert!(ram_requirement_error(3_900, Chain::Signet).is_none());
        assert!(ram_requirement_error(3_900, Chain::Testnet4).is_none());
        let msg = ram_requirement_error(2_000, Chain::Testnet4).unwrap();
        assert!(msg.contains("4 GB class"), "{msg}");
    }

    #[test]
    fn minimum_ram_matches_the_supported_classes() {
        // 8 GiB machines report slightly less than 8192 MiB.
        assert_eq!(minimum_ram_mib(Chain::Mainnet), 7_500);
        assert_eq!(minimum_ram_mib(Chain::Regtest), 3_800);
        assert_eq!(minimum_ram_mib(Chain::Signet), 3_800);
        assert_eq!(minimum_ram_mib(Chain::Testnet4), 3_800);
    }

    #[test]
    fn dynamic_reserve_shrinks_with_available_memory() {
        // Available 2.87 GiB (the log): reserve falls from 3 GiB to 1.4 GiB.
        // The reserve floors at 1 GiB, not at 512 MiB: below that the OS and
        // the page cache need priority.
        for startup in [3_066u64, 4_096, 999_999] {
            assert_eq!(dynamic_reserve_for(startup, 2_870), 1_435);
            assert_eq!(dynamic_reserve_for(startup, 1_000), 1_024);
            assert_eq!(dynamic_reserve_for(startup, 0), 1_024);
        }
        assert_eq!(dynamic_reserve_for(3_066, 6_900), 3_066);
        assert_eq!(dynamic_reserve_for(4_096, 6_900), 3_450);
        // An operator reserve can still be lower than available/2.
        assert_eq!(dynamic_reserve_for(1_024, 6_900), 1_024);
    }

    #[test]
    fn cache_action_follows_available_and_stays_stable() {
        // Slow prefetch and room above reserve + cache + margin: grow.
        assert_eq!(
            cache_action(5000, 1024, 512, 2000, 512, 300),
            CacheAction::Grow
        );
        // Free RAM alone is not a reason to grow.
        assert_eq!(
            cache_action(5000, 1024, 512, 2000, 512, 5),
            CacheAction::Hold
        );
        // Below the OS reserve: shrink even if the cache is at its floor.
        assert_eq!(
            cache_action(500, 1024, 512, 2000, 512, 300),
            CacheAction::Shrink
        );
        // A dynamic ceiling below the current cache forces a shrink.
        assert_eq!(
            cache_action(5000, 1024, 2048, 1500, 512, 300),
            CacheAction::Shrink
        );
        // At the cap: hold.
        assert_eq!(
            cache_action(5000, 1024, 2000, 2000, 512, 300),
            CacheAction::Hold
        );
        // Between the reserve and the growth headroom: hold instead of
        // oscillating.
        assert_eq!(
            cache_action(2000, 1024, 512, 2000, 1024, 300),
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

    #[test]
    fn zfs_and_overlay_are_classified_explicitly() {
        assert_eq!(disk_kind_for("sda", "zfs").0, DiskKind::Zfs);
        assert_eq!(disk_kind_for("sda", "overlay").0, DiskKind::Overlay);
        assert_eq!(
            disk_kind_for("e-lite-no-such-device", "ext4").0,
            DiskKind::SataSsd
        );
    }

    #[test]
    fn disk_identity_prefers_kernel_device_number() {
        let mut base = DiskInfo {
            kind: DiskKind::SataSsd,
            device: Some("sda".to_string()),
            dev: Some("8:1".to_string()),
            mount: "/data".to_string(),
            fstype: "ext4".to_string(),
            mount_source: "/dev/sda1".to_string(),
            free_bytes: 1,
            total_bytes: 2,
            source: "test".to_string(),
        };
        let mut core = base.clone();
        core.mount = "/data/core".to_string();
        core.mount_source = "/dev/sda2".to_string();
        assert!(disk_same(&base, &core), "same parent device");

        core.dev = Some("8:2".to_string());
        assert!(
            disk_same(&base, &core),
            "two partitions of one physical disk are shared"
        );

        // Without resolved device names, fall back to raw major:minor.
        base.device = None;
        core.device = None;
        assert!(!disk_same(&base, &core), "different partitions");
        core.dev = base.dev.clone();
        assert!(disk_same(&base, &core), "same raw device");
    }
}
