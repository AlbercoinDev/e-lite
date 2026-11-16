//! Machine detection: CPU, disks and the startup plan derived from them.

use super::*;

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
    #[cfg(test)]
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
        let core_reserve_mib = core_reserve_mib.min(mem.available_mib.saturating_sub(128));
        // Report the Core cache this plan actually reserves: the caller already
        // resolved it (operator `-dbcache` or fitted). Reporting the fitted
        // value made the same log show two different `dbcache` numbers.
        let core_dbcache_mib = if cfg.core.manage == crate::config::CoreManage::External {
            0
        } else {
            core_reserve_mib
        };
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

#[cfg(test)]
mod tests {
    use super::*;

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
