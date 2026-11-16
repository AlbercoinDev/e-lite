//! Host and cgroup memory, ZFS ARC readings and the memory budget.

use super::*;

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
pub(super) fn zfs_arc_mib() -> Option<u64> {
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
pub(super) struct ArcManagerState {
    /// None = not probed yet. The probe writes the *current* value back, so
    /// it changes nothing when it succeeds.
    pub(super) writable: Option<bool>,
    /// Cap actually written by this run, so unchanged targets are skipped.
    pub(super) last_written_mib: Option<u64>,
    pub(super) last_change: Option<Instant>,
    /// Last permission probe (a negative result is retried, so the operator
    /// can fix the sysfs permissions without restarting e-lite).
    pub(super) last_probe: Option<Instant>,
}

pub(super) const ARC_PARAM_PATH: &str = "/sys/module/zfs/parameters/zfs_arc_max";

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

/// Warn about an apparently uncapped ZFS ARC (more than a quarter of RAM).
/// Returns true when it printed, so callers can rate-limit. `prefix` is the
/// log prefix. The advice points at `e-lite arc-setup`, which is the only way
/// the dynamic manager can raise/lower the cap; a fixed cap stays valid.
pub fn warn_uncapped_arc(mem: &MemoryInfo, prefix: &str) -> bool {
    let (Some(arc), Some(total)) = (mem.arc_mib, mem.total_mib) else {
        return false;
    };
    if total == 0 || arc * 4 <= total {
        return false;
    }
    let pct = arc * 100 / total;
    eprintln!(
        "{prefix} ZFS ARC is using {arc} MiB ({pct}% of RAM) and no cap was found; it is \
         invisible to MemAvailable and squeezes the cache plan. Let e-lite manage it: \
         `e-lite arc-setup` (as root) makes zfs_arc_max writable and installs \
         e-lite-arc-cap.service, so the cap follows half of RAM while Core downloads or the \
         index drains and zfs_arc_min_mib (1024) once fully synced. A fixed cap also works: \
         zfs_arc_max=<bytes> in /etc/modprobe.d",
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
pub(super) fn dynamic_reserve_for(startup_reserve_mib: u64, available_mib: u64) -> u64 {
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

#[cfg(test)]
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn meminfo_parser_reads_free_and_page_cache() {
        let text =
            "MemTotal: 8131584 kB\nMemFree: 163840 kB\nBuffers: 10240 kB\nCached: 2772992 kB\n";
        assert_eq!(meminfo_from(text, "MemFree"), Some(160));
        assert_eq!(meminfo_from(text, "Buffers"), Some(10));
        assert_eq!(meminfo_from(text, "Cached"), Some(2708));
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
}
