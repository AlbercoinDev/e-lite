mod config;
mod core_plan;
mod core_probe;
mod core_supervisor;
mod ibd_lock;
mod logging;
mod privacy;
mod public_servers;
mod snapshot;
mod tls;
mod tor;
mod tuning;

use config::{Config, CoreManage};
use core_supervisor::{CoreStartOutcome, CoreSupervisor};
use el_chain::ReaderTracker;
use el_index::{
    apply_prepared_deferred_undo, prefetch_prevouts, prefetch_prevouts_with_deadline,
    prepare_block_bytes, prune_undo, rewind_to, write_window_undo, GroupOverlay,
};
use el_mempool::Mempool;
use el_model::{Chain, Hash32, IndexedTip, NetworkIdentity, ScriptHash};
use el_rawtx::{CoreBlockSource, RawtxConfig, Resolver};
use el_rpc::{BlockchainInfo, CoreClient, CoreConfig};
use el_serve::{display_size, state, Shared, Stats, VERSION};
use el_store::Store;
use rocksdb::WriteBatch;
use serde_json::json;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime};
use tuning::{CommitReason, Tunables};

type E = Box<dyn std::error::Error + Send + Sync>;
type R<T> = std::result::Result<T, E>;

fn fail<T>(msg: impl Into<String>) -> R<T> {
    Err(msg.into().into())
}

fn format_local_time(t: SystemTime) -> String {
    let secs = t
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
        return format!("{secs}");
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

fn readiness_line(
    shared: &Shared,
    sync: &el_serve::SyncStatus,
    tls: &str,
    fully: bool,
    session: &str,
) -> String {
    let log = shared.log_file.display();
    if fully {
        format!(
            "[e-lite] FULLY SYNCED: height {}/{}; session {} (indexing and serving since \
             the start); Electrum {}{}; log {}",
            sync.height, sync.headers, session, shared.listen, tls, log
        )
    } else if sync.offline {
        format!(
            "[e-lite] READY (offline): cached index at height {}; Core unavailable; \
             Electrum {}{}; log {}",
            sync.height, shared.listen, tls, log
        )
    } else {
        let note = if sync.core_ibd {
            format!(
                "Core still in IBD, verify {:.1}%",
                sync.verification * 100.0
            )
        } else {
            "Core caught up".to_string()
        };
        format!(
            "[e-lite] READY: index caught up to Core at height {} ({} headers; {}; session {}); \
             Electrum {}{}; log {}",
            sync.height, sync.headers, note, session, shared.listen, tls, log
        )
    }
}

fn format_uptime(d: Duration) -> String {
    if d.as_secs() < 60 {
        return format!("{:.1}s", d.as_secs_f64());
    }
    let secs = d.as_secs();
    format!(
        "{}h {:02}m {:02}s",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Set once; SIGTERM/SIGINT request a clean stop at a commit boundary.
fn shutdown_flag() -> Arc<AtomicBool> {
    static FLAG: OnceLock<Arc<AtomicBool>> = OnceLock::new();
    FLAG.get_or_init(|| {
        let flag = Arc::new(AtomicBool::new(false));
        let _ = signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&flag));
        let _ = signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&flag));
        flag
    })
    .clone()
}

/// SIGUSR1 asks the running daemon to reload the hot `[index]`/`[pruning]`
/// settings from the same config file. Cold fields are logged, not applied.
fn reload_flag() -> Arc<AtomicBool> {
    static FLAG: OnceLock<Arc<AtomicBool>> = OnceLock::new();
    FLAG.get_or_init(|| {
        let flag = Arc::new(AtomicBool::new(false));
        let _ = signal_hook::flag::register(signal_hook::consts::SIGUSR1, Arc::clone(&flag));
        flag
    })
    .clone()
}

static CONFIG_PATH: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();

fn config_path() -> Option<std::path::PathBuf> {
    CONFIG_PATH.get().cloned().flatten()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ErrorClass {
    Permanent,
    Transient,
}

/// Errors that retrying cannot fix: wrong network, prune gap, corrupt schema...
/// A transient "Block not available (pruned data)" is *not* permanent: the lag
/// guard can pause Core and retry.
fn classify_error(msg: &str) -> ErrorClass {
    let m = msg.to_ascii_lowercase();
    let permanent = [
        "genesis mismatch",
        "needs_recovery",
        "schema",
        "txids.dat",
        "database is not initialized",
        "network mismatch",
        "signet challenge mismatch",
        "index behind pruneheight",
        "core is pruned and the index is empty",
        "core is pruned from height",
        "repair failed",
        "no repair source",
        "database belongs to",
    ]
    .iter()
    .any(|p| m.contains(p));
    if permanent {
        ErrorClass::Permanent
    } else {
        ErrorClass::Transient
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Command {
    Help,
    Version,
    Init,
    Doctor,
    Index,
    Serve,
    Run,
    Status,
    ExportSnapshot,
    ImportSnapshot,
    Core,
    Onion,
    Cache,
    Privacy,
    ArcSetup,
}

#[derive(Default)]
struct Args {
    command: Option<Command>,
    subcommand: Option<String>,
    config: Option<String>,
    public_servers: Option<String>,
    network: Option<String>,
    data_dir: Option<String>,
    rpc_url: Option<String>,
    rpc_user: Option<String>,
    rpc_password: Option<String>,
    core_datadir: Option<String>,
    blocks_dir: Option<String>,
    cookie_file: Option<String>,
    listen: Option<String>,
    tls: Option<bool>,
    plain_lan: bool,
    tls_listen: Option<String>,
    tls_cert: Option<String>,
    tls_key: Option<String>,
    metrics: Option<String>,
    allow_mainnet: bool,
    json: bool,
    no_follow: bool,
    batch: Option<u32>,
    reorg_window: Option<u32>,
    block_source: Option<String>,
    readers: Option<usize>,
    manage: Option<String>,
    prune_mib: Option<u64>,
    min_free_gib: Option<u64>,
    allow_clearnet: bool,
    allow_auto_prune: bool,
    no_rawtx: bool,
    output: Option<String>,
    input: Option<String>,
    with_core: bool,
    no_hash_files: bool,
    compress_level: Option<u32>,
    verify: Option<String>,
    expect_digest: Option<String>,
    set: Vec<String>,
    profile: Option<String>,
    adaptive: Option<bool>,
    cache_mib: Option<u64>,
    commit_interval_ms: Option<u64>,
    commit_sync: Option<String>,
    prefetch_blocks: Option<usize>,
    write_buffer_mib: Option<u64>,
    adaptive_memtables: Option<bool>,
    prefetch_deadline_ms: Option<u64>,
    max_open_files: Option<u32>,
    memory_reserve_mib: Option<u64>,
    memory_fraction: Option<f64>,
    allow_low_memory: bool,
    /// `arc-setup --apply`: run the setup as root (writes the systemd unit).
    apply: bool,
    background_jobs: Option<usize>,
    milestone_blocks: Option<u64>,
    milestone_seconds: Option<u64>,
    max_core_lag_blocks: Option<u32>,
    max_bulk_lag_blocks: Option<u32>,
    max_bulk_lag_gib: Option<u64>,
    pause_core_during_bulk: Option<bool>,
    core_resume_ahead_blocks: Option<u32>,
    core_resume_ahead_gib: Option<u64>,
    scheduler: Option<String>,
    prune_defer_free_gib: Option<u64>,
    min_gain_blocks: Option<u32>,
    prune_interval_seconds: Option<u64>,
    poll_seconds: Option<u64>,
    repair_max_gap_blocks: Option<u32>,
    critical_free_gib: Option<u64>,
    max_clients: Option<usize>,
    banner_file: Option<String>,
    core_args: Vec<String>,
    core_io_priority: Option<u8>,
    core_cgroup_io_weight: Option<u64>,
    p2p_peers: Option<String>,
    no_p2p: bool,
    no_esplora: bool,
    decoys: Option<u32>,
    rawtx_cache_mib: Option<u64>,
    tor_socks: Option<String>,
    tor_mode: Option<String>,
    tor_plain: Option<bool>,
    tor_tls: Option<bool>,
    tor_auto_cert: Option<bool>,
    tor_onion_port: Option<u16>,
    tor_onion_tls_port: Option<u16>,
    tor_socks_port: Option<u16>,
    tor_binary: Option<String>,
    tor_target_plain: Option<String>,
    tor_target_tls: Option<String>,
    no_tor: bool,
    purge: bool,
    yes: bool,
    dry_run: bool,
    stop_on_exit: Option<bool>,
    restart: Option<bool>,
    log_file: Option<String>,
    log_max_mib: Option<u64>,
    log_level: Option<String>,
    no_log_file: bool,
}

fn parse_args() -> R<Args> {
    let mut args = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(raw) = it.next() {
        let (flag, inline) = match raw.split_once('=') {
            Some((f, v)) => (f.to_string(), Some(v.to_string())),
            None => (raw.clone(), None),
        };
        let mut value = |name: &str| -> R<String> {
            if let Some(v) = inline.clone() {
                return Ok(v);
            }
            it.next()
                .ok_or_else(|| format!("{name} needs a value").into())
        };
        match flag.as_str() {
            "-h" | "--help" => args.command = Some(Command::Help),
            "--version" | "-V" => args.command = Some(Command::Version),
            "--config" => args.config = Some(value("--config")?),
            "--public-servers" => args.public_servers = Some(value("--public-servers")?),
            "--network" => args.network = Some(value("--network")?),
            "--data-dir" => args.data_dir = Some(value("--data-dir")?),
            "--core-rpc" => args.rpc_url = Some(value("--core-rpc")?),
            "--core-datadir" => args.core_datadir = Some(value("--core-datadir")?),
            "--blocks-dir" => args.blocks_dir = Some(value("--blocks-dir")?),
            "--block-source" => args.block_source = Some(value("--block-source")?),
            "--bulk-readers" | "--readers" => args.readers = Some(value("--readers")?.parse()?),
            "--cookie-file" => args.cookie_file = Some(value("--cookie-file")?),
            "--rpc-user" => args.rpc_user = Some(value("--rpc-user")?),
            "--rpc-password" => args.rpc_password = Some(value("--rpc-password")?),
            "--listen" => args.listen = Some(value("--listen")?),
            "--tls-listen" => args.tls_listen = Some(value("--tls-listen")?),
            "--tls" => args.tls = Some(true),
            "--no-tls" => args.tls = Some(false),
            "--plain-lan" => args.plain_lan = true,
            "--tls-cert" => args.tls_cert = Some(value("--tls-cert")?),
            "--tls-key" => args.tls_key = Some(value("--tls-key")?),
            "--metrics" => args.metrics = Some(value("--metrics")?),
            "--batch" => args.batch = Some(value("--batch")?.parse()?),
            "--reorg-window" => args.reorg_window = Some(value("--reorg-window")?.parse()?),
            "--manage" => args.manage = Some(value("--manage")?),
            "--prune-mib" => args.prune_mib = Some(value("--prune-mib")?.parse()?),
            "--min-free-gib" => args.min_free_gib = Some(value("--min-free-gib")?.parse()?),
            "--output" => args.output = Some(value("--output")?),
            "--input" => args.input = Some(value("--input")?),
            "--with-core" => args.with_core = true,
            // Hashing is the default (AUD-143); the old flag is accepted.
            "--hash-files" => args.no_hash_files = false,
            "--no-hash-files" => args.no_hash_files = true,
            "--compress-level" => args.compress_level = Some(value("--compress-level")?.parse()?),
            "--verify" => {
                let v = inline.clone().unwrap_or_else(|| "basic".to_string());
                if v != "basic" && v != "full" {
                    return fail(
                        "--verify takes `basic` (per-file hashes) or `full` \
                                 (hashes + a bounded verifychain on the restored Core)"
                            .to_string(),
                    );
                }
                args.verify = Some(v);
            }
            "--expect-digest" => args.expect_digest = Some(value("--expect-digest")?),
            "--set" => args.set.push(value("--set")?),
            "--profile" => args.profile = Some(value("--profile")?),
            "--cache-mib" => args.cache_mib = Some(value("--cache-mib")?.parse()?),
            "--commit-interval-ms" => {
                args.commit_interval_ms = Some(value("--commit-interval-ms")?.parse()?)
            }
            "--commit-sync" => args.commit_sync = Some(value("--commit-sync")?),
            "--prefetch-blocks" => {
                args.prefetch_blocks = Some(value("--prefetch-blocks")?.parse()?)
            }
            "--write-buffer-mib" => {
                args.write_buffer_mib = Some(value("--write-buffer-mib")?.parse()?)
            }
            "--adaptive-memtables" => args.adaptive_memtables = Some(true),
            "--no-adaptive-memtables" => args.adaptive_memtables = Some(false),
            "--prefetch-deadline-ms" => {
                args.prefetch_deadline_ms = Some(value("--prefetch-deadline-ms")?.parse()?)
            }
            "--max-open-files" => args.max_open_files = Some(value("--max-open-files")?.parse()?),
            "--memory-reserve-mib" => {
                args.memory_reserve_mib = Some(value("--memory-reserve-mib")?.parse()?)
            }
            "--memory-fraction" => {
                args.memory_fraction = Some(value("--memory-fraction")?.parse()?)
            }
            "--allow-low-memory" => args.allow_low_memory = true,
            "--apply" => args.apply = true,
            "--background-jobs" => {
                args.background_jobs = Some(value("--background-jobs")?.parse()?)
            }
            "--milestone-blocks" => {
                args.milestone_blocks = Some(value("--milestone-blocks")?.parse()?)
            }
            "--milestone-seconds" => {
                args.milestone_seconds = Some(value("--milestone-seconds")?.parse()?)
            }
            "--max-core-lag-blocks" => {
                args.max_core_lag_blocks = Some(value("--max-core-lag-blocks")?.parse()?)
            }
            "--max-bulk-lag-blocks" => {
                args.max_bulk_lag_blocks = Some(value("--max-bulk-lag-blocks")?.parse()?)
            }
            "--max-bulk-lag-gib" => {
                args.max_bulk_lag_gib = Some(value("--max-bulk-lag-gib")?.parse()?)
            }
            "--pause-core-during-bulk" => args.pause_core_during_bulk = Some(true),
            "--no-pause-core-during-bulk" => args.pause_core_during_bulk = Some(false),
            "--core-resume-ahead-blocks" => {
                args.core_resume_ahead_blocks = Some(value("--core-resume-ahead-blocks")?.parse()?)
            }
            "--core-resume-ahead-gib" => {
                args.core_resume_ahead_gib = Some(value("--core-resume-ahead-gib")?.parse()?)
            }
            "--scheduler" => args.scheduler = Some(value("--scheduler")?),
            "--prune-defer-free-gib" => {
                args.prune_defer_free_gib = Some(value("--prune-defer-free-gib")?.parse()?)
            }
            "--min-gain-blocks" => {
                args.min_gain_blocks = Some(value("--min-gain-blocks")?.parse()?)
            }
            "--prune-interval-seconds" => {
                args.prune_interval_seconds = Some(value("--prune-interval-seconds")?.parse()?)
            }
            "--poll-seconds" => args.poll_seconds = Some(value("--poll-seconds")?.parse()?),
            "--repair-max-gap-blocks" => {
                args.repair_max_gap_blocks = Some(value("--repair-max-gap-blocks")?.parse()?)
            }
            "--critical-free-gib" => {
                args.critical_free_gib = Some(value("--critical-free-gib")?.parse()?)
            }
            "--max-clients" => args.max_clients = Some(value("--max-clients")?.parse()?),
            "--banner-file" => args.banner_file = Some(value("--banner-file")?),
            "--core-arg" => args.core_args.push(value("--core-arg")?),
            "--core-io-priority" => {
                args.core_io_priority = Some(value("--core-io-priority")?.parse()?)
            }
            "--core-cgroup-io-weight" => {
                args.core_cgroup_io_weight = Some(value("--core-cgroup-io-weight")?.parse()?)
            }
            "--p2p-peers" => args.p2p_peers = Some(value("--p2p-peers")?),
            "--decoys" => args.decoys = Some(value("--decoys")?.parse()?),
            "--rawtx-cache-mib" => {
                args.rawtx_cache_mib = Some(value("--rawtx-cache-mib")?.parse()?)
            }
            "--tor-socks" => args.tor_socks = Some(value("--tor-socks")?),
            "--tor-mode" => args.tor_mode = Some(value("--tor-mode")?),
            "--tor-onion-port" => args.tor_onion_port = Some(value("--tor-onion-port")?.parse()?),
            "--tor-onion-tls-port" => {
                args.tor_onion_tls_port = Some(value("--tor-onion-tls-port")?.parse()?)
            }
            "--tor-socks-port" => args.tor_socks_port = Some(value("--tor-socks-port")?.parse()?),
            "--tor-binary" => args.tor_binary = Some(value("--tor-binary")?),
            "--tor-target-plain" => args.tor_target_plain = Some(value("--tor-target-plain")?),
            "--tor-target-tls" => args.tor_target_tls = Some(value("--tor-target-tls")?),
            "--tor-plain" => args.tor_plain = Some(true),
            "--no-tor-plain" => args.tor_plain = Some(false),
            "--tor-tls" => args.tor_tls = Some(true),
            "--no-tor-tls" => args.tor_tls = Some(false),
            "--tor-auto-cert" => args.tor_auto_cert = Some(true),
            "--no-tor-auto-cert" => args.tor_auto_cert = Some(false),
            "--no-tor" => args.no_tor = true,
            "--purge" => args.purge = true,
            "--yes" | "-y" => args.yes = true,
            "--dry-run" => args.dry_run = true,
            "--adaptive" => args.adaptive = Some(true),
            "--no-adaptive" => args.adaptive = Some(false),
            "--no-p2p" => args.no_p2p = true,
            "--no-esplora" => args.no_esplora = true,
            "--log-file" => args.log_file = Some(value("--log-file")?),
            "--log-max-mib" => args.log_max_mib = Some(value("--log-max-mib")?.parse()?),
            "--no-log-file" => args.no_log_file = true,
            "--log-level" => args.log_level = Some(value("--log-level")?),
            "--stop-on-exit" => args.stop_on_exit = Some(true),
            "--no-stop-on-exit" => args.stop_on_exit = Some(false),
            "--restart" => args.restart = Some(true),
            "--no-restart" => args.restart = Some(false),
            "--allow-mainnet" => args.allow_mainnet = true,
            "--allow-clearnet" => args.allow_clearnet = true,
            "--allow-auto-prune" => args.allow_auto_prune = true,
            "--no-rawtx" => args.no_rawtx = true,
            "--json" => args.json = true,
            "--no-follow" => args.no_follow = true,
            other if other.starts_with('-') => return fail(format!("unknown flag: {other}")),
            other => {
                if matches!(
                    args.command,
                    Some(Command::Core)
                        | Some(Command::Onion)
                        | Some(Command::Cache)
                        | Some(Command::Privacy)
                ) && args.subcommand.is_none()
                {
                    args.subcommand = Some(other.to_string());
                    continue;
                }
                if args.command.is_some() {
                    return fail(format!("unexpected argument: {other}"));
                }
                args.command = Some(match other {
                    "init" => Command::Init,
                    "doctor" => Command::Doctor,
                    "index" => Command::Index,
                    "serve" => Command::Serve,
                    "run" => Command::Run,
                    "status" => Command::Status,
                    "core" => Command::Core,
                    "onion" => Command::Onion,
                    "cache" => Command::Cache,
                    "privacy" => Command::Privacy,
                    "export-snapshot" => Command::ExportSnapshot,
                    "import-snapshot" => Command::ImportSnapshot,
                    "arc-setup" => Command::ArcSetup,
                    "version" => Command::Version,
                    "help" => Command::Help,
                    _ => return fail(format!("unknown command: {other}")),
                });
            }
        }
    }
    Ok(args)
}

fn effective_config(args: &Args) -> R<(Config, Chain)> {
    effective_config_with(args, true)
}

/// `probe_core = false` is for maintenance commands that only need the data
/// directory (`cache`): resolving `network = auto` must not wait on Core or
/// open the database (which a running daemon holds locked).
fn effective_config_with(args: &Args, probe_core: bool) -> R<(Config, Chain)> {
    // `--set privacy.*` belongs to privacy.toml, not to config.toml.
    let (privacy_sets, config_sets): (Vec<String>, Vec<String>) = args
        .set
        .iter()
        .cloned()
        .partition(|s| s.trim_start().starts_with("privacy."));
    let mut cfg = Config::load_with_overrides(args.config.as_deref(), &config_sets)?;
    cfg.rawtx.privacy_overrides = privacy_sets;
    if let Some(n) = &args.network {
        cfg.network.name = n.clone();
    }
    if let Some(p) = &args.public_servers {
        cfg.public_servers.file = Some(p.clone());
    }
    if let Some(d) = &args.data_dir {
        cfg.network.data_dir = Some(d.clone());
    }
    if let Some(r) = &args.rpc_url {
        cfg.core.rpc_url = Some(r.clone());
    }
    if let Some(u) = &args.rpc_user {
        cfg.core.rpc_user = Some(u.clone());
    }
    if let Some(p) = &args.rpc_password {
        cfg.core.rpc_password = Some(p.clone());
    }
    if let Some(d) = &args.core_datadir {
        cfg.core.core_datadir = Some(d.clone());
    }
    if let Some(d) = &args.blocks_dir {
        cfg.core.blocks_dir = Some(d.clone());
    }
    if let Some(s) = &args.block_source {
        cfg.index.block_source = s.clone();
    }
    if let Some(n) = args.readers {
        cfg.index.readers = n;
    }
    if let Some(c) = &args.cookie_file {
        cfg.core.cookie_file = Some(c.clone());
    }
    if let Some(l) = &args.listen {
        cfg.server.listen = l.clone();
    }
    if let Some(v) = args.tls {
        cfg.server.tls = v;
    }
    if args.plain_lan {
        cfg.server.plain_lan = true;
    }
    if let Some(l) = &args.tls_listen {
        cfg.server.tls_listen = l.clone();
    }
    if let Some(c) = &args.tls_cert {
        cfg.server.tls_cert = c.clone();
    }
    if let Some(k) = &args.tls_key {
        cfg.server.tls_key = k.clone();
    }
    if let Some(m) = &args.metrics {
        cfg.metrics.listen = m.clone();
    }
    if let Some(b) = args.batch {
        cfg.index.batch_blocks = b.max(1);
    }
    if let Some(w) = args.reorg_window {
        cfg.index.reorg_window = w.max(1);
    }
    if let Some(m) = &args.manage {
        cfg.core.manage = match m.as_str() {
            "external" => CoreManage::External,
            "auto" => CoreManage::Auto,
            "managed" => CoreManage::Managed,
            other => {
                return fail(format!(
                    "unknown manage mode \"{other}\": use external, auto or managed"
                ))
            }
        };
    }
    if let Some(p) = args.prune_mib {
        cfg.core.prune_mib = p;
    }
    if let Some(g) = args.min_free_gib {
        cfg.pruning.min_free_gib = g.max(1);
    }
    if args.allow_clearnet {
        cfg.rawtx.allow_clearnet = true;
    }
    if args.allow_auto_prune {
        cfg.pruning.allow_auto_prune = true;
    }
    if args.no_rawtx {
        cfg.rawtx.enabled = false;
    }
    if args.no_follow {
        cfg.index.follow = false;
    }
    if args.allow_mainnet {
        cfg.safety.allow_mainnet = true;
    }
    if let Some(p) = &args.profile {
        cfg.index.profile = p.clone();
    }
    if let Some(a) = args.adaptive {
        cfg.index.adaptive = a;
    }
    if let Some(v) = args.cache_mib {
        cfg.index.cache_mib = v;
    }
    if let Some(v) = args.commit_interval_ms {
        cfg.index.commit_interval_ms = v;
    }
    if let Some(v) = &args.commit_sync {
        cfg.index.commit_sync = v.clone();
    }
    if let Some(v) = args.prefetch_blocks {
        cfg.index.prefetch_blocks = v;
    }
    if let Some(v) = args.write_buffer_mib {
        cfg.index.write_buffer_mib = v;
    }
    if let Some(v) = args.adaptive_memtables {
        cfg.index.adaptive_memtables = v;
    }
    if let Some(v) = args.prefetch_deadline_ms {
        cfg.index.prefetch_deadline_ms = v;
    }
    if let Some(v) = args.max_open_files {
        cfg.index.max_open_files = v;
    }
    if let Some(v) = args.memory_reserve_mib {
        cfg.index.memory_reserve_mib = v;
    }
    if let Some(v) = args.memory_fraction {
        cfg.index.memory_fraction = v;
    }
    if args.allow_low_memory {
        cfg.index.allow_low_memory = true;
    }
    if let Some(v) = args.background_jobs {
        cfg.index.background_jobs = v;
    }
    if let Some(v) = args.milestone_blocks {
        cfg.index.milestone_blocks = v;
    }
    if let Some(v) = args.milestone_seconds {
        cfg.index.milestone_seconds = v;
    }
    if let Some(v) = args.max_core_lag_blocks {
        cfg.pruning.max_core_lag_blocks = v;
    }
    if let Some(v) = args.max_bulk_lag_blocks {
        cfg.pruning.max_bulk_lag_blocks = v;
    }
    if let Some(v) = args.max_bulk_lag_gib {
        cfg.pruning.max_bulk_lag_gib = v;
    }
    if let Some(v) = args.pause_core_during_bulk {
        cfg.pruning.pause_core_during_bulk = v;
    }
    if let Some(v) = args.core_resume_ahead_blocks {
        cfg.pruning.core_resume_ahead_blocks = v;
    }
    if let Some(v) = args.core_resume_ahead_gib {
        cfg.pruning.core_resume_ahead_gib = v;
    }
    if let Some(v) = args.scheduler.clone() {
        cfg.pruning.scheduler = v;
    }
    if let Some(v) = args.prune_defer_free_gib {
        cfg.pruning.prune_defer_free_gib = v;
    }
    if let Some(v) = args.min_gain_blocks {
        cfg.pruning.min_gain_blocks = v;
    }
    if let Some(v) = args.prune_interval_seconds {
        cfg.pruning.prune_interval_seconds = v;
    }
    if let Some(v) = args.poll_seconds {
        cfg.pruning.poll_seconds = v;
    }
    if let Some(v) = args.repair_max_gap_blocks {
        cfg.pruning.repair_max_gap_blocks = v;
    }
    if let Some(v) = args.critical_free_gib {
        cfg.pruning.critical_free_gib = v;
    }
    if let Some(v) = args.max_clients {
        cfg.server.max_clients = v;
    }
    if let Some(v) = &args.banner_file {
        cfg.server.banner_file = v.clone();
    }
    if !args.core_args.is_empty() {
        cfg.core.extra_args.extend(args.core_args.iter().cloned());
    }
    if let Some(v) = args.core_io_priority {
        cfg.core.io_priority = Some(v);
    }
    if let Some(v) = args.core_cgroup_io_weight {
        cfg.core.cgroup_io_weight = Some(v);
    }
    if let Some(v) = &args.p2p_peers {
        cfg.rawtx.p2p_peers = v
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }
    if args.no_p2p {
        cfg.rawtx.use_p2p = false;
    }
    if args.no_esplora {
        cfg.rawtx.use_esplora = false;
    }
    if let Some(v) = args.decoys {
        cfg.rawtx.decoys = v;
    }
    if let Some(v) = args.rawtx_cache_mib {
        cfg.rawtx.cache_mib = v;
    }
    if let Some(v) = &args.tor_socks {
        cfg.rawtx.tor_socks = Some(v.clone());
    }
    if let Some(v) = &args.tor_mode {
        cfg.tor.mode = v.clone();
    }
    if let Some(v) = args.tor_plain {
        cfg.tor.plain = v;
    }
    if let Some(v) = args.tor_tls {
        cfg.tor.tls = v;
    }
    if let Some(v) = args.tor_auto_cert {
        cfg.tor.tls_auto_cert = v;
    }
    if let Some(v) = args.tor_onion_port {
        cfg.tor.onion_port = v;
    }
    if let Some(v) = args.tor_onion_tls_port {
        cfg.tor.onion_tls_port = v;
    }
    if let Some(v) = args.tor_socks_port {
        cfg.tor.socks_port = v;
    }
    if let Some(v) = &args.tor_binary {
        cfg.tor.binary = v.clone();
    }
    if let Some(v) = &args.tor_target_plain {
        cfg.tor.target_plain = v.clone();
    }
    if let Some(v) = &args.tor_target_tls {
        cfg.tor.target_tls = v.clone();
    }
    if args.no_tor {
        cfg.tor.mode = "off".to_string();
    }
    if let Some(v) = args.stop_on_exit {
        cfg.core.stop_on_exit = v;
    }
    if let Some(v) = args.restart {
        cfg.core.restart = v;
    }
    if let Some(p) = &args.log_file {
        cfg.logging.file = Some(p.clone());
    }
    if let Some(v) = args.log_max_mib {
        cfg.logging.max_mib = v;
    }
    if let Some(v) = &args.log_level {
        cfg.logging.level = v.clone();
    }
    if cfg.index.block_source != "auto"
        && cfg.index.block_source != "rpc"
        && cfg.index.block_source != "files"
    {
        return fail(format!(
            "unknown block source \"{}\": use auto, rpc or files",
            cfg.index.block_source
        ));
    }
    let chain = if probe_core {
        resolve_chain(&cfg)?
    } else {
        resolve_chain_offline(&cfg)?
    };
    // Listener/metrics/TLS defaults depend on the network; resolve them before
    // validation and binding.
    cfg.apply_network_defaults(chain);
    cfg.validate()?;
    // Never start a fresh Core datadir while an existing one holds this chain's
    // data, and never guess when two candidates exist.
    if cfg.core.core_datadir.is_none() && cfg.network.data_dir.is_none() {
        let preferred = config::home().join("e-lite").join(chain.name());
        let data = cfg.data_dir(chain);
        if data != preferred {
            eprintln!(
                "[e-lite] WARNING: using legacy data dir {}; move it to {} to use the \
                 per-network layout",
                data.display(),
                preferred.display()
            );
        }
    }
    if cfg.core.core_datadir.is_none() {
        let current = cfg.data_dir(chain).join("bitcoin");
        if !current.exists() {
            match cfg.legacy_core_datadirs(chain).as_slice() {
                [] => {}
                [only] => eprintln!(
                    "[e-lite] WARNING: using legacy Core datadir {}; move it to {} so each \
                     network keeps its own Core",
                    only.display(),
                    current.display()
                ),
                many => {
                    return fail(format!(
                        "several legacy Core datadirs contain {} data ({}); set \
                         [core] core_datadir (or --core-datadir) to the one to use",
                        chain.name(),
                        many.iter()
                            .map(|p| p.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
            }
        }
    }
    if chain.requires_explicit_opt_in() && !cfg.safety.allow_mainnet {
        return fail(
            "mainnet requires explicit opt-in: pass --allow-mainnet or set [safety] allow_mainnet = true",
        );
    }
    let rpc_url = cfg.rpc_url(chain);
    if !el_rpc::rpc_url_is_loopback(&rpc_url) {
        if cfg.core.require_loopback_rpc {
            return fail(format!(
                "[core] require_loopback_rpc = true, but rpc_url {rpc_url} is not loopback. \
                 Use an SSH/VPN tunnel and point rpc_url at 127.0.0.1, or disable the strict flag."
            ));
        }
        eprintln!(
            "[e-lite] WARNING: Core RPC is {rpc_url} (non-loopback) over cleartext HTTP. \
             The user/password cross the LAN/WAN in clear text; use an SSH/VPN tunnel. \
             Set [core] require_loopback_rpc = true to make this fatal."
        );
    }
    Ok((cfg, chain))
}

fn chain_from_core(name: &str) -> Option<Chain> {
    match name {
        "main" | "mainnet" => Some(Chain::Mainnet),
        "testnet4" => Some(Chain::Testnet4),
        "signet" => Some(Chain::Signet),
        "regtest" => Some(Chain::Regtest),
        _ => None,
    }
}

/// Resolve `network = "auto"` from Core, or from an existing database when Core
/// is unavailable. Launching Core always needs an explicit network.
fn resolve_chain(cfg: &Config) -> R<Chain> {
    if cfg.network.name != "auto" {
        return Ok(cfg.chain()?);
    }
    let detect_timeout = Duration::from_secs(cfg.core.timeout_seconds.clamp(1, 3));
    for chain in [
        Chain::Mainnet,
        Chain::Testnet4,
        Chain::Signet,
        Chain::Regtest,
    ] {
        let Ok(client) = core_client_with_timeout(cfg, chain, detect_timeout) else {
            continue;
        };
        if let Ok(info) = client.blockchain_info() {
            if let Some(chained) = chain_from_core(&info.chain) {
                return Ok(chained);
            }
        }
    }
    if let Some(chained) = detect_chain_from_db(cfg)? {
        return Ok(chained);
    }
    fail(
        "network = auto: Bitcoin Core is unavailable and no initialized database was found; \
         pass --network <mainnet|testnet4|signet|regtest> or start Core",
    )
}

fn resolve_chain_offline(cfg: &Config) -> R<Chain> {
    if cfg.network.name != "auto" {
        return Ok(cfg.chain()?);
    }
    if cfg.network.data_dir.is_some() {
        return fail(
            "network = auto with an explicit data dir: pass \
             --network <mainnet|testnet4|signet|regtest>",
        );
    }
    // No Core probe and no database open: a running daemon holds the RocksDB
    // lock, and maintenance commands only need to find the directory.
    let found: Vec<Chain> = [
        Chain::Mainnet,
        Chain::Testnet4,
        Chain::Signet,
        Chain::Regtest,
    ]
    .into_iter()
    .filter(|chain| {
        let dir = cfg.data_dir(*chain);
        dir.join("CURRENT").is_file() || dir.join(el_rawtx::CACHE_DIR_NAME).is_dir()
    })
    .collect();
    match found.as_slice() {
        [only] => Ok(*only),
        [] => fail(
            "network = auto: no initialised e-lite data directory was found; \
             pass --network <mainnet|testnet4|signet|regtest>",
        ),
        many => fail(format!(
            "network = auto matches several data directories ({}); \
             pass --network <mainnet|testnet4|signet|regtest>",
            many.iter().map(|c| c.name()).collect::<Vec<_>>().join(", ")
        )),
    }
}

fn detect_chain_from_db(cfg: &Config) -> R<Option<Chain>> {
    if cfg.network.data_dir.is_some() {
        let dir = cfg.data_dir(Chain::Mainnet);
        if !dir.join("CURRENT").is_file() {
            return Ok(None);
        }
        let store = Store::open(&dir)?;
        return Ok(store.network_identity()?.map(|id| id.chain));
    }
    let mut found = Vec::new();
    for chain in [
        Chain::Mainnet,
        Chain::Testnet4,
        Chain::Signet,
        Chain::Regtest,
    ] {
        if cfg.data_dir(chain).join("CURRENT").is_file() {
            found.push(chain);
        }
    }
    match found.len() {
        0 => Ok(None),
        1 => Ok(Some(found[0])),
        _ => fail(format!(
            "network = auto: several initialized databases found ({}); pass --network",
            found
                .iter()
                .map(|c| c.name())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn core_client_with_timeout(cfg: &Config, chain: Chain, timeout: Duration) -> R<CoreClient> {
    let cc = CoreConfig {
        rpc_url: cfg.rpc_url(chain),
        cookie_file: cfg.cookie_file(),
        core_datadir: Some(cfg.core_datadir(chain)),
        rpc_user: cfg.core.rpc_user.clone(),
        rpc_password: cfg.core.rpc_password.clone(),
        chain,
        timeout,
    };
    Ok(CoreClient::connect(&cc)?)
}

fn core_client(cfg: &Config, chain: Chain) -> R<CoreClient> {
    core_client_with_timeout(
        cfg,
        chain,
        Duration::from_secs(cfg.core.timeout_seconds.max(1)),
    )
}

fn core_identity(core: &CoreClient, chain: Chain) -> R<NetworkIdentity> {
    let info = core.blockchain_info()?;
    let core_chain = chain_from_core(&info.chain)
        .ok_or_else(|| format!("Core reports unsupported chain: {}", info.chain))?;
    if core_chain != chain {
        return fail(format!(
            "network mismatch: config says {}, Core says {}",
            chain.name(),
            core_chain.name()
        ));
    }
    let genesis = Hash32::from_hex(&core.block_hash(0)?)?;
    let signet_challenge = match (&info.signet_challenge, chain) {
        (Some(hex_challenge), Chain::Signet) => {
            let bytes = hex::decode(hex_challenge)?;
            Some(ScriptHash::from_script(&bytes).0)
        }
        _ => None,
    };
    let identity = NetworkIdentity {
        chain,
        genesis,
        signet_challenge,
    };
    identity.check_known_genesis()?;
    Ok(identity)
}

fn total_ram_mib() -> Option<u64> {
    tuning::total_ram_mib()
}

fn effective_cache_mib(cfg: &Config) -> u64 {
    tuning::initial_cache_mib(cfg)
}

fn min_free_bytes(cfg: &Config, chain: Chain) -> Option<u64> {
    match (
        el_chain::disk_free_bytes(&cfg.data_dir(chain)),
        el_chain::disk_free_bytes(&cfg.core_datadir(chain)),
    ) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

const GIB_BYTES: u64 = 1024 * 1024 * 1024;

fn disk_low_bytes(cfg: &Config) -> u64 {
    cfg.pruning.min_free_gib.saturating_mul(GIB_BYTES)
}

fn disk_critical_bytes(cfg: &Config) -> u64 {
    let configured = cfg.pruning.critical_free_gib.saturating_mul(GIB_BYTES);
    if configured == 0 {
        return 0;
    }
    // Keep at least a small drain window below the download pause threshold.
    configured
        .min(disk_low_bytes(cfg).saturating_sub(GIB_BYTES / 4))
        .max(1)
}

/// Resume watermark: half the low threshold, clamped to 512 MiB..5 GiB. This
/// avoids the "never resumes on a small disk" problem of a fixed +5 GiB.
fn disk_resume_bytes(cfg: &Config) -> u64 {
    let low = disk_low_bytes(cfg);
    low.saturating_add((low / 2).clamp(512 * 1024 * 1024, 5 * GIB_BYTES))
}

fn warn_if_disk_low(cfg: &Config, chain: Chain) {
    let low = disk_low_bytes(cfg);
    let free = min_free_bytes(cfg, chain);
    if let Some(free) = free {
        if free < low {
            eprintln!(
                "[e-lite] warning: only {} MiB free (threshold {} MiB); indexing will pause \
                 until space is available",
                free / (1024 * 1024),
                low / (1024 * 1024)
            );
        }
    }
}

fn open_store_with(
    cfg: &Config,
    chain: Chain,
    cache_mib: u64,
    background_jobs: usize,
) -> R<Arc<Store>> {
    let dir = cfg.data_dir(chain);
    let cache_bytes = (cache_mib as usize) * 1024 * 1024;
    let ram_mib = total_ram_mib().unwrap_or(cache_mib.max(512));
    let write_mib = if cfg.index.write_buffer_mib > 0 {
        cfg.index.write_buffer_mib
    } else {
        tuning::auto_memtables_mib(cfg)
    };
    let max_buffers = if ram_mib <= 2048 { 2 } else { 3 };
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let requested_jobs = if cfg.index.background_jobs > 0 {
        cfg.index.background_jobs
    } else {
        background_jobs
    };
    let jobs = requested_jobs.clamp(1, 32).min(cpus.max(1)) as i32;
    // Descriptors are shared with client sockets, Core RPC, providers and log
    // files. Reserve them explicitly instead of assuming 64 is enough.
    let fd_reserve = (cfg.server.max_clients as u64)
        .saturating_mul(2)
        .saturating_add(192)
        .clamp(256, 4096);
    // The Core datadir normally lives inside the data dir; without excluding
    // it the status "index" figure and any benchmark disk total would count
    // Core twice.
    let mut exclude = Vec::new();
    let core_dir = cfg.core_datadir(chain);
    if core_dir.starts_with(&dir) {
        exclude.push(core_dir);
    }
    let opts = el_store::StoreOptions {
        cache_bytes,
        write_buffer_total_bytes: (write_mib as usize) * 1024 * 1024,
        max_write_buffer_number: max_buffers,
        max_total_wal_bytes: ((write_mib / 2).clamp(64, 1024) as usize) * 1024 * 1024,
        background_jobs: jobs,
        max_open_files: if cfg.index.max_open_files > 0 {
            cfg.index.max_open_files as i32
        } else {
            el_store::StoreOptions::default().max_open_files
        },
        fd_reserve,
        exclude_paths: exclude,
    };
    let store = Arc::new(Store::open_with(dir, opts)?);
    let plan = store.fd_plan();
    eprintln!(
        "[e-lite] descriptors: soft limit {}, reserve {}, max_open_files requested {}{}",
        plan.soft_limit
            .map(|v| v.to_string())
            .unwrap_or_else(|| "unlimited".to_string()),
        plan.reserve,
        plan.requested,
        if plan.capped {
            format!(" -> {}", plan.effective)
        } else {
            String::new()
        }
    );
    if plan.below_recommended() {
        eprintln!(
            "[e-lite] WARNING: max_open_files was reduced to {} because of the process file \
             limit; prevout reads will re-open SSTs. Raise `ulimit -n` (16384 is a good value) \
             for the best indexing speed.",
            plan.effective
        );
    }
    Ok(store)
}

fn open_store(cfg: &Config, chain: Chain) -> R<Arc<Store>> {
    let jobs = std::thread::available_parallelism()
        .map(|n| n.get().clamp(1, 8))
        .unwrap_or(4);
    open_store_with(cfg, chain, effective_cache_mib(cfg), jobs)
}

fn build_bulk(
    cfg: &Config,
    chain: Chain,
    genesis: Hash32,
    readers: usize,
) -> R<Option<Arc<el_blocks::BulkSource>>> {
    if cfg.index.block_source == "rpc" {
        return Ok(None);
    }
    let blocks_dir = cfg.blocks_dir(chain);
    if !blocks_dir.is_dir() {
        if cfg.index.block_source == "files" {
            return fail(format!(
                "block_source=files but the Core blocks directory does not exist: {}",
                blocks_dir.display()
            ));
        }
        eprintln!(
            "[e-lite] bulk block files unavailable at {}; using Core RPC",
            blocks_dir.display()
        );
        return Ok(None);
    }
    let cache_dir = cfg.data_dir(chain);
    match el_blocks::BulkSource::open(blocks_dir.clone(), &cache_dir, chain, genesis) {
        Ok(source) => {
            println!(
                "bulk block source: {} ({} locators, xor {}, {} readers)",
                blocks_dir.display(),
                source.entries(),
                if source.xor_present() { "on" } else { "off" },
                readers
            );
            Ok(Some(Arc::new(source)))
        }
        Err(e) => {
            if cfg.index.block_source == "files" {
                return Err(e.into());
            }
            eprintln!("[e-lite] bulk block source unavailable ({e}); using Core RPC");
            Ok(None)
        }
    }
}

fn stored_identity(store: &Store, chain: Chain) -> R<Option<NetworkIdentity>> {
    let stored = store.network_identity()?;
    if let Some(id) = &stored {
        if id.chain != chain {
            return fail(format!(
                "database belongs to {}, config says {}",
                id.chain.name(),
                chain.name()
            ));
        }
        id.check_known_genesis()?;
    }
    Ok(stored)
}

fn verify_identity(stored: &NetworkIdentity, live: &NetworkIdentity) -> R<()> {
    if stored.genesis != live.genesis {
        return fail(format!(
            "genesis mismatch: db {} vs core {}",
            stored.genesis, live.genesis
        ));
    }
    if stored.signet_challenge != live.signet_challenge {
        return fail("signet challenge mismatch");
    }
    Ok(())
}

fn main() {
    let result = run();
    if let Err(e) = &result {
        eprintln!("e-lite: error: {e}");
    }
    logging::shutdown();
    if result.is_err() {
        std::process::exit(1);
    }
}

fn run() -> R<()> {
    let args = parse_args()?;
    let _ = CONFIG_PATH.set(Config::resolve_path(args.config.as_deref()));
    let cmd = args.command.unwrap_or(Command::Help);
    if cmd == Command::Help {
        print_help();
        return Ok(());
    }
    if cmd == Command::Version {
        println!("e-lite {VERSION}");
        return Ok(());
    }
    if cmd == Command::ArcSetup {
        // Host setup only: it needs neither a network nor a reachable Core
        // (resolving `network = auto` fails on a machine without both).
        return cmd_arc_setup(args.apply);
    }

    let (cfg, chain) = if matches!(cmd, Command::Cache | Command::Privacy) {
        // Maintenance: no Core probe and no database open (a running daemon
        // holds the RocksDB lock).
        effective_config_with(&args, false)?
    } else {
        effective_config(&args)?
    };
    if let Some(level) = logging::parse(&cfg.logging.level) {
        logging::set_level(level);
    }
    el_serve::set_debug_logs(logging::debug_enabled());
    el_rawtx::set_debug_logs(logging::debug_enabled());
    el_serve::set_electrumx_compat(cfg.server.advertise_electrumx_compat);
    if matches!(cmd, Command::Index | Command::Serve | Command::Run) && !args.no_log_file {
        let path = cfg.app_log_file(chain);
        if let Err(e) = logging::init(&path, cfg.logging.max_mib) {
            eprintln!("[e-lite] WARNING: {e}; continuing without a log file");
        }
    }
    eprintln!(
        "[e-lite] log level: {} ([logging] level or --log-level; hot-reloadable)",
        logging::name(logging::level())
    );
    match cmd {
        Command::Init => cmd_init(&cfg, chain),
        Command::Doctor => cmd_doctor(&cfg, chain),
        Command::Index => cmd_index(&cfg, chain),
        Command::Serve => cmd_serve(&cfg, chain),
        Command::Run => cmd_run(&cfg, chain),
        Command::Status => cmd_status(&cfg, chain, args.json),
        Command::Core => cmd_core(&cfg, chain, args.subcommand.as_deref()),
        Command::Onion => cmd_onion(
            &cfg,
            chain,
            args.subcommand.as_deref(),
            args.purge,
            args.yes,
        ),
        Command::Privacy => match args.subcommand.as_deref() {
            None | Some("check") => {
                privacy::cmd_check(&cfg, config_path().as_deref(), args.json).map_err(Into::into)
            }
            Some(other) => {
                Err(format!("unknown privacy command: {other} (expected: check)").into())
            }
        },
        Command::Cache => cmd_cache(
            &cfg,
            chain,
            args.subcommand.as_deref(),
            args.yes,
            args.dry_run,
            args.json,
        ),
        Command::ExportSnapshot => cmd_export(
            &cfg,
            chain,
            args.output.as_deref(),
            args.with_core,
            !args.no_hash_files,
            args.compress_level,
        ),
        Command::ImportSnapshot => cmd_import(
            &cfg,
            chain,
            args.input.as_deref(),
            args.verify.as_deref(),
            args.expect_digest.as_deref(),
        ),
        Command::Help | Command::Version | Command::ArcSetup => unreachable!(),
    }
}

/// `e-lite arc-setup`: print (and optionally apply, as root) the setup that
/// lets e-lite manage the ZFS ARC cap without running as root: the sysfs
/// parameter becomes group-writable and a systemd oneshot restores the
/// permissions after every boot.
fn cmd_arc_setup(apply: bool) -> R<()> {
    let path = tuning::ARC_CAP_PATH;
    if !std::path::Path::new(path).exists() {
        println!(
            "ZFS ARC parameter not present ({path}); this host is not using ZFS, \
             nothing to set up"
        );
        return Ok(());
    }
    let euid = unsafe { libc::geteuid() };
    let sudo_user = std::env::var("SUDO_USER").ok();
    let target_user = if euid == 0 {
        sudo_user.clone().unwrap_or_else(|| "root".to_string())
    } else {
        std::env::var("USER").unwrap_or_else(|_| "root".to_string())
    };
    let group = group_of_user(&target_user).unwrap_or_else(|| "root".to_string());
    let unit = format!(
        "[Unit]\n\
         Description=e-lite: allow the ZFS ARC cap to be managed by group {group}\n\
         After=zfs-load-module.service systemd-modules-load.service\n\
         ConditionPathExists={path}\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart=/bin/chgrp {group} {path}\n\
         ExecStart=/bin/chmod 0664 {path}\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n"
    );
    println!(
        "To let e-lite ({target_user}, group {group}) manage the ZFS ARC cap without root:\n\
         \n\
         One-time, now:\n\
           sudo chgrp {group} {path}\n\
           sudo chmod 0664 {path}\n\
         \n\
         Persist after every boot (systemd):\n\
           sudo tee /etc/systemd/system/e-lite-arc-cap.service <<'EOF'\n\
         {unit}\
         EOF\n\
           sudo systemctl daemon-reload\n\
           sudo systemctl enable --now e-lite-arc-cap.service\n"
    );
    if !apply {
        println!("Run `e-lite arc-setup --apply` as root to do all of the above now.");
        return Ok(());
    }
    if euid != 0 {
        return fail("arc-setup --apply needs root (use sudo)");
    }
    let run = |args: &[&str]| -> R<()> {
        let status = std::process::Command::new(args[0])
            .args(&args[1..])
            .status()
            .map_err(|e| format!("{}: {e}", args[0]))?;
        if !status.success() {
            return fail(format!("{} failed with {status}", args.join(" ")));
        }
        Ok(())
    };
    run(&["chgrp", &group, path])?;
    run(&["chmod", "0664", path])?;
    let unit_path = "/etc/systemd/system/e-lite-arc-cap.service";
    std::fs::write(unit_path, unit).map_err(|e| format!("cannot write {unit_path}: {e}"))?;
    if std::process::Command::new("systemctl")
        .args(["daemon-reload"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        let _ = std::process::Command::new("systemctl")
            .args(["enable", "--now", "e-lite-arc-cap.service"])
            .status();
    }
    println!(
        "arc-setup applied: {path} is now group-writable by {group} and the unit is installed"
    );
    Ok(())
}

/// Primary group name of `user` (`id -gn`), for the arc-setup unit.
fn group_of_user(user: &str) -> Option<String> {
    let out = std::process::Command::new("id")
        .args(["-gn", user])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let group = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if group.is_empty() {
        None
    } else {
        Some(group)
    }
}

fn print_help() {
    println!(
        r#"e-lite {VERSION} — Electrum 1.4-1.7 server for Bitcoin Core

USAGE:
    e-lite <command> [options]

COMMANDS:
    init             validate network/genesis against Core and create the database
    doctor           check config, Core, disk, ports and database integrity
    index            index the active chain (add --no-follow to stop at tip)
    serve            serve Electrum queries from an already indexed database
    run              index and serve in one process (recommended; waits for Core)
    status           print health (via local HTTP, or the database if stopped)
    core status      show the Core supervisor state and RPC reachability
    core logs        print the last 50 lines of a Core launched by e-lite
    onion show       print the Tor onion address and managed-Tor state
    onion reset      generate a new onion address (stop e-lite first)
    cache status     size and limits of the rawtx cache (the only on-disk cache)
    cache clear      delete the rawtx cache (needs --yes; never touches the index,
                     Core, TLS or Tor); restart e-lite to release RAM as well
    export-snapshot  print the canonical digest, or write a backup with --output
                     (--with-core bundles the network's Core chain data with a
                     SHA-256 per file; gzip: index stored, Core level 3,
                     --compress-level 0..9 sets both)
    import-snapshot  restore a backup with --input (target must be empty)
                     (--verify checks per-file hashes; --verify=full opens the
                     restored Core and runs a bounded verifychain;
                     --expect-digest <hex> requires the digest you recorded)
    arc-setup        let e-lite manage the ZFS ARC cap without root
                     (prints the chgrp/chmod + systemd unit; --apply as root)
    version          print version

OPTIONS:
    --config <path>         TOML config file (default ~/.config/e-lite/config.toml)
    --public-servers <path> public-servers.toml (default: next to the config file)
    --network <name>        auto | regtest | signet | testnet4 | mainnet
    --data-dir <path>       index directory
    --core-rpc <url>        http://host:port (use an SSH/VPN tunnel for WAN)
    --rpc-user <user>       explicit Core RPC user (remote Core)
    --rpc-password <pass>   explicit Core RPC password (remote Core)
    --core-datadir <path>   Core datadir (cookie is read from there)
    --blocks-dir <path>     Core blocks directory (default <datadir>/blocks)
    --block-source <src>    auto | rpc | files (default auto)
    --readers <n>           parallel fetch workers (0 = auto)
    --cookie-file <path>    explicit Core cookie
    --manage <mode>         external | auto | managed (default auto)
    --prune-mib <n>         Core prune mode: 1 manual (default), 0 full, >=550 auto (needs opt-in)
    --min-free-gib <n>      pause Core downloads below this free space
    --listen <addr>         Electrum plain listen (default 127.0.0.1:<network port>;
                            an explicit address also moves TLS to that host)
    --plain-lan             offer the plain listener on 0.0.0.0 (TLS already is)
    --tls / --no-tls        enable (default) or disable Electrum TLS
    --tls-listen <addr>     Electrum TLS listen (default <listen host>:<network TLS port>)
    --tls-cert <path>       PEM certificate chain (default: generated self-signed)
    --tls-key <path>        PEM private key (default: generated with the certificate)
    --metrics <addr>        status/metrics listen (default 127.0.0.1:<network port>)
    --batch <n>             blocks per atomic commit (default 256)
    --reorg-window <n>      undo window in blocks (default 288)
    --allow-mainnet         required for mainnet
    --allow-clearnet        allow external rawtx over clearnet (privacy risk)
    --no-rawtx              disable external rawtx sources
    --no-follow             stop when caught up (only for `index`)
    --json                  JSON output for status
    --output <path>         snapshot file for export-snapshot
    --input <path>          snapshot file for import-snapshot
    --with-core             export-snapshot: bundle the network's Core datadir
    --no-hash-files         export-snapshot: skip the SHA-256 of every Core file
    --compress-level <n>    export-snapshot gzip level for both sections (0..9;
                            default: index 0 = stored, Core 3)
    --verify [basic|full]   import-snapshot: hash check, or Core verifychain
    --expect-digest <hex>   import-snapshot: refuse unless the recomputed index
                            digest equals this value (from your export output)
    --set <key=value>       override any TOML field, e.g. index.profile=ssd
                            (values: true, 42, ["a","b"], or a bare string)

TUNING (all networks):
    --profile <name>        auto | hdd | ssd | nvme | manual
    --adaptive              enable the controller (default)
    --no-adaptive           freeze the configured/derived values
    --cache-mib <n>         RocksDB block cache (0 = profile/RAM)
    --write-buffer-mib <n>  total RocksDB memtable budget (0 = effective RAM/16, 512..2048)
    --memory-reserve-mib <n> RAM left for OS/Core/page cache (0 = effective RAM/8, 2048..4096)
    --memory-fraction <f>   e-lite ceiling as a share of total RAM (0 = 0.75)
    --allow-low-memory      skip the minimum-RAM check (unsupported)
    --background-jobs <n>   RocksDB jobs (0 = profile, capped by CPUs)
    --commit-interval-ms <n> commit/fsync interval (0 = profile; 500..30000)
    --commit-sync <mode>    auto | always | txids (bulk durability tradeoff)
    --prefetch-blocks <n>   prepared-block window cap (0 = auto; >0 caps)
    --prefetch-deadline-ms <n> stop prefetching a block after n ms (0 = off)
    --max-open-files <n>        RocksDB fds for table readers (0 = 8192)
    --adaptive-memtables    let the controller resize memtables at runtime
    --no-adaptive-memtables keep the startup memtable budget (default)
    --milestone-blocks <n>  progress report every N blocks (0 = off)
    --milestone-seconds <n> progress report every N seconds (0 = off)

PRUNING AND CORE:
    --max-core-lag-blocks <n>   auto-prune download limit (0 = off)
    --max-bulk-lag-blocks <n>   legacy block growth limit (used if GiB is 0)
    --max-bulk-lag-gib <n>      pause Core above this unindexed backlog (0 = off)
    --pause-core-during-bulk    pause Core during a long catch-up (default)
    --no-pause-core-during-bulk disable the full bulk pause
    --core-resume-ahead-blocks <n> legacy resume margin in blocks
    --core-resume-ahead-gib <n> resume Core below this backlog (default 7)
    --scheduler <mode>          overlap (default) | alternate (opt-in test)
    --prune-defer-free-gib <n>  skip normal prune above this free space (default 20)
    --min-gain-blocks <n>       minimum safe-height gain between prunes (default 5000)
    --prune-interval-seconds <n> minimum time between prunes
    --poll-seconds <n>          supervisor poll interval
    --repair-max-gap-blocks <n> largest external prune-gap repair
    --critical-free-gib <n>     pause indexing below this free space
    --allow-auto-prune          opt in to prune_mib >= 550 (automatic pruning)
    --core-arg <arg>            extra bitcoind argument (repeatable)
    --core-io-priority <0-7>    best-effort ionice for managed Core (opt-in)
    --core-cgroup-io-weight <n> best-effort cgroup v2 io.weight (opt-in)
    --stop-on-exit / --no-stop-on-exit
    --restart / --no-restart

RAWTX, SERVER AND TLS:
    --max-clients <n>       Electrum client limit
    --banner-file <path>    wallet banner template (created if missing)
    --p2p-peers <a,b>       comma-separated P2P peers for rawtx/repair
    --no-p2p                disable the P2P rawtx/repair source
    --no-esplora            disable the Esplora rawtx/repair source
    --decoys <n>            extra blocks fetched per external lookup
    --rawtx-cache-mib <n>   validated rawtx cache size (0 = disabled)
    --tor-socks <addr>      SOCKS5h proxy for rawtx (empty requires --allow-clearnet)

TOR / ONION:
    --tor-mode <mode>       managed | off (default managed)
    --no-tor                same as --tor-mode off
    --tor-plain / --no-tor-plain    publish the plain Electrum port on the onion
    --tor-tls / --no-tor-tls        publish the TLS Electrum port on the onion
    --tor-auto-cert / --no-tor-auto-cert
                            self-signed TLS certificate when none is configured
    --tor-onion-port <n>    onion virtual port for plain (0 = same as listen)
    --tor-onion-tls-port <n> onion virtual port for TLS (0 = same as tls-listen)
    --tor-socks-port <n>    managed Tor SOCKS port (0 = auto)
    --tor-binary <path>     Tor binary (empty = installer bundle, then PATH)
    --tor-target-plain <addr> loopback target for the plain onion port
    --tor-target-tls <addr> loopback target for the TLS onion port
    --purge                 onion reset: delete keys instead of keeping onion.prev
    --yes, -y               onion reset and cache clear: do not ask for confirmation
    --dry-run               cache clear: report what would be removed and stop

LOGGING:
    --log-file <path>       application log (default <data-dir>/e-lite.log)
    --log-max-mib <n>       rotate to e-lite.log.1 above N MiB (0 = never)
    --log-level <level>     info | debug | trace (alpha default: debug;
                            extra diagnostic lines are gated by this level)
    --no-log-file           disable the application log
"#
    );
}

fn cmd_init(cfg: &Config, chain: Chain) -> R<()> {
    let core = core_client(cfg, chain)?;
    let live = core_identity(&core, chain)?;
    let store = open_store(cfg, chain)?;
    match store.network_identity()? {
        Some(stored) => {
            if stored.chain != live.chain
                || stored.genesis != live.genesis
                || stored.signet_challenge != live.signet_challenge
            {
                return fail(format!(
                    "database identity does not match Core (db genesis {}, core genesis {})",
                    stored.genesis, live.genesis
                ));
            }
            println!(
                "already initialized: {} genesis {}",
                chain.name(),
                stored.genesis
            );
        }
        None => {
            store.init_identity(&live)?;
            println!(
                "initialized {} at {}",
                chain.name(),
                cfg.data_dir(chain).display()
            );
            println!("genesis: {}", live.genesis);
            if let Some(c) = live.signet_challenge {
                println!("signet challenge hash: {c}");
            }
        }
    }
    let info = core.blockchain_info()?;
    println!("core: {} ({} blocks)", info.chain, info.blocks);
    Ok(())
}

/// Publish the effective Tor SOCKS port (not the configured `0 = auto`) and
/// the fact that e-lite never allocates a Tor control port.
fn publish_tor_ports(cfg: &Config, rt: Option<&tor::TorRuntime>, shared: &Shared) {
    shared
        .stats
        .tor_managed
        .store(cfg.tor.managed(), Ordering::Relaxed);
    let port = rt
        .map(|r| r.socks_port() as u64)
        .or_else(|| {
            if cfg.rawtx.enabled {
                cfg.rawtx
                    .tor_socks
                    .as_deref()
                    .and_then(|s| s.rsplit_once(':'))
                    .and_then(|(_, p)| p.parse::<u64>().ok())
            } else {
                None
            }
        })
        .or_else(|| (cfg.tor.socks_port > 0).then_some(cfg.tor.socks_port as u64))
        .unwrap_or(0);
    shared.stats.tor_socks_port.store(port, Ordering::Relaxed);
    shared.stats.tor_control_port.store(0, Ordering::Relaxed);
}

fn cmd_doctor(cfg: &Config, chain: Chain) -> R<()> {
    let mut problems: Vec<String> = Vec::new();
    println!("e-lite {} doctor", VERSION);
    println!("network: {}", chain.name());
    println!("data dir: {}", cfg.data_dir(chain).display());
    let public = public_servers::load(cfg, chain, config_path().as_deref());
    println!(
        "public servers: {} ({})",
        public.path.display(),
        public.source
    );
    if !public.configured() {
        println!("  esplora: none (local data only)");
    } else {
        for url in &public.endpoints {
            println!("  esplora: {url}");
        }
        let expected = chain.known_genesis().map(|g| g.to_hex());
        let probe = public_servers::probe(cfg, &public.endpoints, false, expected.as_deref());
        for status in &probe.statuses {
            println!(
                "  probe: {} healthy={} via={} {}ms{}",
                status.url,
                status.healthy,
                status.via,
                status.latency_ms,
                status
                    .error
                    .as_deref()
                    .map(|e| format!(" error={e}"))
                    .unwrap_or_default()
            );
        }
        println!(
            "  provider ready: {} (tor_ready={}, clearnet_fallback={})",
            probe.ready, probe.tor_ready, probe.clearnet_fallback
        );
    }

    let core = match core_client(cfg, chain) {
        Ok(c) => {
            println!("core rpc: {}", cfg.rpc_url(chain));
            println!(
                "core rpc transport: HTTP ({}). Core has no native TLS; use an SSH/VPN tunnel for LAN/WAN.",
                if el_rpc::rpc_url_is_loopback(&cfg.rpc_url(chain)) {
                    "loopback"
                } else {
                    "non-loopback, cleartext"
                }
            );
            c
        }
        Err(e) => {
            problems.push(format!("Core RPC: {e}"));
            print_problems(&problems);
            return fail("doctor found blocking problems");
        }
    };

    let mut core_available = true;
    match core.network_info() {
        Ok(n) => println!(
            "core version: {} ({})  networkactive: {}  connections: {}",
            n.version,
            n.subversion.trim(),
            n.networkactive,
            n.connections
        ),
        Err(e) => {
            core_available = false;
            println!("core: not available yet ({e})");
        }
    }
    let mut core_pruned = false;
    let mut pruneheight = 0u64;
    match core.blockchain_info() {
        Ok(info) => {
            println!(
                "chain: {}  blocks: {}  headers: {}  pruned: {} (from {})  ibd: {}",
                info.chain,
                info.blocks,
                info.headers,
                info.pruned,
                info.pruneheight,
                info.initialblockdownload
            );
            core_pruned = info.pruned;
            pruneheight = info.pruneheight;
        }
        Err(e) => {
            core_available = false;
            println!("core chain: not available yet ({e})");
        }
    }

    match core_identity(&core, chain) {
        Ok(id) => println!("genesis: {}", id.genesis),
        Err(e) if !core_available => println!("core identity: waiting for Core ({e})"),
        Err(e) => problems.push(format!("identity: {e}")),
    }

    let blocks_dir = cfg.blocks_dir(chain);
    let probe = el_blocks::probe(&blocks_dir, chain);
    println!(
        "blocks dir: {} ({} files, {:.2} GiB, xor {}, magic {})",
        probe.dir.display(),
        probe.files,
        probe.bytes as f64 / (1024.0 * 1024.0 * 1024.0),
        if probe.xor_present { "on" } else { "off" },
        if probe.first_magic_ok {
            "ok"
        } else {
            "unknown"
        }
    );
    if let Some(e) = &probe.error {
        println!("blocks dir note: {e}");
    }
    if cfg.index.block_source == "files" {
        if probe.files == 0 || !probe.first_magic_ok {
            problems.push(format!(
                "block_source=files but {} is not usable as a Core blocks directory",
                blocks_dir.display()
            ));
        }
    } else if !probe.first_magic_ok {
        println!("bulk block source unavailable; ingest will use Core RPC");
    }

    let mut db_tip: Option<IndexedTip> = None;
    let mut db_locked = false;
    let db_dir = cfg.data_dir(chain);
    if !db_dir.exists() {
        println!("db: not initialized (run `e-lite run`; it auto-initializes against Core)");
    } else {
        match Store::open(&db_dir) {
            Ok(store) => {
                match store.network_identity() {
                    Ok(Some(id)) => {
                        println!("db genesis: {}", id.genesis);
                        if let Ok(Some(tip)) = store.committed_tip() {
                            db_tip = Some(tip);
                            println!(
                                "db tip: {} {} (txnum {}, schema {})",
                                tip.height, tip.block_hash, tip.txnum, tip.schema_version
                            );
                        } else {
                            println!("db tip: empty (run `e-lite index`)");
                        }
                    }
                    Ok(None) => {
                        println!("db: not initialized (run `e-lite run`; it auto-initializes)")
                    }
                    Err(e) => problems.push(format!("db identity: {e}")),
                }
                println!("db size: {} bytes", store.disk_usage_bytes());
            }
            Err(e) => {
                let msg = e.to_string();
                let lower = msg.to_ascii_lowercase();
                if lower.contains("lock") || lower.contains("already") {
                    db_locked = true;
                    println!("db: in use by a running e-lite ({msg})");
                } else {
                    problems.push(format!("open db: {msg}"));
                }
            }
        }
    }

    let tor = cfg.rawtx.tor_socks.clone().filter(|s| !s.trim().is_empty());
    let external_enabled = cfg.rawtx.enabled
        && (cfg.rawtx.use_p2p || cfg.rawtx.use_esplora)
        && (tor.is_some() || cfg.rawtx.allow_clearnet);

    if core_pruned {
        match &db_tip {
            None => {
                if db_locked {
                    println!(
                        "core is pruned: the database is in use by a running e-lite; \
                         skipping the empty-index check"
                    );
                } else if pruneheight == 0 {
                    println!(
                        "core is pruned but has not pruned anything yet (pruneheight 0): \
                         e-lite can start indexing from genesis"
                    );
                } else {
                    problems.push(
                        "Core is pruned and the index is empty: use a full node, or let e-lite own Core with [core] prune_mib = 1"
                            .to_string(),
                    );
                }
            }
            Some(t) if (t.height as u64) + 1 < pruneheight => problems.push(format!(
                "Core is pruned from {} but the index stops at {}; missing blocks cannot be read",
                pruneheight, t.height
            )),
            Some(t) => {
                println!(
                    "core pruned from {}; indexed tip {} is inside the retained window",
                    pruneheight, t.height
                );
                println!("rawtx: old blocks will be resolved through verified external sources");
                if !external_enabled {
                    problems.push(
                        "old rawtx will be unavailable: Core is pruned and no external source is enabled (Tor required by default)"
                            .to_string(),
                    );
                }
            }
        }
    }
    if cfg.rawtx.enabled
        && (cfg.rawtx.use_p2p || cfg.rawtx.use_esplora)
        && tor.is_none()
        && !cfg.rawtx.allow_clearnet
    {
        problems.push(
            "rawtx external sources disabled: Tor is required (tor_socks) or set allow_clearnet=true"
                .to_string(),
        );
    }
    if let Some(t) = &tor {
        match t.parse::<std::net::SocketAddr>() {
            Ok(addr) => match TcpStream::connect_timeout(&addr, Duration::from_secs(2)) {
                Ok(_) => println!("tor proxy {}: reachable", t),
                Err(e) => println!(
                    "tor proxy {}: not reachable ({e}); external rawtx will be degraded",
                    t
                ),
            },
            Err(_) => println!("tor proxy {}: invalid address", t),
        }
    }
    if cfg.rawtx.enabled && cfg.rawtx.cache_mib > 0 {
        let dir = cfg.data_dir(chain).join("rawtx-cache");
        match std::fs::create_dir_all(&dir) {
            Ok(_) => println!(
                "rawtx cache: {} ({} MiB max, {} decoys)",
                dir.display(),
                cfg.rawtx.cache_mib,
                cfg.rawtx.decoys
            ),
            Err(e) => problems.push(format!("rawtx cache {}: {e}", dir.display())),
        }
    }

    println!(
        "silent payments: {}",
        if cfg.silent_payments.enabled {
            "enabled (BIP352 indexer not included in this build)"
        } else {
            "disabled (opt-in)"
        }
    );

    let core_free = el_chain::disk_free_bytes(&cfg.core_datadir(chain)).unwrap_or(0);
    let data_free = el_chain::disk_free_bytes(&cfg.data_dir(chain)).unwrap_or(0);
    println!(
        "disk free: index {} GiB, core datadir {} GiB (guard pauses below {} GiB)",
        data_free / (1024 * 1024 * 1024),
        core_free / (1024 * 1024 * 1024),
        cfg.pruning.min_free_gib
    );
    let min_free = cfg.pruning.min_free_gib.saturating_mul(1024 * 1024 * 1024);
    if data_free < min_free || core_free < min_free {
        println!(
            "disk note: free space is below the guard; indexing and Core downloads will pause"
        );
    }
    let mem = tuning::memory_info();
    let budget = tuning::memory_budget(cfg);
    println!(
        "memory: total {} MiB, available {} MiB (host {} MiB), free {} MiB, \
         buff/cache {} MiB, cgroup limit {} MiB, used {} MiB, reclaimable {} MiB",
        mem.total_mib.unwrap_or(0),
        mem.available_mib,
        mem.host_available_mib,
        mem.free_mib,
        mem.cached_mib,
        mem.cgroup_limit_mib.unwrap_or(0),
        mem.cgroup_used_mib.unwrap_or(0),
        mem.reclaimable_mib.unwrap_or(0)
    );
    println!(
        "memory limits: source {:?}, host total {} MiB, cgroup {}",
        mem.source,
        mem.host_total_mib.unwrap_or(0),
        if mem.cgroup_detected {
            if mem.cgroup_unlimited {
                "detected (unlimited)".to_string()
            } else {
                format!("detected (limit {} MiB)", mem.cgroup_limit_mib.unwrap_or(0))
            }
        } else {
            "not detected (host RAM used)".to_string()
        }
    );
    println!(
        "memory minimum: {} MiB for {} ({} here)",
        tuning::minimum_ram_mib(chain),
        chain.name(),
        if mem.total_mib.unwrap_or(0) >= tuning::minimum_ram_mib(chain) {
            "met"
        } else {
            "NOT MET"
        }
    );
    println!(
        "memory budget: total {} MiB, available {} MiB, effective {} MiB \
         (core reserve {}), cached {} MiB, reserve {} MiB, ceiling {} MiB, \
         cache start..max {}..{} MiB, memtables {} MiB",
        budget.total_mib,
        budget.available_mib,
        budget.effective_mib,
        budget.core_reserve_mib,
        budget.cached_mib,
        budget.reserve_mib,
        budget.ceiling_mib,
        budget.cache_start_mib,
        budget.cache_max_mib,
        budget.memtables_mib
    );
    println!(
        "memory plan: runtime {} MiB, planned {} MiB of {} MiB{}",
        budget.runtime_mib,
        budget.planned_mib,
        budget.ceiling_mib,
        if budget.over_budget {
            " (OVER BUDGET)"
        } else {
            ""
        }
    );
    match core_plan::plan_core(cfg, chain) {
        Ok(plan) => println!("core plan: {}", core_plan::describe(&plan)),
        Err(e) => problems.push(format!("core plan: {e}")),
    }
    let machine = tuning::MachinePlan::detect(cfg, chain);
    println!("machine: {}", machine.describe());
    if machine.data_disk.kind == tuning::DiskKind::Unknown
        || machine.data_disk.kind == tuning::DiskKind::Virtual
    {
        println!(
            "machine note: disk class is {}; using conservative reader/cache values",
            machine.data_disk.kind.name()
        );
    }
    {
        let reserve = (cfg.server.max_clients as u64)
            .saturating_mul(2)
            .saturating_add(192)
            .clamp(256, 4096);
        let fd = el_store::fd_plan(8192, reserve);
        println!(
            "descriptors: soft limit {}, reserve {}, max_open_files {} (requested {}){}",
            fd.soft_limit
                .map(|v| v.to_string())
                .unwrap_or_else(|| "unlimited".to_string()),
            fd.reserve,
            fd.effective,
            fd.requested,
            if fd.below_recommended() {
                " — raise `ulimit -n` for best indexing speed"
            } else {
                ""
            }
        );
    }
    let core_sup = CoreSupervisor::new(cfg, chain);
    println!(
        "core manage: {}",
        serde_json::to_string(&core_sup.status_json()).unwrap_or_default()
    );
    if !core_available && matches!(cfg.core.manage, CoreManage::External) {
        println!("core: external mode and unavailable; e-lite will wait and serve cached data");
    }

    if bind_ok(&cfg.server.listen) {
        println!("electrum listen {}: available", cfg.server.listen);
    } else {
        problems.push(format!(
            "electrum listen {} is already in use",
            cfg.server.listen
        ));
    }
    if cfg.server.tls_enabled() {
        if bind_ok(&cfg.server.tls_listen) {
            println!("electrum TLS listen {}: available", cfg.server.tls_listen);
        } else {
            problems.push(format!(
                "electrum TLS listen {} is already in use",
                cfg.server.tls_listen
            ));
        }
        let cert = std::path::PathBuf::from(&cfg.server.tls_cert);
        let key = std::path::PathBuf::from(&cfg.server.tls_key);
        match el_serve::CertWatcher::load(cert.clone(), key.clone(), Duration::ZERO) {
            Ok(_) => println!(
                "electrum TLS certificate: {} (key {})",
                cert.display(),
                key.display()
            ),
            Err(e) => problems.push(format!("electrum TLS certificate/key: {e}")),
        }
    } else {
        println!("electrum TLS: disabled (set [server] tls = true to enable)");
    }
    let tor_status = tor::status(cfg, chain);
    println!(
        "tor: mode {}  socks {}  pid {}  running {}",
        tor_status.mode,
        tor_status.socks,
        tor_status
            .pid
            .map(|p| p.to_string())
            .unwrap_or_else(|| "-".to_string()),
        tor_status.running
    );
    match &tor_status.binary {
        Some(path) => println!("tor binary: {}", path.display()),
        None => {
            if cfg.tor.managed() {
                println!(
                    "tor note: Tor is enabled but no binary was found; install `tor` or rerun \
                     the e-lite installer. Indexing and plain Electrum keep working."
                );
            }
            println!("tor binary: not found");
        }
    }
    if tor_status.binary.is_some() {
        match tor::version(cfg) {
            Ok(v) => println!("tor version: {v}"),
            Err(e) => println!("tor version: unavailable ({e})"),
        }
    }
    match &tor_status.onion {
        Some(addr) => {
            println!(
                "tor onion: {addr}:{} (plain){}",
                tor_status.plain_port,
                if let Some(port) = tor_status.tls_port {
                    format!(", :{port} (tls)")
                } else {
                    String::new()
                }
            );
        }
        None => println!("tor onion: not created yet (it is created on `run`/`serve`)"),
    }
    if cfg.core.manage != CoreManage::External {
        let forced_onion = cfg.core.extra_args.iter().any(|a| {
            let name = a.trim();
            name == "-listenonion" || name.eq_ignore_ascii_case("-listenonion=1")
        });
        if forced_onion {
            println!(
                "core onion: enabled by operator; the managed torrc has no ControlPort, so Core \
                 will log 9051 connection errors unless -torcontrol points to a working one"
            );
        } else {
            println!(
                "core onion: disabled by e-lite (-listenonion=0; only Electrum onion is managed)"
            );
        }
    }
    if cfg.tor.managed() {
        if let Err(e) = tor::loopback_target(&cfg.server.listen) {
            problems.push(e);
        }
        if cfg.tor.tls {
            if let Err(e) = tor::loopback_target(&tor_status.target_tls) {
                problems.push(e);
            }
        }
    } else if cfg.server.tls_enabled() {
        println!(
            "tor TLS: publishes {} over the onion when Tor runs (cert {})",
            cfg.server.tls_listen, cfg.server.tls_cert
        );
    }
    if cfg.metrics.enabled {
        if bind_ok(&cfg.metrics.listen) {
            println!("metrics listen {}: available", cfg.metrics.listen);
        } else {
            problems.push(format!(
                "metrics listen {} is already in use",
                cfg.metrics.listen
            ));
        }
    }

    if problems.is_empty() {
        println!("OK: no blocking problems found");
        Ok(())
    } else {
        print_problems(&problems);
        fail("doctor found blocking problems")
    }
}

fn cmd_core(cfg: &Config, chain: Chain, action: Option<&str>) -> R<()> {
    match action.unwrap_or("status") {
        "status" => {
            let sup = CoreSupervisor::new(cfg, chain);
            println!("{}", serde_json::to_string_pretty(&sup.status_json())?);
            if let Ok(pid) = std::fs::read_to_string(cfg.pid_file(chain)) {
                println!("pid file: {}", pid.trim());
            }
            let core = core_client(cfg, chain)?;
            match core.blockchain_info() {
                Ok(info) => println!(
                    "reachable: yes  chain {}  blocks {}  headers {}  pruned {}  pruneheight {}  ibd {}",
                    info.chain,
                    info.blocks,
                    info.headers,
                    info.pruned,
                    info.pruneheight,
                    info.initialblockdownload
                ),
                Err(e) => println!("reachable: no ({e})"),
            }
            Ok(())
        }
        "logs" => {
            let path = cfg.log_file(chain);
            let text = std::fs::read_to_string(&path)
                .map_err(|e| format!("cannot read Core log {}: {e}", path.display()))?;
            let lines: Vec<&str> = text.lines().collect();
            for line in lines.iter().rev().take(50).rev() {
                println!("{line}");
            }
            Ok(())
        }
        "start" | "stop" => fail(
            "`core start`/`core stop` are not separate commands: use `e-lite run` \
             so the index and Core share the same lifecycle",
        ),
        other => fail(format!(
            "unknown core action \"{other}\": use status or logs"
        )),
    }
}

fn cmd_onion(cfg: &Config, chain: Chain, action: Option<&str>, purge: bool, yes: bool) -> R<()> {
    match action.unwrap_or("show") {
        "show" => {
            let s = tor::status(cfg, chain);
            println!("tor mode:   {}", s.mode);
            match &s.binary {
                Some(p) => println!("tor binary: {}", p.display()),
                None => println!("tor binary: not found"),
            }
            if s.binary.is_some() {
                match tor::version(cfg) {
                    Ok(v) => println!("tor version: {v}"),
                    Err(e) => println!("tor version: unavailable ({e})"),
                }
            }
            println!("tor socks:  {}", s.socks);
            println!(
                "tor pid:    {} ({})",
                s.pid
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                if s.running { "running" } else { "not running" }
            );
            println!("plain:      {} -> {}", s.target_plain, s.plain_port);
            if let Some(port) = s.tls_port {
                println!("tls:        {} -> {}", s.target_tls, port);
            } else {
                println!("tls:        disabled");
            }
            match &s.onion {
                Some(addr) => {
                    println!("onion:      {addr}");
                    println!("connect:    {addr}:{} (plain)", s.plain_port);
                    if let Some(port) = s.tls_port {
                        println!("connect:    {addr}:{port} (tls)");
                    }
                }
                None => {
                    println!("onion:      not created yet (start `e-lite run` or `e-lite serve`)")
                }
            }
            Ok(())
        }
        "reset" => {
            if !yes {
                eprint!(
                    "This generates a new onion address and invalidates the current one. Continue? [y/N] "
                );
                let mut answer = String::new();
                std::io::stdin()
                    .read_line(&mut answer)
                    .map_err(|e| format!("cannot read confirmation: {e}"))?;
                if !matches!(answer.trim(), "y" | "Y" | "yes" | "YES") {
                    println!("aborted; pass --yes for non-interactive use");
                    return Ok(());
                }
            }
            let old = tor::reset(cfg, chain, purge)?;
            println!("old onion: {old}");
            if purge {
                println!("onion keys deleted; the new address is generated on the next start");
            } else {
                println!(
                    "onion keys moved to {} (the new address is generated on the next start)",
                    cfg.tor_dir(chain).join("onion.prev").display()
                );
            }
            Ok(())
        }
        other => fail(format!(
            "unknown onion action \"{other}\": use show or reset"
        )),
    }
}

/// Free space on the filesystem holding `path` (walks up when it does not
/// exist yet). `None` when statvfs fails.
fn free_bytes(path: &std::path::Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let mut probe = path.to_path_buf();
    loop {
        let c = std::ffi::CString::new(probe.as_os_str().as_bytes()).ok()?;
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut stat) } == 0 {
            return Some((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64));
        }
        if !probe.pop() {
            return None;
        }
    }
}

/// `e-lite cache status|clear`: the only command that deletes files, and it
/// only deletes the memoised rawtx cache. The index, Core, TLS and Tor share
/// the data directory and are never touched (see `el_rawtx::cachelayout`).
fn cmd_cache(
    cfg: &Config,
    chain: Chain,
    action: Option<&str>,
    yes: bool,
    dry_run: bool,
    json: bool,
) -> R<()> {
    use el_rawtx::cachelayout;
    let data_dir = cfg.data_dir(chain);
    let dir = cachelayout::resolve_dir(&data_dir, &cfg.rawtx.cache_dir);
    let (block_limit_mib, tx_limit_mib) = el_rawtx::cache_split(cfg.rawtx.cache_mib);
    let action = action.unwrap_or("status");
    match action {
        "status" => {
            let inv = cachelayout::inventory(&dir)?;
            let free = free_bytes(&dir).or_else(|| free_bytes(&data_dir));
            let refused = cachelayout::refuse_reason(&dir, &data_dir);
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "network": chain.name(),
                        "path": dir.display().to_string(),
                        "enabled": cfg.rawtx.cache_mib > 0,
                        "capacity_bytes": cfg.rawtx.cache_mib * 1024 * 1024,
                        "blocks": {
                            "files": inv.block_files,
                            "bytes": inv.block_bytes,
                            "limit_bytes": block_limit_mib * 1024 * 1024,
                        },
                        "txs": {
                            "files": inv.tx_files,
                            "bytes": inv.tx_bytes,
                            "limit_bytes": tx_limit_mib * 1024 * 1024,
                        },
                        "total": {"files": inv.files(), "bytes": inv.bytes()},
                        "skipped": inv.skipped,
                        "free_bytes": free,
                        "refused": refused,
                    }))?
                );
            } else {
                println!("network: {}", chain.name());
                println!("cache:   {}", dir.display());
                if cfg.rawtx.cache_mib == 0 {
                    println!("state:   disabled ([rawtx] cache_mib = 0)");
                }
                println!(
                    "blocks:  {} files, {} (limit {})",
                    inv.block_files,
                    display_size(inv.block_bytes),
                    display_size(block_limit_mib * 1024 * 1024)
                );
                println!(
                    "txs:     {} files, {} (limit {})",
                    inv.tx_files,
                    display_size(inv.tx_bytes),
                    display_size(tx_limit_mib * 1024 * 1024)
                );
                println!(
                    "total:   {} files, {} of {}",
                    inv.files(),
                    display_size(inv.bytes()),
                    display_size(cfg.rawtx.cache_mib * 1024 * 1024)
                );
                if let Some(free) = free {
                    println!("free:    {} on that filesystem", display_size(free));
                }
                if inv.skipped > 0 {
                    println!(
                        "skipped: {} entries that are not cache files (never touched)",
                        inv.skipped
                    );
                }
                if let Some(reason) = &refused {
                    println!(
                        "WARNING: this path is not a rawtx cache ({reason}); \
                         `cache clear` would refuse it"
                    );
                }
                println!(
                    "note:    the in-memory block cache is not affected; restart e-lite to \
                     release RAM"
                );
            }
            Ok(())
        }
        "clear" => {
            if let Some(reason) = cachelayout::refuse_reason(&dir, &data_dir) {
                return fail(format!(
                    "refusing to clear {}: {reason}. Nothing was deleted; pass an explicit \
                     [rawtx] cache_dir or --data-dir if this is really the cache",
                    dir.display()
                ));
            }
            if cfg.rawtx.cache_mib == 0 {
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json!({
                            "network": chain.name(),
                            "path": dir.display().to_string(),
                            "enabled": false,
                            "removed": {"files": 0, "bytes": 0},
                        }))?
                    );
                } else {
                    println!("rawtx cache is disabled ([rawtx] cache_mib = 0); nothing to clear");
                }
                return Ok(());
            }
            let before = cachelayout::inventory(&dir)?;
            let free_before = free_bytes(&dir);
            if !yes || dry_run {
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json!({
                            "network": chain.name(),
                            "path": dir.display().to_string(),
                            "dry_run": true,
                            "would_remove": {"files": before.files(), "bytes": before.bytes()},
                        }))?
                    );
                } else {
                    println!(
                        "would remove {} files ({}) from {}",
                        before.files(),
                        display_size(before.bytes()),
                        dir.display()
                    );
                    println!("nothing was deleted; re-run with --yes to clear the cache");
                }
                return Ok(());
            }
            let removed = cachelayout::clear(&dir, false)?;
            let after = cachelayout::inventory(&dir)?;
            let free_after = free_bytes(&dir);
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "network": chain.name(),
                        "path": dir.display().to_string(),
                        "removed": {"files": removed.files(), "bytes": removed.bytes()},
                        "remaining": {"files": after.files(), "bytes": after.bytes()},
                        "skipped": after.skipped,
                        "free_bytes_before": free_before,
                        "free_bytes_after": free_after,
                    }))?
                );
            } else {
                println!(
                    "removed {} files ({}) from {}",
                    removed.files(),
                    display_size(removed.bytes()),
                    dir.display()
                );
                println!(
                    "remaining: {} files, {}",
                    after.files(),
                    display_size(after.bytes())
                );
                if let (Some(before), Some(after)) = (free_before, free_after) {
                    println!(
                        "free space: {} -> {}",
                        display_size(before),
                        display_size(after)
                    );
                }
                if after.skipped > 0 {
                    println!("kept {} entries that are not cache files", after.skipped);
                }
                println!("the in-memory block cache is freed by restarting e-lite");
            }
            Ok(())
        }
        other => fail(format!(
            "unknown cache action \"{other}\": use status or clear"
        )),
    }
}

fn print_problems(p: &[String]) {
    for x in p {
        eprintln!("PROBLEM: {x}");
    }
}

fn bind_ok(addr: &str) -> bool {
    std::net::TcpListener::bind(addr).is_ok()
}

// ------------------------------------------------------------------- indexing

const CATCH_UP_BLOCKS: u64 = 512;
const GIB: u64 = 1024 * 1024 * 1024;

type FetchResult =
    std::result::Result<(u64, el_index::PreparedBlock, el_index::PrefetchedPrevouts), String>;
type PendingMap = std::collections::BTreeMap<usize, FetchResult>;

struct MilestoneState {
    last_height: u64,
    last_txnum: u64,
    last_at: Instant,
}

struct Indexer {
    store: Arc<Store>,
    core: CoreClient,
    bulk: Option<Arc<el_blocks::BulkSource>>,
    resolver: Option<Arc<Resolver>>,
    tunables: Arc<Tunables>,
    cfg: Config,
    lock: Arc<RwLock<()>>,
    shared: Option<Arc<Shared>>,
    shutdown: Arc<AtomicBool>,
    readers: Option<Arc<ReaderTracker>>,
    can_control_core: bool,
    lag_paused: AtomicBool,
    /// Core downloads are paused (disk pressure, lag guard or manual pause),
    /// but indexing of already-downloaded blocks may continue.
    downloads_paused: AtomicBool,
    /// Indexing itself is paused. Only the critical disk floor or shutdown set
    /// this; it is separate from pausing Core downloads.
    paused: AtomicBool,
    pause_marker: std::path::PathBuf,
    data_dir: std::path::PathBuf,
    core_datadir: std::path::PathBuf,
    blocks_dir: std::path::PathBuf,
    last_disk_check: Mutex<Instant>,
    /// Rate limit for the no-progress pipeline note (10 s).
    last_pipeline_note: Mutex<Option<Instant>>,
    progress_started: Instant,
    progress_start_height: u64,
    progress_start_txnum: u64,
    milestone: Mutex<MilestoneState>,
    milestone_sizes_running: Arc<AtomicBool>,
    bulk_lag_baseline: AtomicU64,
    bulk_started: AtomicBool,
    bulk_full_pause: AtomicBool,
    /// Opt-in drain-time lag policy (`E_LITE_LAG_POLICY=drain`). The default
    /// keeps the block window with a fixed baseline (no ratchet).
    lag_policy_drain: bool,
    lag_target_secs: u64,
    /// `true` when `[pruning] scheduler = "alternate"`: Core downloads and
    /// e-lite indexes in alternating phases. `false` keeps the overlap.
    scheduler_alternate: bool,
    /// Byte watermark confirmation: a pause/resume must persist before acting.
    high_since: Mutex<Option<Instant>>,
    low_since: Mutex<Option<Instant>>,
    /// The alternate scheduler has paused indexing (Core is downloading).
    /// Serving (Electrum, Tor, status) continues.
    index_paused: AtomicBool,
    /// 0 = overlap/indexing, 1 = Core paused/e-lite draining, 2 = Core phase.
    phase: AtomicU64,
    phase_state: Mutex<Option<(Instant, u64)>>,
    last_rocksdb_log: Mutex<Instant>,
    /// Latest Core observation shared by a 1 Hz poller thread. Keeps
    /// `getblockchaininfo` out of the block-application path.
    core_obs: Arc<Mutex<Option<(BlockchainInfo, Instant)>>>,
    /// `pruneblockchain` runs in its own thread: the WAL barrier is taken on
    /// the apply thread, but the RPC may spend tens of seconds in Core.
    prune_rx: Mutex<Option<std::sync::mpsc::Receiver<Result<i64, String>>>>,
    prune_in_flight: AtomicBool,
    /// A prune requested by the supervisor. It is started only at a commit
    /// boundary, after the current group has been committed and the WAL
    /// barrier has advanced the durable watermark.
    deferred_prune: Mutex<Option<u32>>,
}

impl Drop for Indexer {
    fn drop(&mut self) {
        // Never leave Core with P2P disabled after a clean e-lite shutdown.
        if self.can_control_core
            && (self.lag_paused.load(Ordering::Relaxed)
                || self.downloads_paused.load(Ordering::Relaxed))
        {
            let _ = self.core.set_network_active(true);
            self.mark_paused(false);
            if let Some(sh) = &self.shared {
                sh.stats.core_network_active.store(true, Ordering::Relaxed);
            }
        }
    }
}

impl Indexer {
    #[allow(clippy::too_many_arguments)]
    fn new(
        store: Arc<Store>,
        core: CoreClient,
        bulk: Option<Arc<el_blocks::BulkSource>>,
        resolver: Option<Arc<Resolver>>,
        tunables: Arc<Tunables>,
        cfg: Config,
        lock: Arc<RwLock<()>>,
        shared: Option<Arc<Shared>>,
        shutdown: Arc<AtomicBool>,
        readers: Option<Arc<ReaderTracker>>,
        can_control_core: bool,
        chain: Chain,
    ) -> Self {
        let data_dir = cfg.data_dir(chain);
        let core_datadir = cfg.core_datadir(chain);
        let blocks_dir = cfg.blocks_dir(chain);
        let pause_marker = data_dir.join("core-paused-by-e-lite");
        let tip = store.committed_tip().ok().flatten();
        let start_height = tip.as_ref().map(|t| t.height as u64).unwrap_or(0);
        let start_txnum = tip.as_ref().map(|t| t.txnum).unwrap_or(0);
        let now = Instant::now();
        let lag_policy_drain = matches!(
            std::env::var("E_LITE_LAG_POLICY").ok().as_deref(),
            Some("drain")
        );
        let lag_target_secs = std::env::var("E_LITE_LAG_TARGET_SECONDS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(1_800)
            .clamp(120, 6 * 3_600);
        let requested_alternate = cfg.pruning.scheduler.eq_ignore_ascii_case("alternate");
        let scheduler_alternate = requested_alternate && can_control_core;
        if requested_alternate && !can_control_core {
            eprintln!(
                "[e-lite] WARNING: scheduler = \"alternate\" needs a Core that e-lite can pause; \
                 this Core is external/already running. Keeping overlap."
            );
        }
        if let Some(sh) = &shared {
            sh.stats
                .scheduler_alternate
                .store(scheduler_alternate, Ordering::Relaxed);
            sh.stats.scheduler_high_bytes.store(
                cfg.pruning
                    .max_bulk_lag_gib
                    .saturating_mul(1024 * 1024 * 1024),
                Ordering::Relaxed,
            );
            sh.stats.scheduler_low_bytes.store(
                cfg.pruning
                    .core_resume_ahead_gib
                    .saturating_mul(1024 * 1024 * 1024),
                Ordering::Relaxed,
            );
        }
        Indexer {
            store,
            core,
            bulk,
            resolver,
            tunables,
            cfg,
            lock,
            shared,
            shutdown,
            readers,
            can_control_core,
            lag_paused: AtomicBool::new(false),
            downloads_paused: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            pause_marker,
            data_dir,
            core_datadir,
            blocks_dir,
            last_disk_check: Mutex::new(
                Instant::now()
                    .checked_sub(Duration::from_secs(60))
                    .unwrap_or_else(Instant::now),
            ),
            last_pipeline_note: Mutex::new(None),
            progress_started: now,
            progress_start_height: start_height,
            progress_start_txnum: start_txnum,
            milestone: Mutex::new(MilestoneState {
                last_height: start_height,
                last_txnum: start_txnum,
                last_at: now,
            }),
            milestone_sizes_running: Arc::new(AtomicBool::new(false)),
            bulk_lag_baseline: AtomicU64::new(u64::MAX),
            bulk_started: AtomicBool::new(false),
            bulk_full_pause: AtomicBool::new(false),
            lag_policy_drain,
            lag_target_secs,
            scheduler_alternate,
            high_since: Mutex::new(None),
            low_since: Mutex::new(None),
            index_paused: AtomicBool::new(false),
            phase: AtomicU64::new(0),
            phase_state: Mutex::new(None),
            last_rocksdb_log: Mutex::new(Instant::now()),
            core_obs: Arc::new(Mutex::new(None)),
            prune_rx: Mutex::new(None),
            prune_in_flight: AtomicBool::new(false),
            deferred_prune: Mutex::new(None),
        }
    }

    /// Shared handle for the Core observation poller.
    fn core_observation(&self) -> Arc<Mutex<Option<(BlockchainInfo, Instant)>>> {
        Arc::clone(&self.core_obs)
    }

    /// Cached observation, falling back to a synchronous RPC only when the
    /// cached value is missing or older than `max_age`.
    fn observed_core(&self, max_age: Duration) -> Option<BlockchainInfo> {
        if let Ok(guard) = self.core_obs.lock() {
            if let Some((info, at)) = guard.as_ref() {
                if at.elapsed() <= max_age {
                    return Some(info.clone());
                }
            }
        }
        None
    }

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
    fn disk_guard(&self) -> bool {
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

    fn downloads_paused(&self) -> bool {
        self.downloads_paused.load(Ordering::SeqCst)
    }

    /// Pause Core's P2P downloads without stopping indexing. The marker lets a
    /// later e-lite run restore `networkactive` after a crash.
    fn pause_downloads(&self, reason: &str) {
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

    fn resume_downloads(&self, reason: &str) {
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

    fn mark_paused(&self, paused: bool) {
        if paused {
            let _ = std::fs::write(
                &self.pause_marker,
                format!("pid={} at={:?}\n", std::process::id(), Instant::now()),
            );
        } else {
            let _ = std::fs::remove_file(&self.pause_marker);
        }
    }

    fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::SeqCst);
    }

    fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    fn stats_core_height(&self, h: u64) {
        if let Some(sh) = &self.shared {
            sh.stats.core_height.store(h, Ordering::Relaxed);
        }
        self.tunables.set_core_state(h, self.tunables.core_ibd());
    }

    /// Live Core tip and pruneheight for progress lines. Falls back to the
    /// catch-up snapshot when Core is briefly unavailable.
    fn live_core_and_prune(&self, fallback_core: u64, fallback_prune: u64) -> (u64, u64) {
        let cached = self
            .shared
            .as_ref()
            .map(|sh| sh.stats.core_height.load(Ordering::Relaxed))
            .unwrap_or(0);
        let core = self
            .core
            .block_count()
            .unwrap_or(fallback_core)
            .max(fallback_core)
            .max(cached);
        let prune = self
            .shared
            .as_ref()
            .map(|sh| sh.stats.core_pruneheight.load(Ordering::Relaxed))
            .filter(|h| *h > 0)
            .unwrap_or(fallback_prune);
        (core, prune)
    }

    fn refresh_core_height(&self) {
        if let Ok(h) = self.core.block_count() {
            self.stats_core_height(h);
        }
    }

    fn stats_tip(&self, tip: &IndexedTip, last_block_ms: u64) {
        if let Some(sh) = &self.shared {
            sh.stats
                .indexed_height
                .store(tip.height as u64, Ordering::Relaxed);
            sh.stats.txnum.store(tip.txnum, Ordering::Relaxed);
            sh.stats
                .last_block_ms
                .store(last_block_ms, Ordering::Relaxed);
            sh.stats.blocks_applied.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn handle_reorg(&self, core_tip: u64) -> R<()> {
        let Some(tip) = self.store.committed_tip()? else {
            return Ok(());
        };
        let check_height = (tip.height as u64).min(core_tip);
        let core_hash = self.core.block_hash(check_height)?;
        let db_rec = self
            .store
            .get_block(check_height as u32)?
            .ok_or("database is missing its own tip block")?;
        if db_rec.hash.to_hex() == core_hash {
            // Core's chain can be shorter than the index (invalidateblock,
            // manual rollback). Drop the blocks above Core's tip.
            if (tip.height as u64) > core_tip {
                let window = self.cfg.index.reorg_window as u64;
                if (tip.height as u64).saturating_sub(core_tip) > window {
                    return fail(format!(
                        "Core tip {core_tip} is {} blocks below the index ({}), beyond the \
                         {window}-block undo window: NEEDS_RECOVERY (restore a snapshot or reindex)",
                        (tip.height as u64).saturating_sub(core_tip),
                        tip.height
                    ));
                }
                if let Some(sh) = &self.shared {
                    sh.set_state(state::REORG);
                }
                eprintln!(
                    "[e-lite] reorg detected: Core tip {core_tip} is behind the index ({}); rewinding",
                    tip.height
                );
                let _g = self.lock.write().map_err(|_| "lock poisoned")?;
                rewind_to(&self.store, check_height as u32)?;
                drop(_g);
                if let Some(sh) = &self.shared {
                    sh.headers.invalidate_from(check_height as u32);
                    sh.dirty_scripts.mark_all_subscriptions();
                    refresh_ready_state(sh);
                }
            }
            return Ok(());
        }

        if let Some(sh) = &self.shared {
            sh.set_state(state::REORG);
        }
        let window = self.cfg.index.reorg_window as u64;
        let mut h = check_height;
        loop {
            if h == 0 {
                return fail(format!(
                    "genesis mismatch: db {}, core {} — wrong network or wiped node",
                    tip.block_hash,
                    self.core.block_hash(0)?
                ));
            }
            let ch = self.core.block_hash(h)?;
            let db = self
                .store
                .get_block(h as u32)?
                .ok_or_else(|| format!("db missing block {h}"))?;
            if db.hash.to_hex() == ch {
                break;
            }
            h -= 1;
            if (tip.height as u64).saturating_sub(h) > window {
                return fail(format!(
                    "reorg deeper than the {window}-block undo window: NEEDS_RECOVERY (restore a snapshot or reindex)"
                ));
            }
        }
        eprintln!(
            "[e-lite] reorg detected: rewinding from {} to {} and replaying the active chain",
            tip.height, h
        );
        {
            let _g = self.lock.write().map_err(|_| "lock poisoned")?;
            rewind_to(&self.store, h as u32)?;
        }
        if let Some(sh) = &self.shared {
            sh.headers.invalidate_from(h as u32);
            sh.dirty_scripts.mark_all_subscriptions();
        }
        Ok(())
    }

    /// `auto`: relax the RocksDB fsync during bulk/IBD and return to strict
    /// durability when the index is live. `txids.dat` is fsynced either way.
    fn commit_sync(&self, ibd: bool, height: u64, core_tip: u64) -> bool {
        commit_sync_policy(&self.cfg.index.commit_sync, ibd, height, core_tip)
    }

    #[allow(clippy::too_many_arguments)]
    fn commit_group(
        &self,
        batch: WriteBatch,
        tip: &IndexedTip,
        txids: &[Hash32],
        blocks: u32,
        bytes: u64,
        last_block_ms: u64,
        undo: &[el_model::UndoRecord],
        sync: bool,
        reason: CommitReason,
    ) -> R<()> {
        let mut batch = batch;
        // Undo is built for every applied block, so an early commit (time,
        // transaction limit, boundary, shutdown) is as rewindable as a full
        // batch. Only the records inside the window of the tip actually
        // committed are written (AUD-131); older ones in the database are
        // deleted in the same batch.
        prune_undo(
            &self.store,
            tip.height,
            self.cfg.index.reorg_window,
            &mut batch,
        )?;
        write_window_undo(
            &self.store,
            &mut batch,
            undo,
            tip.height,
            self.cfg.index.reorg_window,
        )?;
        {
            let _g = self.lock.write().map_err(|_| "lock poisoned")?;
            let commit_started = Instant::now();
            let result = self.store.commit_with_sync(batch, tip, txids, sync);
            let (ct, cd) = self.store.last_commit_timings_us();
            // The spike check compares against the per-block cost of the last
            // full group *before* this one, so a sustained slowdown cannot be
            // silently re-normalised into the baseline.
            let prev_per_block = self.tunables.commit_per_block_ms.load(Ordering::Relaxed);
            let commit_elapsed = commit_started.elapsed();
            self.tunables.record_commit_group(
                commit_started,
                commit_elapsed,
                blocks as u64,
                txids.len() as u64,
                bytes,
                reason,
                ct,
                cd,
            );
            if result.is_ok() && reason.full() && blocks > 0 {
                self.warn_commit_spike(
                    commit_elapsed,
                    blocks,
                    txids.len() as u64,
                    bytes,
                    reason,
                    tip.height as u64,
                    prev_per_block,
                );
            }
            crate::trace_log!(
                "commit-group: {} ms, {} blocks / {} txs / {} MiB, reason {}, \
                 txids {} us, db {} us, sync {}",
                commit_elapsed.as_millis(),
                blocks,
                txids.len(),
                bytes / (1024 * 1024),
                reason.name(),
                ct,
                cd,
                sync
            );
            if let Err(e) = result {
                // Whatever happened, keep the flat file aligned with the DB.
                if let Ok(Some(t)) = self.store.committed_tip() {
                    self.store.truncate_txids(t.txnum)?;
                }
                return Err(e.into());
            }
        }
        self.stats_tip(tip, last_block_ms);
        if let Some(sh) = &self.shared {
            sh.stats.commits.fetch_add(1, Ordering::Relaxed);
            sh.stats
                .committed_txs
                .fetch_add(txids.len() as u64, Ordering::Relaxed);
            let (ct, cd) = self.store.last_commit_timings_us();
            sh.stats.commit_txids_us.store(ct, Ordering::Relaxed);
            sh.stats.commit_db_us.store(cd, Ordering::Relaxed);
            let c = self.store.compaction_stats();
            sh.stats
                .rocksdb_pending_compaction_bytes
                .store(c.pending_compaction_bytes, Ordering::Relaxed);
            sh.stats
                .rocksdb_running_compactions
                .store(c.running_compactions, Ordering::Relaxed);
            sh.stats
                .rocksdb_memtable_bytes
                .store(c.memtable_bytes, Ordering::Relaxed);
            sh.stats
                .rocksdb_block_cache_bytes
                .store(c.block_cache_bytes, Ordering::Relaxed);
            sh.stats
                .rocksdb_sst_bytes
                .store(c.sst_files_bytes, Ordering::Relaxed);
            sh.stats
                .rocksdb_table_readers_bytes
                .store(c.table_readers_bytes, Ordering::Relaxed);
            sh.stats
                .rocksdb_level0_files
                .store(c.level0_files, Ordering::Relaxed);
            sh.stats
                .rocksdb_properties_ok
                .store(c.properties_ok, Ordering::Relaxed);
            let mut cf_map = serde_json::Map::new();
            for (name, pending, memtable, sst, readers, level0, ok) in self.store.cf_stats() {
                cf_map.insert(
                    name.to_string(),
                    json!({
                        "pending_compaction_bytes": pending,
                        "memtable_bytes": memtable,
                        "sst_bytes": sst,
                        "table_readers_bytes": readers,
                        "level0_files": level0,
                        "properties_ok": ok,
                    }),
                );
            }
            if let Ok(mut v) = sh.stats.rocksdb_cf.lock() {
                *v = serde_json::Value::Object(cf_map);
            }
            let fd = self.store.fd_plan();
            sh.stats
                .fd_soft_limit
                .store(fd.soft_limit.unwrap_or(0), Ordering::Relaxed);
            sh.stats.fd_reserve.store(fd.reserve, Ordering::Relaxed);
            sh.stats
                .max_open_files_requested
                .store(fd.requested as u64, Ordering::Relaxed);
            sh.stats
                .max_open_files_effective
                .store(fd.effective as u64, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Explain a slow commit group from the log alone. Bulk groups
    /// legitimately run for the full commit interval (20-60 s), so the
    /// thresholds are well above normal bulk cost: twice the configured time
    /// limit, or 5x the previous full group's per-block cost once the group
    /// reaches 10 s. The context — running compactions, L0 files, real
    /// memtables, pending compaction and the Core backlog — is sampled at the
    /// spike so a stall like a 45 s commit can be attributed afterwards from
    /// the log.
    #[allow(clippy::too_many_arguments)]
    fn warn_commit_spike(
        &self,
        d: Duration,
        blocks: u32,
        txs: u64,
        bytes: u64,
        reason: CommitReason,
        tip_height: u64,
        prev_per_block: u64,
    ) {
        let ms = d.as_millis() as u64;
        if ms < 10_000 {
            return;
        }
        let over_limit = ms >= self.tunables.bulk_commit_ms_limit().saturating_mul(2);
        let per_block = ms / blocks.max(1) as u64;
        let over_ewma = per_block >= prev_per_block.max(1_000).saturating_mul(5);
        if !(over_limit || over_ewma) {
            return;
        }
        let c = self.store.compaction_stats();
        let lag = self.tunables.core_height().saturating_sub(tip_height);
        eprintln!(
            "[e-lite] commit spike: {} ms for {} blocks / {} txs ({} MiB), reason {}; \
             rocksdb running {}, l0 {} files, memtable {} MiB, pending_compaction {} MiB; \
             lag {} blocks; previous per-block cost {} ms",
            ms,
            blocks,
            txs,
            bytes / (1024 * 1024),
            reason.name(),
            c.running_compactions,
            c.level0_files,
            c.memtable_bytes / (1024 * 1024),
            c.pending_compaction_bytes / (1024 * 1024),
            lag,
            prev_per_block,
        );
    }

    /// Pause/resume e-lite indexing for the `alternate` scheduler. Serving
    /// (Electrum, Tor, metrics) continues; only block application stops.
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

    /// Track the current scheduler phase and its rate. `phase`: 0 overlap,
    /// 1 Core paused/e-lite draining, 2 Core downloading/e-lite paused.
    fn update_phase(&self, phase: u64, committed: u64) {
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
                2 => "core",
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

    fn set_index_paused(&self, paused: bool) {
        if !self.scheduler_alternate {
            if self.index_paused.swap(false, Ordering::SeqCst) {
                if let Some(sh) = &self.shared {
                    sh.stats
                        .scheduler_index_paused
                        .store(false, Ordering::Relaxed);
                }
            }
            return;
        }
        let previous = self.index_paused.swap(paused, Ordering::SeqCst);
        if previous == paused {
            return;
        }
        if let Some(sh) = &self.shared {
            sh.stats
                .scheduler_index_paused
                .store(paused, Ordering::Relaxed);
        }
        if paused {
            eprintln!(
                "[e-lite] scheduler alternate: e-lite indexing paused; Core downloading \
                 (serving continues)"
            );
        } else {
            eprintln!("[e-lite] scheduler alternate: Core paused; e-lite indexing");
        }
    }

    /// Pause Core's P2P when automatic pruning would outrun the index, or when
    /// the bulk guard is enabled for manual pruning. Core keeps at least
    /// `MIN_BLOCKS_TO_KEEP` (288) blocks, so the auto limit is capped below it.
    ///
    /// For manual pruning the guard uses absolute byte watermarks
    /// (`max_bulk_lag_gib`/`core_resume_ahead_gib`) over the estimated
    /// unindexed backlog. The legacy block window is used only when the GiB
    /// value is 0. In `alternate` mode, e-lite also stops indexing while Core
    /// downloads and resumes when Core is paused.
    fn lag_guard(&self, info: &BlockchainInfo, committed: u64) {
        if !self.can_control_core {
            return;
        }
        let paused = self.lag_paused.load(Ordering::Relaxed);
        let lag = info.blocks.saturating_sub(committed);
        let avg = self.tunables.avg_block_bytes();
        let backlog = backlog_bytes(lag, avg);
        let bulk_blocks = if self.cfg.core.prune_mib == 1 {
            self.cfg.pruning.max_bulk_lag_blocks as u64
        } else {
            0
        };
        let bulk_gib = if self.cfg.core.prune_mib == 1 {
            self.cfg.pruning.max_bulk_lag_gib
        } else {
            0
        };
        let bulk = self.cfg.core.prune_mib == 1 && (bulk_blocks > 0 || bulk_gib > 0);
        let auto_enabled = self.cfg.core.prune_mib != 1 && self.cfg.pruning.max_core_lag_blocks > 0;
        let high_bytes = (bulk_gib > 0).then(|| bulk_gib.saturating_mul(GIB));
        let low_bytes =
            (bulk_gib > 0).then(|| self.cfg.pruning.core_resume_ahead_gib.saturating_mul(GIB));
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
            self.bulk_lag_baseline.store(u64::MAX, Ordering::Relaxed);
            self.set_index_paused(false);
            if paused && !self.downloads_paused() && self.core.set_network_active(true).is_ok() {
                self.lag_paused.store(false, Ordering::Relaxed);
                self.mark_paused(false);
                if let Some(sh) = &self.shared {
                    sh.stats.core_network_active.store(true, Ordering::Relaxed);
                    refresh_ready_state(sh);
                }
                eprintln!("[e-lite] resumed Core downloads");
            }
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
                self.bulk_lag_baseline.store(u64::MAX, Ordering::Relaxed);
                self.bulk_started.store(false, Ordering::Relaxed);
                self.bulk_full_pause.store(false, Ordering::Relaxed);
                self.set_index_paused(false);
                if paused && !self.downloads_paused() && self.core.set_network_active(true).is_ok()
                {
                    self.lag_paused.store(false, Ordering::Relaxed);
                    self.mark_paused(false);
                    if let Some(sh) = &self.shared {
                        sh.stats.core_network_active.store(true, Ordering::Relaxed);
                        refresh_ready_state(sh);
                    }
                    eprintln!("[e-lite] resumed Core downloads after IBD");
                }
                return;
            }
            if let (Some(high), Some(low)) = (high_bytes, low_bytes) {
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
                let ahead = self.cfg.pruning.core_resume_ahead_blocks as u64;
                if self.cfg.pruning.pause_core_during_bulk {
                    let first = !self.bulk_started.swap(true, Ordering::SeqCst);
                    if first && lag > ahead {
                        self.bulk_full_pause.store(true, Ordering::SeqCst);
                        self.tunables.reset_resume_shrink();
                    }
                } else {
                    self.bulk_started.store(true, Ordering::Relaxed);
                }
                if self.bulk_full_pause.load(Ordering::SeqCst) {
                    bulk_full = true;
                    limit = ahead.max(1);
                    resume = ahead.max(1);
                } else if self.lag_policy_drain {
                    let rate = self.tunables.recent_blocks_rate();
                    if rate > 0 {
                        let (pause_at, resume_at) =
                            bulk_drain_window(rate, self.lag_target_secs, ahead.max(1));
                        limit = pause_at;
                        resume = resume_at;
                    } else {
                        let baseline = self.bulk_lag_baseline.load(Ordering::Relaxed);
                        let (new_baseline, pause_at, resume_at) =
                            bulk_lag_window(lag, baseline, bulk_blocks, paused);
                        self.bulk_lag_baseline
                            .store(new_baseline, Ordering::Relaxed);
                        limit = pause_at;
                        resume = resume_at;
                    }
                } else {
                    let baseline = self.bulk_lag_baseline.load(Ordering::Relaxed);
                    let (new_baseline, pause_at, resume_at) =
                        bulk_lag_window(lag, baseline, bulk_blocks, paused);
                    self.bulk_lag_baseline
                        .store(new_baseline, Ordering::Relaxed);
                    limit = pause_at;
                    resume = resume_at;
                }
                pause_due = lag > limit || reader_pressure;
                resume_due = lag <= resume && !reader_pressure;
            }
        } else {
            if !info.pruned {
                self.set_index_paused(false);
                if paused && !self.downloads_paused() && self.core.set_network_active(true).is_ok()
                {
                    self.lag_paused.store(false, Ordering::Relaxed);
                    self.mark_paused(false);
                    if let Some(sh) = &self.shared {
                        sh.stats.core_network_active.store(true, Ordering::Relaxed);
                        refresh_ready_state(sh);
                    }
                    eprintln!("[e-lite] Core is no longer pruned: resumed downloads");
                }
                return;
            }
            if info.pruneheight == 0 {
                self.set_index_paused(false);
                if paused && !self.downloads_paused() && self.core.set_network_active(true).is_ok()
                {
                    self.lag_paused.store(false, Ordering::Relaxed);
                    self.mark_paused(false);
                    if let Some(sh) = &self.shared {
                        sh.stats.core_network_active.store(true, Ordering::Relaxed);
                        refresh_ready_state(sh);
                    }
                    eprintln!("[e-lite] Core has no pruned blocks: resumed downloads");
                }
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

        let baseline = self.bulk_lag_baseline.load(Ordering::Relaxed);
        if !paused && pause_due {
            match self.core.set_network_active(false) {
                Ok(_) => {
                    self.lag_paused.store(true, Ordering::Relaxed);
                    self.mark_paused(true);
                    self.set_index_paused(false);
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
                        } else if high_bytes.is_some() {
                            eprintln!(
                                "[e-lite] lag {lag} blocks, backlog {} GiB (high {} GiB, low \
                                 {} GiB, reader {reader_pressure}): paused Core downloads",
                                backlog / GIB,
                                high_bytes.unwrap_or(0) / GIB,
                                low_bytes.unwrap_or(0) / GIB
                            );
                        } else {
                            eprintln!(
                                "[e-lite] lag {lag} blocks (baseline {baseline}, max growth \
                                 {bulk_blocks}, reader {reader_pressure}): paused Core downloads \
                                 until the index catches up"
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
                self.set_index_paused(false);
                return;
            }
            self.tunables.shrink_for_core_resume(&self.store);
            match self.core.set_network_active(true) {
                Ok(_) => {
                    self.lag_paused.store(false, Ordering::Relaxed);
                    self.mark_paused(false);
                    let prewarm = is_bulk && self.bulk_full_pause.swap(false, Ordering::SeqCst);
                    if is_bulk && prewarm {
                        self.bulk_lag_baseline.store(lag, Ordering::Relaxed);
                    }
                    self.set_index_paused(is_bulk && self.scheduler_alternate);
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
            self.set_index_paused(false);
        } else {
            self.set_index_paused(is_bulk && self.scheduler_alternate);
        }
    }

    fn prune_in_flight(&self) -> bool {
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
    fn run_deferred_prune(&self, supervisor: &mut el_chain::Supervisor) {
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
    fn supervisor_step(&self, supervisor: &mut el_chain::Supervisor) {
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

    /// Index at most `max_blocks` before returning. The supervisor runs at
    /// commit boundaries, so a long catch-up does not need to tear the
    /// pipeline down to prune or check the disk.
    fn catch_up_limited(
        &self,
        max_blocks: u64,
        supervisor: &mut el_chain::Supervisor,
        last_tick: &mut Instant,
    ) -> R<u64> {
        let tip_at_start = self.store.committed_tip()?;
        // Check the disks before any Core RPC or block write. If Core is slow
        // or unreachable, this still stops the indexer before the disk fills.
        if self.disk_guard() {
            return Ok(tip_at_start.map(|t| t.height as u64).unwrap_or(0));
        }
        // The alternate scheduler is the only phase that polls while e-lite is
        // not indexing. Use the 1 Hz observation there to avoid an RPC every
        // 500 ms; in the indexing phase always ask Core so a new block/reorg is
        // seen immediately.
        let info = if self.index_paused.load(Ordering::SeqCst) {
            match self.observed_core(Duration::from_secs(2)) {
                Some(info) => info,
                None => self.core.blockchain_info()?,
            }
        } else {
            self.core.blockchain_info()?
        };
        let core_tip = info.blocks;
        self.tunables
            .set_core_state(core_tip, info.initialblockdownload);
        self.stats_core_height(core_tip);
        if let Some(sh) = &self.shared {
            sh.stats
                .core_pruneheight
                .store(info.pruneheight, Ordering::Relaxed);
            sh.stats.core_pruned.store(info.pruned, Ordering::Relaxed);
            sh.stats
                .core_ibd
                .store(info.initialblockdownload, Ordering::Relaxed);
        }
        // A previous e-lite run may have paused Core and crashed. The marker
        // is only written by e-lite, so resuming is safe. Do not consume a
        // marker written by a *live* lag pause: the guard owns that state and
        // must resume at its own watermark (75% of the limit).
        if self.can_control_core
            && !self.lag_paused.load(Ordering::Relaxed)
            && !self.downloads_paused()
            && self.pause_marker.is_file()
            && self.core.set_network_active(true).is_ok()
        {
            let _ = std::fs::remove_file(&self.pause_marker);
            self.lag_paused.store(false, Ordering::Relaxed);
            if let Some(sh) = &self.shared {
                sh.stats.core_network_active.store(true, Ordering::Relaxed);
            }
            eprintln!("[e-lite] resumed Core downloads after a previous pause");
        }
        if self.is_paused() {
            return Ok(tip_at_start.map(|t| t.height as u64).unwrap_or(0));
        }
        let repair_limit = self
            .resolver
            .as_ref()
            .filter(|r| r.has_repair_sources())
            .map(|_| self.cfg.pruning.repair_max_gap_blocks as u64);
        check_pruned(&info, tip_at_start.as_ref(), repair_limit)?;
        // Pruned IBD from scratch is local-only and requires a Core owned by
        // e-lite, so the lag guard can pause it before it prunes.
        if info.pruned && tip_at_start.is_none() {
            if self.bulk.is_none() {
                return fail(
                    "Core is pruned and the index is empty: pruned IBD requires a local Core \
                     blocks directory (run e-lite on the Core host) and [core] prune_mib = 1",
                );
            }
            if !self.can_control_core {
                return fail(
                    "Core is pruned and the index is empty, but e-lite did not start Core; \
                     stop Core and use [core] manage = \"managed\" with prune_mib = 1, or use a full node",
                );
            }
        }
        self.handle_reorg(core_tip)?;

        // `handle_reorg` may have rewound the database; always continue from
        // the committed tip after the rewind, not from the pre-reorg tip.
        let mut tip = self.store.committed_tip()?;
        let mut committed_height = tip.map(|t| t.height as u64).unwrap_or(0);

        // Automatic pruning (`-prune=NNN`) is decided by Core's own tip. If we
        // fall too far behind, stop new downloads while the backlog is drained.
        let committed = committed_height;
        self.lag_guard(&info, committed);
        let phase = if self.lag_paused.load(Ordering::Relaxed) {
            1
        } else if self.index_paused.load(Ordering::SeqCst) {
            2
        } else {
            0
        };
        self.update_phase(phase, committed);
        if self.index_paused.load(Ordering::SeqCst) {
            // Alternate scheduler: Core is downloading. Keep serving the
            // current index; the next call re-checks the backlog.
            return Ok(committed_height);
        }

        let start = tip.map(|t| t.height as u64 + 1).unwrap_or(0);
        if start > core_tip || max_blocks == 0 {
            self.refresh_core_height();
            return Ok(committed_height);
        }
        let target = core_tip.min(start.saturating_add(max_blocks.saturating_sub(1)));
        let wanted = target - start + 1;
        let hashes = self.core.block_hashes(start, wanted)?;
        if hashes.is_empty() {
            crate::debug_log!(
                "pipeline: block_hashes({start}, {wanted}) returned empty; core tip {core_tip}"
            );
            self.refresh_core_height();
            return Ok(committed_height);
        }
        if wanted >= 64 {
            // Real-work calls only: at the tail the calls repeat every 500 ms
            // and the end-note (rate-limited) carries the diagnosis.
            crate::debug_log!(
                "pipeline: call start {} target {} wanted {} (bulk {})",
                start,
                target,
                wanted,
                self.bulk.is_some()
            );
        }
        let hashes_len = hashes.len();

        // At the tip a call asks for one or two blocks every 500 ms: spawning
        // the full reader pool for that was pure thread churn (AUD-146).
        let workers = self.tunables.readers().min(hashes_len).max(1);
        let hashes = Arc::new(hashes);
        let next_index = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (tx, rx) = std::sync::mpsc::sync_channel::<(usize, FetchResult)>(workers * 2);
        // Bound prepared blocks (fetched/decoded/prevout-mapped) to a small
        // window. The channel alone only bounds queued results; the apply
        // thread can keep draining out-of-order results into `pending`, so an
        // explicit window is needed to cap memory.
        // Bound prepared blocks by both worker count and the configured
        // prefetch depth. A window measured only in blocks ignores that a
        // full block can be 100x heavier than an empty one.
        let default_window = (workers * 2).clamp(4, 32);
        let configured = self.tunables.prefetch_blocks();
        let window = if configured == 0 {
            default_window
        } else {
            default_window.min(configured.clamp(2, 64))
        };
        let (permit_tx, permit_rx) = std::sync::mpsc::sync_channel::<()>(window);
        for _ in 0..window {
            let _ = permit_tx.send(());
        }
        let permit_rx = Arc::new(Mutex::new(permit_rx));
        let stop = Arc::new(AtomicBool::new(false));
        let prefetch_deadline_ms = self.cfg.index.prefetch_deadline_ms;
        let mut readers = Vec::with_capacity(workers);
        for _ in 0..workers {
            let core = self.core.clone();
            let bulk = self.bulk.clone();
            let resolver = self.resolver.clone();
            let store = Arc::clone(&self.store);
            let tunables = Arc::clone(&self.tunables);
            let pruneheight = info.pruneheight;
            let hashes = Arc::clone(&hashes);
            let next_index = Arc::clone(&next_index);
            let stop_thread = Arc::clone(&stop);
            let permit_rx = Arc::clone(&permit_rx);
            let permit_tx = permit_tx.clone();
            let tx = tx.clone();
            readers.push(std::thread::spawn(move || loop {
                if stop_thread.load(Ordering::Relaxed) {
                    break;
                }
                let acquired = permit_rx
                    .lock()
                    .map(|rx| rx.recv().is_ok())
                    .unwrap_or(false);
                if !acquired || stop_thread.load(Ordering::Relaxed) {
                    break;
                }
                let i = next_index.fetch_add(1, Ordering::Relaxed);
                if i >= hashes.len() {
                    // Give the permit back: the apply loop is the only other
                    // producer and this reader is done.
                    let _ = permit_tx.send(());
                    break;
                }
                let h = start + i as u64;
                let read_started = Instant::now();
                let fetched = fetch_block(
                    &core,
                    &bulk,
                    resolver.as_deref(),
                    pruneheight,
                    h,
                    &hashes[i],
                );
                tunables.record_read(read_started.elapsed());
                let item = match fetched {
                    Ok((height, expected_hash, bytes)) => {
                        let decode_started = Instant::now();
                        let prepared = prepare_block_bytes(&bytes, &expected_hash, height as u32);
                        tunables.record_decode(decode_started.elapsed());
                        match prepared {
                            // Resolve prevouts here, in parallel with the apply
                            // thread, so random UTXO reads do not serialise the
                            // whole pipeline. Missing entries fall back to the
                            // overlay/point lookup during apply.
                            Ok(p) => {
                                let prefetch_started = Instant::now();
                                let prevouts = if prefetch_deadline_ms > 0 {
                                    prefetch_prevouts_with_deadline(
                                        &store,
                                        &p,
                                        Duration::from_millis(prefetch_deadline_ms),
                                    )
                                } else {
                                    prefetch_prevouts(&store, &p)
                                };
                                tunables.record_prefetch(prefetch_started.elapsed());
                                prevouts.map(|m| (height, p, m)).map_err(|e| e.to_string())
                            }
                            Err(e) => Err(e.to_string()),
                        }
                    }
                    Err(e) => Err(e),
                };
                if tx.send((i, item)).is_err() {
                    break;
                }
            }));
        }
        drop(tx);

        let mut overlay = GroupOverlay::new();
        let mut batch = WriteBatch::default();
        let mut group_txids: Vec<Hash32> = Vec::new();
        let mut group_scripts: std::collections::HashSet<ScriptHash> =
            std::collections::HashSet::new();
        let mut count = 0u32;
        let mut applied_total = 0u64;
        let mut group_bytes = 0usize;
        let mut last_ms = 0u64;
        // Undo records of the applied-but-uncommitted blocks, trimmed to the
        // reorg window as the group grows. Only these are written at commit
        // (AUD-131); bulk blocks far below the tip never reach RocksDB.
        let mut group_undo: std::collections::VecDeque<el_model::UndoRecord> =
            std::collections::VecDeque::new();
        let undo_window = self.cfg.index.reorg_window;
        let mut result: R<()> = Ok(());
        let mut paused_by_disk = false;
        let mut group_started = Instant::now();
        let mut last_lag_check = Instant::now();
        // Give the supervisor a chance to prune/act even when a 512-block
        // catch-up call is slow; this bounds prune latency independently of
        // the number of blocks processed.
        let supervisor_poll = Duration::from_secs(self.cfg.pruning.poll_seconds.max(1));

        let mut pending: PendingMap = PendingMap::new();
        let mut next_apply = 0usize;
        while next_apply < hashes_len {
            // A prune requested at the previous commit boundary starts here,
            // after the commit and its WAL barrier. It never runs in the middle
            // of an uncommitted group.
            self.run_deferred_prune(supervisor);
            let wait_started = Instant::now();
            // Bound the wait: a wedged reader (a panic dies silently, an RPC
            // can stall) must not block the apply loop forever. The next
            // call has fresh readers; the timeout only makes the stall loud.
            let wait_deadline = wait_started + Duration::from_secs(10);
            while !pending.contains_key(&next_apply) {
                let Some(remaining) = wait_deadline.checked_duration_since(Instant::now()) else {
                    crate::debug_log!(
                        "pipeline: 10 s without block {} from the readers; \
                         ending the call (items pending {})",
                        start + next_apply as u64,
                        pending.len()
                    );
                    break;
                };
                match rx.recv_timeout(remaining) {
                    Ok((i, item)) => {
                        pending.insert(i, item);
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                }
            }
            self.tunables.record_wait(wait_started.elapsed());
            let Some(item) = pending.remove(&next_apply) else {
                break;
            };
            next_apply += 1;
            if self.shutdown.load(Ordering::SeqCst) {
                break;
            }
            if self.is_paused() || self.disk_guard() {
                paused_by_disk = true;
                break;
            }
            if self.index_paused.load(Ordering::SeqCst) {
                // Alternate scheduler switched to Core downloading. Commit the
                // applied prefix below, then return.
                break;
            }
            let (height, prepared, prevouts) = match item {
                Ok(v) => v,
                Err(e) => {
                    let _ = permit_tx.send(());
                    result = Err(e.into());
                    break;
                }
            };
            let t0 = Instant::now();
            // Always generate undo. The previous rule guessed the end of the
            // commit group, but a time limit, a transaction limit or a
            // supervisor-deadline break could commit at a lower height and
            // leave the new tip without its reorg window (or without undo at
            // all). Extra records are pruned inside the same commit.
            let undo_ready = true;
            match apply_prepared_deferred_undo(
                &self.store,
                height as u32,
                &prepared,
                tip,
                &mut overlay,
                &mut batch,
                Some(&prevouts),
            ) {
                Ok(out) => {
                    group_txids.extend_from_slice(&out.txids);
                    let floor = (height as u32).saturating_sub(undo_window);
                    while group_undo.front().is_some_and(|u| u.height < floor) {
                        group_undo.pop_front();
                    }
                    group_undo.push_back(out.undo);
                    if self.shared.is_some() {
                        group_scripts.extend(out.touched_scripts.iter().copied());
                    }
                    tip = Some(out.tip);
                    count += 1;
                    applied_total += 1;
                    group_bytes = group_bytes.saturating_add(prepared.raw_size());
                    self.tunables.record_block_bytes(prepared.raw_size());
                    last_ms = t0.elapsed().as_millis() as u64;
                    self.tunables.record_apply(t0.elapsed());
                    if (self.cfg.core.prune_mib >= 550
                        || (self.cfg.core.prune_mib == 1
                            && self.cfg.pruning.max_bulk_lag_blocks > 0))
                        && info.pruned
                        && last_lag_check.elapsed() >= Duration::from_millis(100)
                    {
                        // Use the 1 Hz observation. Auto-prune keeps a
                        // synchronous fallback (at most every 2 s) because
                        // Core could otherwise outrun a stale snapshot.
                        let max_age = if self.cfg.core.prune_mib >= 550 {
                            Duration::from_secs(2)
                        } else {
                            Duration::from_secs(5)
                        };
                        let live = self.observed_core(max_age).or_else(|| {
                            if self.cfg.core.prune_mib >= 550 {
                                self.core.blockchain_info().ok()
                            } else {
                                None
                            }
                        });
                        if let Some(live) = live {
                            self.stats_core_height(live.blocks);
                            if let Some(sh) = &self.shared {
                                sh.stats
                                    .core_pruneheight
                                    .store(live.pruneheight, Ordering::Relaxed);
                                sh.stats.core_pruned.store(live.pruned, Ordering::Relaxed);
                                sh.stats
                                    .core_ibd
                                    .store(live.initialblockdownload, Ordering::Relaxed);
                            }
                            // Pause decisions use the last *committed* tip, not
                            // applied-but-uncommitted progress.
                            let committed = committed_height;
                            if live.pruneheight > committed.saturating_add(1) {
                                eprintln!(
                                    "[e-lite] WARNING: Core pruned to {} but the index is at {}; \
                                     blocks {}..{} are no longer available",
                                    live.pruneheight,
                                    committed,
                                    committed + 1,
                                    live.pruneheight.saturating_sub(1)
                                );
                            }
                            self.lag_guard(&live, committed);
                        }
                        last_lag_check = Instant::now();
                    }
                }
                Err(e) => {
                    let _ = permit_tx.send(());
                    result = Err(e.into());
                    break;
                }
            }
            let _ = permit_tx.send(());
            // Bulk commits are deliberately infrequent: the prune gate uses the
            // *committed* tip, so a 20-60 s interval cannot lose data that Core
            // has already pruned. A tiny group is not committed on the timer;
            // it waits for a useful minimum (or 60 s) to avoid 1-block commits.
            let commit_elapsed = group_started.elapsed();
            let commit_reason = if count >= self.tunables.batch_blocks().max(1) {
                Some(CommitReason::Blocks)
            } else if group_txids.len() >= self.tunables.max_group_txs() {
                Some(CommitReason::Txs)
            } else if group_bytes >= self.tunables.max_group_bytes() {
                Some(CommitReason::Bytes)
            } else if commit_elapsed >= self.tunables.commit_interval()
                && (count >= self.tunables.commit_min_blocks()
                    || commit_elapsed >= Duration::from_secs(60))
                && undo_ready
            {
                Some(CommitReason::Time)
            } else {
                None
            };
            if let Some(reason) = commit_reason {
                let current = tip.expect("tip set");
                let sync =
                    self.commit_sync(info.initialblockdownload, current.height as u64, core_tip);
                if let Err(e) = self.commit_group(
                    std::mem::take(&mut batch),
                    &current,
                    &group_txids,
                    count,
                    group_bytes as u64,
                    last_ms,
                    group_undo.make_contiguous(),
                    sync,
                    reason,
                ) {
                    result = Err(e);
                    break;
                }
                group_undo.clear();
                committed_height = current.height as u64;
                let (live_core, live_prune) =
                    self.live_core_and_prune(info.blocks, info.pruneheight);
                let rate_secs = self.progress_started.elapsed().as_secs_f64().max(0.001);
                self.tunables.record_tx_rate(
                    ((current.txnum.saturating_sub(self.progress_start_txnum)) as f64 / rate_secs)
                        as u64,
                );
                self.tunables.record_blocks_rate(
                    ((current.height as u64).saturating_sub(self.progress_start_height) as f64
                        / rate_secs) as u64,
                );
                if let Some(sh) = &self.shared {
                    self.tunables.publish(&self.store, &sh.stats);
                }
                self.print_progress(
                    &current,
                    live_core,
                    live_prune,
                    info.headers,
                    info.verificationprogress,
                    info.initialblockdownload,
                );
                self.maybe_milestone(&current);
                if let Some(sh) = &self.shared {
                    for script in group_scripts.drain() {
                        sh.dirty_scripts.mark(script);
                    }
                } else {
                    group_scripts.clear();
                }
                group_txids.clear();
                overlay.clear();
                count = 0;
                group_bytes = 0;
                group_started = Instant::now();
                if last_tick.elapsed() >= supervisor_poll {
                    self.supervisor_step(supervisor);
                    *last_tick = Instant::now();
                }
            }
        }

        // A clean end-of-call always commits the applied prefix. The batch is
        // already built and undo is written for every applied block, so the
        // commit is the cheap step; skipping it made the tail refetch the same
        // blocks every 500 ms call and stall forever a few blocks behind the
        // tip (no commits, no READY). The disk guard still stops the *next*
        // call from fetching more.
        if result.is_ok() && count > 0 {
            if paused_by_disk || self.disk_guard() {
                crate::debug_log!(
                    "pipeline: committing the applied prefix ({} blocks) under \
                     disk pressure before pausing",
                    count
                );
            }
            let current = tip.expect("tip set");
            let sync = self.commit_sync(info.initialblockdownload, current.height as u64, core_tip);
            let reason = if self.shutdown.load(Ordering::SeqCst) {
                CommitReason::Shutdown
            } else {
                CommitReason::Boundary
            };
            self.commit_group(
                std::mem::take(&mut batch),
                &current,
                &group_txids,
                count,
                group_bytes as u64,
                last_ms,
                group_undo.make_contiguous(),
                sync,
                reason,
            )?;
            committed_height = current.height as u64;
            let (live_core, live_prune) = self.live_core_and_prune(info.blocks, info.pruneheight);
            let rate_secs = self.progress_started.elapsed().as_secs_f64().max(0.001);
            self.tunables.record_tx_rate(
                ((current.txnum.saturating_sub(self.progress_start_txnum)) as f64 / rate_secs)
                    as u64,
            );
            self.tunables.record_blocks_rate(
                ((current.height as u64).saturating_sub(self.progress_start_height) as f64
                    / rate_secs) as u64,
            );
            if let Some(sh) = &self.shared {
                self.tunables.publish(&self.store, &sh.stats);
            }
            self.print_progress(
                &current,
                live_core,
                live_prune,
                info.headers,
                info.verificationprogress,
                info.initialblockdownload,
            );
            self.maybe_milestone(&current);
            if let Some(sh) = &self.shared {
                for script in group_scripts.drain() {
                    sh.dirty_scripts.mark(script);
                }
            } else {
                group_scripts.clear();
            }
            if self.shutdown.load(Ordering::SeqCst) {
                eprintln!(
                    "[e-lite] shutdown requested; flushed at height {}",
                    current.height
                );
            }
        }

        stop.store(true, Ordering::Relaxed);
        drop(permit_tx);
        for reader in readers {
            let _ = reader.join();
        }
        // Work prepared but never applied is waste. It is expected at the end
        // of a 512-block call (the window in flight), not every 10 s.
        let mut discarded = pending.len() as u64;
        while rx.try_recv().is_ok() {
            discarded += 1;
        }
        if let Some(sh) = &self.shared {
            sh.stats
                .pipeline_prepared
                .fetch_add(applied_total + discarded, Ordering::Relaxed);
            sh.stats
                .pipeline_applied
                .fetch_add(applied_total, Ordering::Relaxed);
            sh.stats
                .pipeline_discarded
                .fetch_add(discarded, Ordering::Relaxed);
        }
        // Make a no-progress call impossible to miss: this is exactly the tail
        // signature (calls every 500 ms, wanted 2, applied 0, no commits, no
        // READY). Rate-limited so a real stall prints ~6 lines per minute.
        if wanted < 64 && applied_total == 0 {
            let note_at = self.last_pipeline_note.lock().map(|mut s| {
                let due = s
                    .map(|at| at.elapsed() >= Duration::from_secs(10))
                    .unwrap_or(true);
                if due {
                    *s = Some(Instant::now());
                }
                due
            });
            if note_at.unwrap_or(false) {
                crate::debug_log!(
                    "pipeline: no progress: committed {} of target {} (wanted {}, \
                     applied 0, discarded {}, paused_by_disk {}, disk_guard {})",
                    committed_height,
                    target,
                    wanted,
                    discarded,
                    paused_by_disk,
                    self.disk_guard()
                );
            }
        } else if wanted >= 64 {
            crate::debug_log!(
                "pipeline: call end committed {} (wanted {}, applied {}, discarded {})",
                committed_height,
                wanted,
                applied_total,
                discarded
            );
        }
        result?;
        self.refresh_core_height();
        Ok(committed_height)
    }
}

fn fetch_block(
    core: &CoreClient,
    bulk: &Option<Arc<el_blocks::BulkSource>>,
    resolver: Option<&Resolver>,
    pruneheight: u64,
    height: u64,
    hash: &str,
) -> std::result::Result<(u64, String, Vec<u8>), String> {
    let expected = Hash32::from_hex(hash).map_err(|e| e.to_string())?;

    // A prune gap cannot be served by Core or the local block files. Fetch the
    // missing block from a verified cache/P2P/Esplora source instead. This path
    // is never used for heights Core can still serve.
    let mut repair_error = None;
    if height.saturating_add(1) <= pruneheight {
        if let Some(resolver) = resolver {
            match resolver.fetch_block_by_hash(&expected) {
                Ok(bytes) => {
                    eprintln!(
                        "[e-lite] repaired pruned block {height} ({hash}) from a verified source"
                    );
                    return Ok((height, hash.to_string(), bytes));
                }
                Err(e) => repair_error = Some(e.to_string()),
            }
        }
    }

    if let Some(bulk) = bulk {
        if let Ok(Some(bytes)) = bulk.fetch(&expected) {
            return Ok((height, hash.to_string(), bytes));
        }
        // Give the scanner a short window to publish a record Core has just
        // written, then fall back to RPC.
        if bulk
            .wait_for(&expected, Duration::from_millis(50))
            .is_some()
        {
            match bulk.fetch(&expected) {
                Ok(Some(bytes)) => return Ok((height, hash.to_string(), bytes)),
                Ok(None) => {}
                Err(e) => eprintln!("[e-lite] bulk read failed at {height}: {e}"),
            }
        }
    }
    match core.block_hex(hash) {
        Ok(raw) => {
            let bytes = hex::decode(&raw).map_err(|e| e.to_string())?;
            Ok((height, hash.to_string(), bytes))
        }
        Err(e) => {
            let mut msg = e.to_string();
            if let Some(repair) = repair_error {
                msg.push_str("; repair failed: ");
                msg.push_str(&repair);
            }
            Err(msg)
        }
    }
}

impl Indexer {
    fn print_progress(
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
    fn maybe_milestone(&self, tip: &IndexedTip) {
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

fn commit_sync_policy(mode: &str, ibd: bool, height: u64, core_tip: u64) -> bool {
    match mode {
        "always" => true,
        "txids" => false,
        _ => !(ibd || height.saturating_add(1_000) < core_tip),
    }
}

/// Drain-time window: pause Core when the index would need more than
/// `target_secs` to consume the backlog at `rate` blocks/s; resume at half.
/// The thresholds depend only on the measured rate and the configured target,
/// never on the lag observed at resume, so they cannot ratchet upwards.
fn bulk_drain_window(rate: u64, target_secs: u64, min_resume: u64) -> (u64, u64) {
    let pause = rate
        .saturating_mul(target_secs)
        .max(min_resume.saturating_mul(2));
    let resume = (pause / 2).max(min_resume);
    (pause, resume)
}

/// Bulk lag guard window. `u64::MAX` means "no baseline yet". The baseline
/// follows the lowest lag seen **while Core is downloading**. While paused it
/// must stay fixed; otherwise every block indexed would lower the resume
/// threshold and Core would resume immediately.
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

fn bulk_lag_window(lag: u64, baseline: u64, limit: u64, paused: bool) -> (u64, u64, u64) {
    let base = if paused {
        if baseline == u64::MAX {
            lag
        } else {
            baseline
        }
    } else if baseline == u64::MAX || lag < baseline {
        lag
    } else {
        baseline
    };
    (
        base,
        base.saturating_add(limit),
        base.saturating_add(limit / 2),
    )
}

fn dir_usage_bytes(path: &std::path::Path) -> u64 {
    fn walk(p: &std::path::Path, acc: &mut u64) {
        let Ok(rd) = std::fs::read_dir(p) else {
            return;
        };
        for entry in rd.flatten() {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                walk(&entry.path(), acc);
            } else if meta.is_file() {
                *acc = acc.saturating_add(meta.len());
            }
        }
    }
    let mut acc = 0;
    walk(path, &mut acc);
    acc
}

fn supervisor_config(cfg: &Config, can_control_core: bool) -> el_chain::SupervisorConfig {
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
                        .saturating_mul(GIB_BYTES),
                )
            }
        },
    }
}

#[allow(clippy::too_many_arguments)]
fn supervisor_tick(
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
fn refresh_ready_state(shared: &Shared) {
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

fn apply_supervisor_actions(
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

/// Fitted Core `-dbcache` that e-lite must leave out of its own budget. Zero
/// when Core is external or already answering RPC: its cache is already
/// reflected in `MemAvailable`.
fn core_reserve_mib(cfg: &Config, core_running: bool) -> u64 {
    if core_running || cfg.core.manage == CoreManage::External {
        return 0;
    }
    core_plan::operator_dbcache_mib(cfg).unwrap_or_else(|| {
        let mem = tuning::memory_info();
        tuning::fitted_dbcache_mib(mem.total_mib.unwrap_or(1024), mem.available_mib)
    })
}

/// Everything that must be decided before e-lite creates certificates, opens
/// the database or starts Core: memory class, resource plan and Core
/// parameters. It has no side effects, so a rejected configuration leaves the
/// data directory untouched.
fn preflight(cfg: &Config, chain: Chain, require_min_ram: bool, core_running: bool) -> R<()> {
    if require_min_ram {
        tuning::check_minimum_ram(cfg, chain)?;
    }
    let plan = core_plan::plan_core(cfg, chain)?;
    if !plan.args.is_empty() {
        eprintln!("[e-lite] core plan: {}", core_plan::describe(&plan));
    }
    let core_reserve = core_reserve_mib(cfg, core_running);
    let mplan = tuning::MachinePlan::detect_with_core_reserve(cfg, chain, core_reserve);
    if matches!(
        mplan.data_disk.kind,
        tuning::DiskKind::Unknown | tuning::DiskKind::Zfs | tuning::DiskKind::Overlay
    ) {
        let advice = match mplan.data_disk.kind {
            tuning::DiskKind::Zfs => "keep the ZFS ARC capped (e-lite warns below if it is not)",
            tuning::DiskKind::Overlay => {
                "set [index] profile explicitly for the disk under the overlay"
            }
            _ => "set [index] profile explicitly if the disk is not a local SSD",
        };
        eprintln!(
            "[e-lite] WARNING: data disk at {} is {} (fstype {:?}, source {}); using the local-SSD \
             profile; {advice}.",
            mplan.data_disk.mount,
            mplan.data_disk.kind.name(),
            mplan.data_disk.fstype,
            mplan.data_disk.source
        );
        if mplan.data_disk.kind == tuning::DiskKind::Zfs {
            tuning::warn_uncapped_arc(&mplan.mem, "[e-lite] WARNING:");
        }
    }
    let mem = tuning::memory_info();
    let mut b = tuning::budget_with(
        cfg,
        mem.total_mib.unwrap_or(1024),
        mem.available_mib,
        mem.cached_mib,
        None,
        None,
        mplan.core_reserve_mib,
    );
    b.cache_max_mib = b.cache_max_mib.min(mplan.cache_mib).max(8);
    b.cache_start_mib = b.cache_start_mib.min(b.cache_max_mib);
    b.cache_floor_mib = b.cache_floor_mib.min(b.cache_max_mib);
    b.planned_mib = b
        .runtime_mib
        .saturating_add(b.memtables_mib)
        .saturating_add(b.cache_start_mib);
    b.over_budget = b.planned_mib > b.ceiling_mib
        || b.planned_mib.saturating_add(b.reserve_mib) > b.effective_mib;
    eprintln!(
        "[e-lite] memory plan: total {} MiB, available {} MiB, effective {} MiB \
         (core reserve {}), reserve {} MiB, page floor {} MiB, ceiling {} MiB -> \
         cache {}..{} MiB, memtables {} MiB, runtime {} MiB (planned {} MiB)",
        b.total_mib,
        b.available_mib,
        b.effective_mib,
        b.core_reserve_mib,
        b.reserve_mib,
        b.page_cache_floor_mib,
        b.ceiling_mib,
        b.cache_start_mib,
        b.cache_max_mib,
        b.memtables_mib,
        b.runtime_mib,
        b.planned_mib
    );
    if b.over_budget {
        return fail(format!(
            "the memory plan needs {} MiB but only {} MiB fit inside the ceiling; lower \
             [index] cache_mib/write_buffer_mib, raise memory_fraction, or free RAM",
            b.planned_mib, b.ceiling_mib
        ));
    }
    if b.ceiling_mib < 256 {
        return fail(format!(
            "only {} MiB of the memory ceiling is available for the index; raise \
             memory_fraction, lower memory_reserve_mib, or use a larger machine",
            b.ceiling_mib
        ));
    }
    Ok(())
}

/// Background controller: runs independently of the indexing pipeline so a
/// single long `catch_up_limited` call cannot delay memory protection. The
/// step itself is rate-limited by `maybe_adapt`.
fn spawn_controller(
    tunables: Arc<Tunables>,
    store: Arc<Store>,
    shared: Option<Arc<Shared>>,
    flag: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("el-tune".to_string())
        .spawn(move || loop {
            for _ in 0..5 {
                if flag.load(Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
            if flag.load(Ordering::SeqCst) {
                return;
            }
            let committed = store
                .committed_tip()
                .ok()
                .flatten()
                .map(|t| t.height as u64)
                .unwrap_or(0);
            let (mode, lag) = tunables.mode_for(committed);
            if tunables
                .maybe_adapt(mode, lag, &store, shared.as_ref().map(|s| &s.stats))
                .is_some()
            {
                tunables.apply_store(&store);
            }
            // The dynamic ZFS ARC cap runs on its own cadence, independent of
            // the adaptive tuning gate.
            tunables.arc_tick(lag);
        })
        .unwrap_or_else(|e| {
            eprintln!("[e-lite] cannot start the tuning thread: {e}");
            std::thread::spawn(|| {})
        })
}

fn cmd_index(cfg: &Config, chain: Chain) -> R<()> {
    let core_running = core_client(cfg, chain)?.tcp_reachable();
    preflight(cfg, chain, true, core_running)?;
    let started_at = SystemTime::now();
    let started = Instant::now();
    let plan = tuning::MachinePlan::detect_with_core_reserve(
        cfg,
        chain,
        core_reserve_mib(cfg, core_running),
    );
    eprintln!("[e-lite] machine plan: {}", plan.describe());
    let rotational = plan.data_disk.kind == tuning::DiskKind::Hdd;
    let tunables = Arc::new(Tunables::new_with_plan(cfg, rotational, Some(&plan)));
    let store = open_store_with(cfg, chain, tunables.cache_mib(), tunables.background_jobs())?;
    tunables.apply_store(&store);
    warn_if_disk_low(cfg, chain);
    let core = core_client(cfg, chain)?;
    let flag = shutdown_flag();
    // A from-zero managed Core begins downloading immediately, so the slot
    // must be taken before it starts, not after the first index call.
    let from_zero = store.committed_tip().ok().flatten().is_none();
    let initial_lock =
        if from_zero && cfg.core.manage != CoreManage::External && ibd_lock::IbdLock::enabled() {
            ibd_lock::IbdLock::acquire_waiting(&flag, chain)
        } else {
            None
        };
    if flag.load(Ordering::SeqCst) {
        println!("shutdown requested; index stopped");
        return Ok(());
    }
    let mut core_sup = CoreSupervisor::new(cfg, chain);
    let Some(identity) =
        ensure_identity_blocking(cfg, chain, &store, &core, &mut core_sup, &flag, true, None)?
    else {
        println!("shutdown requested; index stopped");
        return Ok(());
    };
    let bulk = build_bulk(cfg, chain, identity.genesis, tunables.readers())?;
    println!(
        "e-lite {}: indexing started at {} (network {})",
        VERSION,
        format_local_time(started_at),
        chain.name()
    );
    let can_control = core_sup.can_control_core();
    let store_for_controller = Arc::clone(&store);
    let tunables_for_controller = Arc::clone(&tunables);
    let controller_flag = Arc::clone(&flag);
    let idx = Indexer::new(
        store,
        core,
        bulk,
        None,
        Arc::clone(&tunables),
        cfg.clone(),
        Arc::new(RwLock::new(())),
        None,
        Arc::clone(&flag),
        None,
        can_control,
        chain,
    );
    let controller = spawn_controller(
        Arc::clone(&tunables_for_controller),
        Arc::clone(&store_for_controller),
        None,
        Arc::clone(&controller_flag),
    );
    let result = index_loop(
        cfg,
        chain,
        None,
        idx,
        &mut core_sup,
        flag,
        !cfg.index.follow,
        initial_lock,
    );
    controller_flag.store(true, Ordering::SeqCst);
    let _ = controller.join();
    core_sup.stop();
    if result.is_ok() {
        println!(
            "[e-lite] stopped at {} (uptime {})",
            format_local_time(SystemTime::now()),
            format_uptime(started.elapsed())
        );
    }
    result
}

fn sleep_with_flag(seconds: u64, flag: &Arc<AtomicBool>) -> bool {
    for _ in 0..seconds {
        if flag.load(Ordering::SeqCst) {
            return false;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    !flag.load(Ordering::SeqCst)
}

/// Wait until Core is reachable and the database identity is verified or
/// initialized. With `require_core = false`, an existing database may be used
/// while Core is down (serving cached data).
#[allow(clippy::too_many_arguments)]
fn ensure_identity_blocking(
    cfg: &Config,
    chain: Chain,
    store: &Store,
    core: &CoreClient,
    core_sup: &mut CoreSupervisor,
    shutdown: &Arc<AtomicBool>,
    require_core: bool,
    shared: Option<&Arc<Shared>>,
) -> R<Option<NetworkIdentity>> {
    // Enforce `managed` ownership before trusting a Core that is already up.
    if cfg.core.manage != CoreManage::External {
        if let Err(se) = core_sup.ensure_started(core) {
            if cfg.core.manage == CoreManage::Managed {
                return fail(format!("cannot start managed Core: {se}"));
            }
            eprintln!("[e-lite] Core start: {se}");
        }
    }
    let stored = stored_identity(store, chain)?;
    if let Some(id) = &stored {
        match core_identity(core, chain) {
            Ok(live) => {
                verify_identity(id, &live)?;
                if let Some(sh) = shared {
                    sh.set_identity(live.clone());
                    sh.set_state(state::BUILDING);
                }
                return Ok(Some(live));
            }
            Err(_) if !require_core => {
                // Offline serve: keep the pinned identity and let the caller
                // print a single "serving cached index" line.
                if let Some(sh) = shared {
                    sh.set_state(state::CORE_UNAVAILABLE);
                }
                return Ok(Some(id.clone()));
            }
            Err(_) => {}
        }
    }

    let mut backoff = 1u64;
    let mut loading_logged = false;
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return Ok(None);
        }
        match core_identity(core, chain) {
            Ok(live) => {
                if let Some(id) = &stored {
                    verify_identity(id, &live)?;
                } else {
                    store.init_identity(&live)?;
                    println!(
                        "initialized {} at {}",
                        chain.name(),
                        cfg.data_dir(chain).display()
                    );
                    println!("genesis: {}", live.genesis);
                }
                if let Some(sh) = shared {
                    sh.set_identity(live.clone());
                    sh.set_state(state::BUILDING);
                }
                return Ok(Some(live));
            }
            Err(e) => {
                let msg = e.to_string();
                let loading = msg.contains("Loading block index")
                    || msg.contains("Verifying blocks")
                    || msg.contains("(code -28)");
                if let Some(sh) = shared {
                    sh.set_state(if stored.is_some() {
                        state::CORE_UNAVAILABLE
                    } else {
                        state::WAITING_FOR_CORE
                    });
                }
                match core_sup.ensure_started(core) {
                    Ok(CoreStartOutcome::Started) => {
                        eprintln!("[e-lite] started Core; waiting for its RPC")
                    }
                    Ok(_) => {}
                    Err(se) if cfg.core.manage == CoreManage::Managed => {
                        return fail(format!("cannot start managed Core: {se}"));
                    }
                    Err(se) => eprintln!("[e-lite] Core start: {se}"),
                }
                if loading {
                    if !loading_logged {
                        eprintln!(
                            "[e-lite] Core is loading its block index; waiting for RPC readiness"
                        );
                        loading_logged = true;
                    }
                } else {
                    eprintln!("[e-lite] waiting for Bitcoin Core ({msg}); retrying in {backoff}s");
                    loading_logged = false;
                }
                if !sleep_with_flag(backoff, shutdown) {
                    return Ok(None);
                }
                backoff = (backoff * 2).min(30);
            }
        }
    }
}

/// Reload the hot subset of the config. Cold fields are reported but require a
/// restart; the last valid config stays in effect on parse/validation errors.
fn reload_hot(
    old: &Config,
    idx: &Indexer,
    supervisor: &mut el_chain::Supervisor,
    shared: Option<&Arc<Shared>>,
    can_control_core: bool,
    chain: Chain,
) {
    let Some(path) = config_path() else {
        eprintln!("[e-lite] reload requested but no config file was resolved");
        return;
    };
    let path_str = path.to_string_lossy().to_string();
    match Config::load(Some(&path_str)) {
        Ok(mut new) => {
            new.apply_network_defaults(chain);
            if let Err(e) = new.validate() {
                eprintln!("[e-lite] reload rejected ({e}); keeping the previous config");
                return;
            }
            let mut changed = idx.tunables.reload(&new);
            idx.tunables.apply_store(&idx.store);
            if let (Some(a), Some(b)) = (
                logging::parse(&new.logging.level),
                logging::parse(&old.logging.level),
            ) {
                if a != b {
                    logging::set_level(a);
                    el_serve::set_debug_logs(a >= logging::DEBUG);
                    el_rawtx::set_debug_logs(a >= logging::DEBUG);
                    changed.push("logging.level");
                }
            }
            if new.server.advertise_electrumx_compat != old.server.advertise_electrumx_compat {
                // New connections get the new banner; open ones keep theirs.
                el_serve::set_electrumx_compat(new.server.advertise_electrumx_compat);
                changed.push("server.advertise_electrumx_compat");
            }
            if changed.contains(&"profile") {
                if let Err(e) = idx
                    .store
                    .set_background_jobs(idx.tunables.background_jobs())
                {
                    eprintln!("[e-lite] reload: cannot set RocksDB background jobs: {e}");
                }
            }
            supervisor.update_config(supervisor_config(&new, can_control_core));
            if let Some(sh) = shared {
                idx.tunables.publish(&idx.store, &sh.stats);
            }
            if new.core.prune_mib != old.core.prune_mib
                || new.index.block_source != old.index.block_source
                || new.index.reorg_window != old.index.reorg_window
                || new.network.data_dir != old.network.data_dir
                || new.server.listen != old.server.listen
                || new.server.tls_listen != old.server.tls_listen
                || new.server.tls_cert != old.server.tls_cert
                || new.server.tls_key != old.server.tls_key
                || new.index.write_buffer_mib != old.index.write_buffer_mib
                || new.index.milestone_blocks != old.index.milestone_blocks
                || new.index.milestone_seconds != old.index.milestone_seconds
                || new.pruning.max_core_lag_blocks != old.pruning.max_core_lag_blocks
                || new.pruning.max_bulk_lag_blocks != old.pruning.max_bulk_lag_blocks
                || new.pruning.max_bulk_lag_gib != old.pruning.max_bulk_lag_gib
                || new.pruning.core_resume_ahead_blocks != old.pruning.core_resume_ahead_blocks
                || new.pruning.core_resume_ahead_gib != old.pruning.core_resume_ahead_gib
                || new.pruning.pause_core_during_bulk != old.pruning.pause_core_during_bulk
                || new.pruning.scheduler != old.pruning.scheduler
            {
                eprintln!(
                    "[e-lite] reload: cold fields changed (prune_mib, block_source, \
                     reorg_window, data_dir, server listeners/TLS, write buffers, \
                     milestones, lag limits/scheduler); restart required"
                );
            }
            eprintln!(
                "[e-lite] reloaded {} hot field(s): {}",
                changed.len(),
                if changed.is_empty() {
                    "no changes".to_string()
                } else {
                    changed.join(", ")
                }
            );
        }
        Err(e) => eprintln!("[e-lite] reload failed ({e}); keeping the previous config"),
    }
}

/// Poll Core once per second into a shared snapshot. The block-application
/// loop then reads that snapshot instead of opening an RPC connection every
/// 100 ms; auto-prune keeps a synchronous fallback for safety.
fn spawn_core_observer(idx: &Indexer, flag: Arc<AtomicBool>) {
    let core = idx.core.clone();
    let obs = idx.core_observation();
    let shared = idx.shared.clone();
    let tunables = Arc::clone(&idx.tunables);
    let _ = std::thread::Builder::new()
        .name("el-core-obs".to_string())
        .spawn(move || {
            while !flag.load(Ordering::SeqCst) {
                if let Ok(info) = core.blockchain_info() {
                    if let Ok(mut guard) = obs.lock() {
                        *guard = Some((info.clone(), Instant::now()));
                    }
                    tunables.set_core_state(info.blocks, info.initialblockdownload);
                    if let Some(sh) = &shared {
                        sh.stats.core_height.store(info.blocks, Ordering::Relaxed);
                        sh.stats.core_headers.store(info.headers, Ordering::Relaxed);
                        sh.stats
                            .core_pruneheight
                            .store(info.pruneheight, Ordering::Relaxed);
                        sh.stats.core_pruned.store(info.pruned, Ordering::Relaxed);
                        sh.stats
                            .core_ibd
                            .store(info.initialblockdownload, Ordering::Relaxed);
                        sh.stats.core_verification_bps.store(
                            (info.verificationprogress.clamp(0.0, 1.0) * 10_000.0) as u64,
                            Ordering::Relaxed,
                        );
                    }
                }
                for _ in 0..10 {
                    if flag.load(Ordering::SeqCst) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        });
}

/// Shared index loop for `index` and `run`. `stop_at_tip` implements
/// `--no-follow`; `run` always follows.
#[allow(clippy::too_many_arguments)]
fn index_loop(
    cfg: &Config,
    chain: Chain,
    shared: Option<Arc<Shared>>,
    idx: Indexer,
    core_sup: &mut CoreSupervisor,
    flag: Arc<AtomicBool>,
    stop_at_tip: bool,
    mut ibd_lock: Option<ibd_lock::IbdLock>,
) -> R<()> {
    let can_control = idx.can_control_core;
    let mut supervisor = el_chain::Supervisor::new(supervisor_config(cfg, can_control));
    let poll = Duration::from_secs(cfg.pruning.poll_seconds.max(1));
    let mut last_tick = Instant::now()
        .checked_sub(poll)
        .unwrap_or_else(Instant::now);
    let mut last_height = u64::MAX;
    let mut was_ready = false;
    let mut was_fully_synced = false;
    // The READY/FULLY SYNCED INFO lines print once per session; later live
    // re-syncs (Core one block ahead, then caught up again) log at debug.
    let mut printed_ready_once = false;
    let mut printed_fully_once = false;
    let mut backoff = 1u64;
    let mut loading_logged = false;
    eprintln!(
        "[e-lite] tuning profile={} readers={} commit={}ms commit_sync={} batch={} cache={}MiB jobs={} prefetch={} adaptive={}",
        idx.tunables.profile().name(),
        idx.tunables.readers(),
        idx.tunables.commit_ms(),
        cfg.index.commit_sync,
        idx.tunables.batch_blocks(),
        idx.tunables.cache_mib(),
        idx.tunables.background_jobs(),
        idx.tunables.prefetch_blocks(),
        idx.tunables.adaptive()
    );
    let (
        mem_total,
        _mem_available,
        mem_effective,
        mem_core_reserve,
        mem_reserve,
        mem_ceiling,
        mem_memtables,
        mem_runtime,
    ) = idx.tunables.memory_numbers();
    let mem_now = tuning::memory_info();
    eprintln!(
        "[e-lite] memory: total {} MiB, available {} MiB, effective {} MiB \
         (core reserve {}), free {} MiB, buff/cache {} MiB (sreclaim {}, shmem {}), \
         arc {}, rss {}, reserve {} MiB, ceiling {} MiB -> cache {} MiB, \
         memtables {} MiB, runtime {} MiB",
        mem_total,
        mem_now.available_mib,
        mem_effective,
        mem_core_reserve,
        mem_now.free_mib,
        mem_now.cached_mib,
        mem_now.sreclaimable_mib,
        mem_now.shmem_mib,
        mem_now
            .arc_mib
            .map(|v| v.to_string())
            .unwrap_or_else(|| "none".to_string()),
        tuning::process_rss_mib(),
        mem_reserve,
        mem_ceiling,
        idx.tunables.cache_mib(),
        mem_memtables,
        mem_runtime
    );
    if idx.lag_policy_drain {
        eprintln!(
            "[e-lite] lag policy=drain target={}s (Core pauses when the index would need \
             more than that to drain the backlog)",
            idx.lag_target_secs
        );
    }
    spawn_core_observer(&idx, Arc::clone(&flag));
    if let Some(sh) = &shared {
        idx.tunables.publish(&idx.store, &sh.stats);
    }

    // Acquire the intensive-sync slot *before* the first 512-block call. The
    // old loop checked only after a call returned; at 1-2 blocks/s that left a
    // ~5-9 minute window where a second instance could index concurrently.
    if ibd_lock.is_none() && ibd_lock::IbdLock::enabled() && idx.core.tcp_reachable() {
        if let Ok(info) = idx.core.blockchain_info() {
            let committed = idx
                .store
                .committed_tip()
                .ok()
                .flatten()
                .map(|t| t.height as u64)
                .unwrap_or(0);
            let lag = info.blocks.saturating_sub(committed);
            if info.initialblockdownload || lag > 5_000 {
                ibd_lock = ibd_lock::IbdLock::acquire_waiting(&flag, chain);
                if flag.load(Ordering::SeqCst) {
                    return Ok(());
                }
            }
        }
    }

    loop {
        if reload_flag().swap(false, Ordering::SeqCst) {
            reload_hot(
                cfg,
                &idx,
                &mut supervisor,
                shared.as_ref(),
                can_control,
                chain,
            );
        }
        if flag.load(Ordering::SeqCst) {
            if shared.is_none() {
                println!("shutdown requested; index stopped");
            }
            return Ok(());
        }
        match idx.catch_up_limited(CATCH_UP_BLOCKS, &mut supervisor, &mut last_tick) {
            Ok(height) => {
                if flag.load(Ordering::SeqCst) {
                    return Ok(());
                }
                backoff = 1;
                loading_logged = false;
                let core_height = shared
                    .as_ref()
                    .map(|s| s.stats.core_height.load(Ordering::Relaxed))
                    .unwrap_or_else(|| idx.tunables.core_height());
                let core_ibd = shared
                    .as_ref()
                    .map(|s| s.stats.core_ibd.load(Ordering::Relaxed))
                    .unwrap_or_else(|| idx.tunables.core_ibd());
                let lag = core_height.saturating_sub(height);
                // Intensive sync = Core IBD or a large backlog. Only one runs
                // per user; the others wait here (still serving) and start on
                // their own once the slot is released.
                let intensive = core_ibd || lag > 5_000;
                if intensive && ibd_lock.is_none() && ibd_lock::IbdLock::enabled() {
                    ibd_lock = ibd_lock::IbdLock::acquire_waiting(&flag, chain);
                    if flag.load(Ordering::SeqCst) {
                        return Ok(());
                    }
                } else if !intensive && lag <= 1_000 {
                    if let Some(lock) = ibd_lock.take() {
                        eprintln!(
                            "[e-lite] caught up; released the intensive-sync slot ({})",
                            lock.path.display()
                        );
                    }
                }
                // The `el-tune` thread is the single owner of adaptation. A
                // second call site here could pass the 30 s gate concurrently
                // and change cache/readers twice.
                if let Some(sh) = &shared {
                    let caught_up = !core_ibd && height >= core_height;
                    let next = if idx.is_paused() || idx.downloads_paused() || supervisor.paused() {
                        state::PAUSE_DOWNLOAD
                    } else if caught_up {
                        state::READY
                    } else {
                        state::BUILDING
                    };
                    sh.set_state(next);
                    let sync = sh.sync_status();
                    let tls = sh
                        .tls_listen
                        .as_ref()
                        .map(|addr| format!(" · SSL {addr}"))
                        .unwrap_or_default();
                    let print_fully = sync.fully_synced && !was_fully_synced;
                    let print_ready = sync.ready && !was_ready && !print_fully;
                    let session = format_uptime(idx.progress_started.elapsed());
                    if print_fully {
                        if !printed_fully_once {
                            eprintln!("{}", readiness_line(sh, &sync, &tls, true, &session));
                            printed_fully_once = true;
                        } else {
                            // In live mode Core gets ahead by a block and the
                            // index re-syncs every few minutes; the first
                            // FULLY SYNCED of the session is the INFO line.
                            crate::debug_log!(
                                "scheduler: fully synced again at height {} (core {})",
                                sync.height,
                                sync.core_height
                            );
                        }
                    } else if print_ready {
                        if !printed_ready_once {
                            eprintln!("{}", readiness_line(sh, &sync, &tls, false, &session));
                            printed_ready_once = true;
                        } else {
                            crate::debug_log!(
                                "scheduler: ready again at height {} (core {})",
                                sync.height,
                                sync.core_height
                            );
                        }
                    }
                    if sync.fully_synced {
                        was_fully_synced = true;
                        was_ready = true;
                    } else if sync.ready {
                        was_ready = true;
                        was_fully_synced = false;
                    } else {
                        was_ready = false;
                        was_fully_synced = false;
                    }
                }
                if stop_at_tip {
                    let core_tip = idx.core.blockchain_info()?.blocks;
                    if height >= core_tip {
                        println!("caught up at height {height}");
                        return Ok(());
                    }
                }
                if height == last_height {
                    std::thread::sleep(Duration::from_millis(500));
                }
                last_height = height;
            }
            Err(e) => {
                let msg = e.to_string();
                match classify_error(&msg) {
                    ErrorClass::Permanent => {
                        if let Some(sh) = &shared {
                            sh.set_state(state::FATAL);
                        }
                        return Err(e);
                    }
                    ErrorClass::Transient => {
                        if let Some(sh) = &shared {
                            sh.set_state(state::CORE_UNAVAILABLE);
                        }
                        let _ = core_sup.ensure_started(&idx.core);
                        let loading = msg.contains("Loading block index")
                            || msg.contains("Verifying blocks")
                            || msg.contains("(code -28)");
                        if loading {
                            if !loading_logged {
                                eprintln!(
                                    "[e-lite] Core is loading its block index; waiting for RPC readiness"
                                );
                                loading_logged = true;
                            }
                        } else {
                            eprintln!(
                                "[e-lite] transient error: {msg}; retrying in {backoff}s \
                                 (serving cached index if available)"
                            );
                            loading_logged = false;
                        }
                        if !sleep_with_flag(backoff, &flag) {
                            return Ok(());
                        }
                        backoff = (backoff * 2).min(30);
                        continue;
                    }
                }
            }
        }
        if idx.is_paused() {
            // Indexing is stopped by the critical disk floor. Still run the
            // supervisor so the prune gate can delete old Core block files and
            // free space; without this the system cannot recover on its own.
            // Check every second, not every poll, because the disk is critical.
            if last_tick.elapsed() >= Duration::from_secs(1) {
                idx.poll_prune(&mut supervisor);
                idx.run_deferred_prune(&mut supervisor);
                match supervisor_tick(
                    &mut supervisor,
                    &idx.core,
                    &idx.store,
                    idx.readers.as_deref(),
                    &cfg.data_dir(chain),
                    &cfg.core_datadir(chain),
                    &cfg.blocks_dir(chain),
                    shared.as_ref(),
                ) {
                    Ok(actions) => apply_supervisor_actions(
                        &mut supervisor,
                        &idx.core,
                        actions,
                        shared.as_ref(),
                        Some(&idx),
                        can_control,
                    ),
                    Err(e) => eprintln!("[e-lite] supervisor: {e}"),
                }
                last_tick = Instant::now();
            } else {
                std::thread::sleep(Duration::from_millis(200));
            }
        } else if last_tick.elapsed() >= poll
            || (idx.downloads_paused() && last_tick.elapsed() >= Duration::from_secs(1))
        {
            idx.poll_prune(&mut supervisor);
            idx.run_deferred_prune(&mut supervisor);
            match supervisor_tick(
                &mut supervisor,
                &idx.core,
                &idx.store,
                idx.readers.as_deref(),
                &cfg.data_dir(chain),
                &cfg.core_datadir(chain),
                &cfg.blocks_dir(chain),
                shared.as_ref(),
            ) {
                Ok(actions) => apply_supervisor_actions(
                    &mut supervisor,
                    &idx.core,
                    actions,
                    shared.as_ref(),
                    Some(&idx),
                    can_control,
                ),
                Err(e) => eprintln!("[e-lite] supervisor: {e}"),
            }
            last_tick = Instant::now();
        }
    }
}

/// A pruned Core can serve only if the local index already covers the gap.
fn check_pruned(
    info: &BlockchainInfo,
    tip: Option<&IndexedTip>,
    repair_max_gap: Option<u64>,
) -> R<()> {
    if !info.pruned {
        return Ok(());
    }
    let tip = match tip {
        Some(t) => t,
        None => {
            // A local managed Core can bootstrap while it has not pruned yet.
            if info.pruneheight == 0 {
                return Ok(());
            }
            return fail(
                "Core is pruned and the index is empty: use a full node, or let e-lite own Core \
                 with [core] prune_mib = 1 (manual pruning) so it can index before pruning",
            );
        }
    };
    if (tip.height as u64) + 1 < info.pruneheight {
        let first = tip.height as u64 + 1;
        let gap = info.pruneheight.saturating_sub(first);
        if let Some(max) = repair_max_gap {
            if gap <= max {
                eprintln!(
                    "[e-lite] prune gap of {gap} blocks ({first}..{}): fetching verified blocks \
                     from the repair sources",
                    info.pruneheight.saturating_sub(1)
                );
                return Ok(());
            }
        }
        return fail(format!(
            "Core is pruned from height {} but the index stops at {}; blocks {}..{} are unavailable \
             (gap {}). Use a full node, enable a verified P2P/Esplora repair source, or use \
             [core] prune_mib = 1 so Core cannot create this gap.",
            info.pruneheight,
            tip.height,
            first,
            info.pruneheight.saturating_sub(1),
            gap
        ));
    }
    Ok(())
}

fn build_resolver(
    cfg: &Config,
    chain: Chain,
    store: &Arc<Store>,
    core: &CoreClient,
    esplora_urls: Vec<String>,
    electrum_servers: Vec<String>,
) -> R<Arc<Resolver>> {
    let mut peers = cfg.rawtx.p2p_peers.clone();
    let mut hints = Vec::new();
    if p2p_discovery_enabled(cfg) {
        let (found, connected) = discover_p2p_peers(core);
        peers = found.iter().map(|p| p.addr.clone()).collect();
        if !found.is_empty() {
            println!(
                "p2p peers discovered from Core: {} ({} connected)",
                found.len(),
                connected
            );
        }
        hints = found;
    }
    let tor = cfg.rawtx.tor_socks.clone().filter(|s| !s.trim().is_empty());
    // privacy.toml (AUD-163): created from the template on the first run; an
    // invalid file stops the start with the file, the key and the range.
    let privacy_loaded = privacy::load(cfg, config_path().as_deref(), true)?;
    eprintln!("[e-lite] privacy: {}", privacy::summary(&privacy_loaded));
    for w in &privacy_loaded.warnings {
        eprintln!("[e-lite] WARNING: privacy: {w}");
    }
    let privacy_params = privacy_loaded.params.clone();
    let rc = RawtxConfig {
        enabled: cfg.rawtx.enabled,
        cache_mib: cfg.rawtx.cache_mib,
        decoys: privacy_params.block_decoys,
        tor_socks: tor,
        allow_clearnet: cfg.rawtx.allow_clearnet,
        fallback_clearnet: cfg.rawtx.tor_fallback_clearnet,
        request_timeout_seconds: cfg.rawtx.request_timeout_seconds,
        tor_request_timeout_seconds: cfg.rawtx.tor_request_timeout_seconds,
        tor_connect_timeout_seconds: cfg.rawtx.tor_connect_timeout_seconds,
        tor_stall_timeout_seconds: cfg.rawtx.tor_stall_timeout_seconds,
        provider_cooldown_seconds: cfg.rawtx.provider_cooldown_seconds,
        max_parallel_fetches: cfg.rawtx.max_parallel_fetches,
        cache_dir: if cfg.rawtx.cache_dir.trim().is_empty() {
            None
        } else {
            Some(std::path::PathBuf::from(cfg.rawtx.cache_dir.trim()))
        },
        use_p2p: cfg.rawtx.use_p2p,
        use_esplora: cfg.rawtx.use_esplora,
        p2p_v2: cfg.rawtx.p2p_v2,
        esplora_urls: esplora_urls.clone(),
        p2p_peers: peers,
        p2p_timeout: Duration::from_secs(cfg.rawtx.p2p_timeout_seconds.max(1)),
        p2p_tor_timeout: Duration::from_secs(cfg.rawtx.p2p_tor_timeout_seconds.max(1)),
        chain,
        privacy: privacy_params.clone(),
        electrum_servers,
    };
    let resolver = Resolver::new(Arc::clone(store), &rc)?;
    match resolver.tx_mode_servers() {
        Some(n) => {
            eprintln!(
                "[e-lite] privacy: per-txid mode over {n} servers ({} electrum, {} esplora), \
                 {} decoy wallets; whole blocks as fallback",
                rc.electrum_servers.len(),
                rc.esplora_urls.len(),
                privacy_params.decoy_wallets
            );
            if n < privacy_params.min_servers {
                eprintln!(
                    "[e-lite] WARNING: privacy: sessions.min_servers = {} but only {n} servers are \
                     configured; transactions are spread over the ones available",
                    privacy_params.min_servers
                );
            }
        }
        None if privacy_params.mode == el_rawtx::ExternalMode::Tx && cfg.rawtx.enabled => {
            eprintln!(
                "[e-lite] privacy: per-txid mode unavailable (no Electrum/Esplora server reachable \
                 through Tor); whole blocks only"
            );
        }
        None => {}
    }
    if !hints.is_empty() {
        // Same peers, plus Core's view of their transport (v1/v2).
        resolver.refresh_p2p_peers(hints);
    }
    Ok(Arc::new(
        resolver.with_local(Arc::new(CoreBlockSource::new(core.clone()))),
    ))
}

/// Peers are discovered from Core only when the operator did not pin
/// `[rawtx] p2p_peers`.
fn p2p_discovery_enabled(cfg: &Config) -> bool {
    cfg.rawtx.enabled && cfg.rawtx.use_p2p && cfg.rawtx.p2p_peers.is_empty()
}

/// Up to 8 peers from Core that can serve old witness blocks: its current
/// **outbound** connections first (addrman holds many stale addresses and
/// each dead peer costs a timeout), then addrman. Returns `(peers,
/// connected)`.
fn discover_p2p_peers(core: &CoreClient) -> (Vec<el_rawtx::PeerHint>, usize) {
    let connected = core.connected_peers().unwrap_or_default();
    let need_addrman = select_p2p_peers(&connected, &[]).0.len() < P2P_PEERS;
    let addrman = if need_addrman {
        core.node_addresses(64).unwrap_or_default()
    } else {
        Vec::new()
    };
    select_p2p_peers(&connected, &addrman)
}

const P2P_PEERS: usize = 8;

/// Whether e-lite can reach a peer of this Core network: through Tor or
/// directly, never I2P/CJDNS (`""` = an old Core that does not say).
fn p2p_network_reachable(network: &str) -> bool {
    matches!(network, "" | "ipv4" | "ipv6" | "onion")
}

/// Filter and order Core's peers for block fetches (AUD-154):
/// - inbound peers are skipped: their `addr` is an ephemeral source port, a
///   connection to it is refused (and used to trip the Tor breaker);
/// - `NODE_NETWORK` and `NODE_WITNESS` are required (old witness blocks);
/// - I2P/CJDNS are unreachable through Tor;
/// - onion peers go first so they survive the cap: they need no exit node,
///   and exit policies often reject port 8333.
fn select_p2p_peers(
    connected: &[el_rpc::PeerInfo],
    addrman: &[el_rpc::NodeAddress],
) -> (Vec<el_rawtx::PeerHint>, usize) {
    const NETWORK_AND_WITNESS: u64 = 1 | (1 << 3);
    let full = |services: u64| services & NETWORK_AND_WITNESS == NETWORK_AND_WITNESS;
    let mut live: Vec<(bool, el_rawtx::PeerHint)> = connected
        .iter()
        .filter(|p| !p.inbound && full(p.services) && p2p_network_reachable(&p.network))
        .map(|p| {
            let v2 = match p.transport_protocol_type.as_str() {
                "v2" => Some(true),
                "v1" => Some(false),
                _ => None,
            };
            (
                p.network == "onion" || p.addr.contains(".onion"),
                el_rawtx::PeerHint {
                    addr: p.addr.clone(),
                    v2,
                },
            )
        })
        .collect();
    live.sort_by_key(|(onion, _)| !onion);
    let mut peers: Vec<el_rawtx::PeerHint> = live.into_iter().map(|(_, p)| p).collect();
    let connected_count = peers.len();
    let mut known: Vec<(bool, el_rawtx::PeerHint)> = addrman
        .iter()
        .filter(|a| full(a.services) && p2p_network_reachable(&a.network))
        .map(|a| {
            let host = if a.address.contains(':') && !a.address.starts_with('[') {
                format!("[{}]", a.address)
            } else {
                a.address.clone()
            };
            (
                a.network == "onion" || a.address.ends_with(".onion"),
                el_rawtx::PeerHint::from(format!("{host}:{}", a.port)),
            )
        })
        .collect();
    known.sort_by_key(|(onion, _)| !onion);
    peers.extend(known.into_iter().map(|(_, p)| p));
    let mut seen = std::collections::HashSet::new();
    peers.retain(|peer| seen.insert(peer.addr.clone()));
    peers.truncate(P2P_PEERS);
    let connected_count = connected_count.min(peers.len());
    (peers, connected_count)
}

/// How long until the next peer refresh: soon while the list is empty (Core
/// just started), otherwise every 10 minutes so dead peers are replaced
/// before every fetch falls to Esplora.
fn p2p_refresh_interval(peers: usize) -> Duration {
    if peers == 0 {
        Duration::from_secs(60)
    } else {
        Duration::from_secs(600)
    }
}

/// Refresh the P2P peer list from Core for the whole session (AUD-137).
fn spawn_p2p_refresh(shared: Arc<Shared>, cfg: &Config) {
    if !p2p_discovery_enabled(cfg) || shared.resolver.p2p_peers().is_none() {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("el-p2p-peers".to_string())
        .spawn(move || loop {
            let current = shared.resolver.p2p_peers().unwrap_or(0);
            let wait = p2p_refresh_interval(current);
            let started = Instant::now();
            while started.elapsed() < wait {
                if shared.shutdown.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
            let (peers, connected) = discover_p2p_peers(&shared.core);
            // Keep the old list when Core is unreachable or reports none.
            if peers.is_empty() {
                continue;
            }
            let count = shared.resolver.refresh_p2p_peers(peers).unwrap_or(0);
            if current == 0 {
                eprintln!(
                    "[e-lite] p2p peers discovered from Core: {count} ({connected} connected)"
                );
            } else {
                crate::debug_log!("p2p peers refreshed from Core: {count} ({connected} connected)");
            }
        });
    if let Err(e) = spawned {
        eprintln!("[e-lite] cannot start the p2p peer refresh thread: {e}");
    }
}

fn requires_ready_provider(cfg: &Config, chain: Chain, has_esplora: bool) -> bool {
    // Only an Esplora endpoint can be probed. With the per-txid path alone
    // (Electrum servers, no Esplora) or with no sources at all, waiting could
    // never succeed and would keep the wallet listeners closed forever
    // (AUD-167). The caller reports the effective mode.
    cfg.rawtx.enabled
        && cfg.rawtx.use_esplora
        && has_esplora
        && cfg.public_servers.require_ready
        && chain != Chain::Regtest
}

/// Whether `run` holds the wallet listeners for a provider (AUD-127). An
/// unpruned Core serves every raw transaction locally, so public providers
/// are never needed for wallet data; `core_pruned = None` (Core did not
/// answer) keeps the conservative gate.
fn provider_gate_required(
    cfg: &Config,
    chain: Chain,
    core_pruned: Option<bool>,
    has_esplora: bool,
) -> bool {
    requires_ready_provider(cfg, chain, has_esplora) && core_pruned != Some(false)
}

/// With `open_without_provider` the gate gives up after
/// `ready_timeout_seconds` and opens the listeners with local data. By default
/// it stays fail-closed: a pruned Core plus a failed `transaction.get` makes
/// Sparrow persist an unfetchable transaction (AUD-045).
fn provider_gate_expired(cfg: &Config, waited: Duration) -> bool {
    cfg.public_servers.open_without_provider
        && waited >= Duration::from_secs(cfg.public_servers.ready_timeout_seconds)
}

/// Keep probing in the background after the listeners opened without a
/// ready provider, so `/status.public_servers.provider_ready` and the log
/// reflect a provider that comes back. Only constant, wallet-free requests
/// are sent (`/block-height/0` and the genesis block).
fn spawn_provider_reprobe(shared: Arc<Shared>, cfg: Config) {
    if !cfg.rawtx.enabled || !cfg.rawtx.use_esplora || shared.public_servers.is_empty() {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("el-providers".to_string())
        .spawn(move || {
            while !shared.provider_ready.load(Ordering::Relaxed) {
                for _ in 0..60 {
                    if shared.shutdown.load(Ordering::Relaxed) {
                        return;
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
                refresh_providers(&shared, &cfg, false);
            }
            eprintln!("[e-lite] a public provider answered; raw transactions of pruned blocks are available again");
        });
    if let Err(e) = spawned {
        eprintln!("[e-lite] cannot start the provider re-probe thread: {e}");
    }
}

fn effective_rate_limit(cfg: &Config) -> (u64, u64) {
    // Same generous budget for every client. The limiter delays instead of
    // rejecting, so an initial wallet scan never aborts; a sustained abuser is
    // still capped. `rate_limit = 0` disables it entirely.
    (
        cfg.server.rate_limit.unwrap_or(200),
        cfg.server.rate_burst.unwrap_or(5000),
    )
}

/// One-time `debug:` lines that make a log self-describing: which banner is
/// served (so a run proves whether `advertise_electrumx_compat` reached the
/// process), what the rawtx cache held at startup (cold vs warm) and the
/// effective provider limits. No per-request cost.
fn log_startup_diagnostics(cfg: &Config, chain: Chain, shared: &Shared) {
    if !logging::debug_enabled() {
        return;
    }
    eprintln!(
        "[e-lite] debug: server: banner {:?} | electrumx compat {} | batches max {} items, \
         {} workers ({} with transaction.get), pipeline {} | plain {} | tls {}",
        el_serve::banner_for_log(),
        if cfg.server.advertise_electrumx_compat {
            "on (opt-in)"
        } else {
            "off"
        },
        el_serve::MAX_BATCH_REQUESTS,
        el_serve::BATCH_WORKERS,
        el_serve::BATCH_IO_WORKERS,
        el_serve::PIPELINE_DEPTH,
        cfg.server.listen,
        if cfg.server.tls_enabled() {
            cfg.server.tls_listen.as_str()
        } else {
            "disabled"
        },
    );
    let (block_mib, tx_mib) = el_rawtx::cache_split(cfg.rawtx.cache_mib);
    let inv = shared.resolver.cache_inventory();
    eprintln!(
        "[e-lite] debug: rawtx cache: {} | blocks {} files {} / {} MiB | txs {} files {} / {} MiB",
        shared.resolver.cache_dir().display(),
        inv.block_files,
        display_size(inv.block_bytes),
        block_mib,
        inv.tx_files,
        display_size(inv.tx_bytes),
        tx_mib,
    );
    eprintln!(
        "[e-lite] debug: rawtx: probe {}s (tor >= {}s), ready {}s, tor connect/stall/attempt \
         {}/{}/{}s (onion x2), p2p {}s (tor {}s), race {} (hedge tor {}/{}s, direct {}/{}s), \
         prefetch {}, decoys {}, max_parallel {}, clearnet fallback {}",
        cfg.public_servers.probe_timeout_seconds,
        public_servers::probe_tor_floor(cfg).as_secs(),
        cfg.public_servers.ready_timeout_seconds,
        cfg.rawtx.tor_connect_timeout_seconds,
        cfg.rawtx.tor_stall_timeout_seconds,
        cfg.rawtx.tor_request_timeout_seconds,
        cfg.rawtx.p2p_timeout_seconds,
        cfg.rawtx.p2p_tor_timeout_seconds,
        el_rawtx::RACE_WIDTH,
        el_rawtx::HEDGE_FIRST_BYTE_TOR.as_secs(),
        el_rawtx::HEDGE_SLOW_TOR.as_secs(),
        el_rawtx::HEDGE_FIRST_BYTE_DIRECT.as_secs(),
        el_rawtx::HEDGE_SLOW_DIRECT.as_secs(),
        if cfg.rawtx.prefetch { "on" } else { "off" },
        cfg.rawtx.decoys,
        cfg.rawtx.max_parallel_fetches,
        if cfg.rawtx.tor_fallback_clearnet {
            "on"
        } else {
            "off"
        },
    );
    eprintln!(
        "[e-lite] debug: network: {} (same data dir and Core per network)",
        chain.name()
    );
}

fn build_shared(
    cfg: &Config,
    chain: Chain,
    store: Arc<Store>,
    identity: NetworkIdentity,
) -> R<Arc<Shared>> {
    let core = core_client(cfg, chain)?;
    let resolved = public_servers::load(cfg, chain, crate::config_path().as_deref());
    let public_servers = if !cfg.rawtx.esplora_urls.is_empty() {
        cfg.rawtx.esplora_urls.clone()
    } else {
        resolved.endpoints.clone()
    };
    let resolver = build_resolver(
        cfg,
        chain,
        &store,
        &core,
        public_servers.clone(),
        resolved.electrum.clone(),
    )?;
    let readers = el_chain::ReaderTracker::new();
    // Opt-in (AUD-134). P2P may gain peers later, so the configured sources
    // count, not the ones usable right now.
    let prefetch = if cfg.rawtx.prefetch && cfg.rawtx.enabled {
        // Every download slot but the one kept for the wallet (AUD-160).
        el_serve::Prefetcher::new(
            Arc::clone(&resolver),
            Arc::clone(&readers),
            cfg.rawtx.max_parallel_fetches.saturating_sub(1).max(1),
            el_serve::PREFETCH_QUEUE,
        )
    } else {
        el_serve::Prefetcher::disabled()
    };
    let log_file = cfg.app_log_file(chain);
    let banner_file = cfg.app_banner_file(chain);
    if let Some(path) = &banner_file {
        if let Err(e) = el_serve::ensure_banner_file(path) {
            eprintln!(
                "[e-lite] WARNING: cannot create banner template {}: {e}",
                path.display()
            );
        }
    }
    let stats = Stats::default();
    if let Ok(Some(tip)) = store.committed_tip() {
        stats
            .indexed_height
            .store(tip.height as u64, Ordering::Relaxed);
        stats.txnum.store(tip.txnum, Ordering::Relaxed);
    }
    if let Ok(info) = core.blockchain_info() {
        stats.core_height.store(info.blocks, Ordering::Relaxed);
        stats.core_headers.store(info.headers, Ordering::Relaxed);
        stats
            .core_pruneheight
            .store(info.pruneheight, Ordering::Relaxed);
        stats.core_pruned.store(info.pruned, Ordering::Relaxed);
        stats
            .core_ibd
            .store(info.initialblockdownload, Ordering::Relaxed);
        stats.core_verification_bps.store(
            (info.verificationprogress.clamp(0.0, 1.0) * 10_000.0) as u64,
            Ordering::Relaxed,
        );
    }
    // Publish the scheduler configuration even in `serve`, where no Indexer
    // exists. `index_paused` stays false because nothing is indexing.
    stats.scheduler_alternate.store(
        cfg.pruning.scheduler.eq_ignore_ascii_case("alternate"),
        Ordering::Relaxed,
    );
    stats.scheduler_high_bytes.store(
        cfg.pruning.max_bulk_lag_gib.saturating_mul(GIB),
        Ordering::Relaxed,
    );
    stats.scheduler_low_bytes.store(
        cfg.pruning.core_resume_ahead_gib.saturating_mul(GIB),
        Ordering::Relaxed,
    );
    let shared = Arc::new(Shared {
        store,
        core,
        resolver,
        public_servers,
        tx_sources: resolved.electrum.len(),
        provider_status: Mutex::new(Vec::new()),
        provider_ready: AtomicBool::new(false),
        tor_ready: AtomicBool::new(false),
        clearnet_fallback: AtomicBool::new(false),
        prefetch,
        mempool: Arc::new(RwLock::new(Mempool::new())),
        relayfee: Mutex::new(0.0),
        fee_estimates: Mutex::new(std::collections::HashMap::new()),
        error_log: Mutex::new(std::collections::HashMap::new()),
        clients: Arc::new(el_serve::ClientRegistry::new()),
        readers,
        silent_payments_enabled: cfg.silent_payments.enabled,
        identity: RwLock::new(identity),
        headers: el_serve::HeaderCache::new(),
        db_bytes: Mutex::new(el_serve::DbBytesCache::default()),
        dirty_scripts: el_serve::DirtyScripts::new(),
        data_dir: cfg.data_dir(chain),
        core_datadir: cfg.core_datadir(chain),
        log_file,
        listen: cfg.server.listen.clone(),
        tls_listen: if cfg.server.tls_enabled() {
            Some(cfg.server.tls_listen.clone())
        } else {
            None
        },
        onion: Mutex::new(None),
        onion_plain_port: tor::effective_onion_ports(cfg).0,
        onion_tls_port: tor::effective_onion_ports(cfg).1.unwrap_or(0),
        banner_file,
        max_clients: cfg.server.max_clients,
        state: std::sync::atomic::AtomicU8::new(state::WAITING_FOR_CORE),
        shutdown: shutdown_flag(),
        lock: Arc::new(RwLock::new(())),
        stats,
    });
    let (rate_limit, rate_burst) = effective_rate_limit(cfg);
    shared.clients.set_rate_limit(rate_limit, rate_burst);
    if rate_limit > 0 {
        eprintln!(
            "[e-lite] Electrum rate limit: {rate_limit} req/s per client, burst {rate_burst}"
        );
    }
    publish_memory(&shared);
    spawn_p2p_refresh(Arc::clone(&shared), cfg);
    Ok(shared)
}

fn refresh_providers(shared: &Shared, cfg: &Config, wait: bool) {
    if !cfg.rawtx.enabled || !cfg.rawtx.use_esplora || shared.public_servers.is_empty() {
        return;
    }
    let expected = shared.identity().genesis.to_hex();
    let probe = public_servers::probe(cfg, &shared.public_servers, wait, Some(expected.as_str()));
    if probe.clearnet_fallback {
        if probe.tor_ready {
            eprintln!(
                "[e-lite] WARNING: some public providers used clearnet because their Tor request failed; see /status"
            );
        } else {
            eprintln!(
                "[e-lite] WARNING: Tor is unavailable; using clearnet public providers (operator IP exposed)"
            );
        }
    }
    if let Ok(mut statuses) = shared.provider_status.lock() {
        *statuses = probe
            .statuses
            .iter()
            .map(public_servers::ProviderStatus::to_json)
            .collect();
    }
    shared.provider_ready.store(probe.ready, Ordering::Relaxed);
    shared.tor_ready.store(probe.tor_ready, Ordering::Relaxed);
    shared
        .clearnet_fallback
        .store(probe.clearnet_fallback, Ordering::Relaxed);
}

fn publish_memory(shared: &Shared) {
    let mem = tuning::memory_info();
    shared
        .stats
        .mem_total_mib
        .store(mem.total_mib.unwrap_or(0), Ordering::Relaxed);
    shared
        .stats
        .mem_available_mib
        .store(mem.available_mib, Ordering::Relaxed);
    shared
        .stats
        .mem_cgroup_limit_mib
        .store(mem.cgroup_limit_mib.unwrap_or(0), Ordering::Relaxed);
    shared
        .stats
        .mem_cgroup_used_mib
        .store(mem.cgroup_used_mib.unwrap_or(0), Ordering::Relaxed);
    shared
        .stats
        .mem_reclaimable_mib
        .store(mem.reclaimable_mib.unwrap_or(0), Ordering::Relaxed);
    shared
        .stats
        .mem_cached_mib
        .store(mem.cached_mib, Ordering::Relaxed);
}

fn is_loopback_listen(addr: &str) -> bool {
    let host = addr.rsplit_once(':').map(|(h, _)| h).unwrap_or(addr);
    host == "localhost" || host == "[::1]" || host == "::1" || host.starts_with("127.")
}

fn warn_public_listen(what: &str, addr: &str) {
    if !is_loopback_listen(addr) {
        eprintln!(
            "[e-lite] WARNING: {what} is listening on {addr}, not loopback. \
             {what} has no transport encryption or client authentication; use a \
             firewall, an SSH/VPN tunnel, a TLS reverse proxy or Tor, and restrict \
             who can connect."
        );
    }
}

fn start_tls_transport(shared: &Arc<Shared>, cfg: &Config) -> R<()> {
    let watcher = el_serve::CertWatcher::load(
        std::path::PathBuf::from(&cfg.server.tls_cert),
        std::path::PathBuf::from(&cfg.server.tls_key),
        el_serve::TLS_CERT_CHECK_INTERVAL,
    )
    .map_err(|e| format!("TLS configuration: {e}"))?;
    if !is_loopback_listen(&cfg.server.tls_listen) {
        eprintln!(
            "[e-lite] WARNING: Electrum TLS is listening on {} without client \
             authentication. TLS encrypts the connection but does not restrict who \
             can connect; use a firewall, SSH/VPN or Tor.",
            cfg.server.tls_listen
        );
    }
    let listener = std::net::TcpListener::bind(&cfg.server.tls_listen)
        .map_err(|e| format!("cannot bind TLS {}: {e}", cfg.server.tls_listen))?;
    el_serve::start_listener_on(listener, Arc::clone(shared), Some(Arc::new(watcher)));
    eprintln!(
        "[e-lite] Electrum TLS listening on {} (cert {})",
        cfg.server.tls_listen, cfg.server.tls_cert
    );
    Ok(())
}

struct Transports {
    mempool: std::thread::JoinHandle<()>,
    notifications: std::thread::JoinHandle<()>,
    memory: std::thread::JoinHandle<()>,
}

fn start_transports(shared: &Arc<Shared>, cfg: &Config) -> R<Transports> {
    warn_public_listen("Electrum", &cfg.server.listen);
    warn_public_listen("metrics", &cfg.metrics.listen);
    let server = Arc::clone(shared);
    el_serve::start_server(server).map_err(|e| format!("cannot bind {}: {e}", shared.listen))?;
    if cfg.server.tls_enabled() {
        start_tls_transport(shared, cfg)?;
    }
    if cfg.metrics.enabled {
        let m = Arc::clone(shared);
        el_serve::start_metrics(m, cfg.metrics.listen.clone())
            .map_err(|e| format!("cannot bind metrics {}: {e}", cfg.metrics.listen))?;
    }
    let mempool = el_serve::start_mempool(Arc::clone(shared));
    let notifications = el_serve::start_notifications(Arc::clone(shared));
    let memory = {
        let shared = Arc::clone(shared);
        std::thread::spawn(move || loop {
            publish_memory(&shared);
            if shared.shutdown.load(Ordering::Relaxed) {
                break;
            }
            for _ in 0..30 {
                if shared.shutdown.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        })
    };
    Ok(Transports {
        mempool,
        notifications,
        memory,
    })
}

fn cmd_serve(cfg: &Config, chain: Chain) -> R<()> {
    // `serve` never starts Core: do not reserve a future `-dbcache`.
    preflight(cfg, chain, false, true)?;
    let started_at = SystemTime::now();
    let started = Instant::now();
    let mut prepared = cfg.clone();
    // TLS does not depend on Tor. Generate or reuse the certificate first so a
    // Tor failure only costs the onion service.
    let auto_cert = tls::prepare(&mut prepared, chain)?;
    if let Err(e) = tor::prepare(&mut prepared, chain) {
        eprintln!("[e-lite] WARNING: managed Tor unavailable: {e}");
    }
    let cfg = &prepared;
    let store = open_store(cfg, chain)?;
    let core = core_client(cfg, chain)?;
    let flag = shutdown_flag();
    // `serve` is the offline mode: never start or stop Core.
    let mut serve_cfg = cfg.clone();
    serve_cfg.core.manage = CoreManage::External;
    let mut core_sup = CoreSupervisor::new(&serve_cfg, chain);
    let Some(identity) = ensure_identity_blocking(
        &serve_cfg,
        chain,
        &store,
        &core,
        &mut core_sup,
        &flag,
        false,
        None,
    )?
    else {
        println!("[e-lite] shutdown requested; serving stopped");
        return Ok(());
    };
    let shared = build_shared(cfg, chain, Arc::clone(&store), identity)?;
    log_startup_diagnostics(cfg, chain, &shared);
    {
        let plan = tuning::MachinePlan::detect_with_core_reserve(cfg, chain, 0);
        if let Ok(mut p) = shared.stats.plan.lock() {
            *p = plan.to_json();
        }
    }
    let mut tor_runtime = match tor::spawn(cfg, chain, Some(Arc::clone(&shared)), auto_cert) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[e-lite] WARNING: onion service unavailable: {e}");
            None
        }
    };
    publish_tor_ports(cfg, tor_runtime.as_ref(), &shared);
    if cfg.tor.managed() && tor_runtime.is_some() {
        let deadline =
            Instant::now() + Duration::from_secs(cfg.tor.start_timeout_seconds.clamp(5, 30));
        while !shared.stats.tor_bootstrapped.load(Ordering::Relaxed) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(500));
        }
    }
    refresh_providers(&shared, cfg, false);
    if !shared.provider_ready.load(Ordering::Relaxed)
        && cfg.rawtx.enabled
        && chain != Chain::Regtest
    {
        eprintln!(
            "[e-lite] WARNING: serve mode without a ready public provider; \
             local queries work, some raw transactions/history entries may be unavailable"
        );
        spawn_provider_reprobe(Arc::clone(&shared), cfg.clone());
    }
    let transports = start_transports(&shared, cfg)?;
    let tip = shared
        .store
        .committed_tip()?
        .map(|t| t.height as u64)
        .unwrap_or(0);
    let mut core_ibd = false;
    let mut core_reachable = false;
    let core_tip = match shared.core.blockchain_info() {
        Ok(info) => {
            check_pruned(&info, shared.store.committed_tip()?.as_ref(), None)?;
            shared
                .stats
                .core_height
                .store(info.blocks, Ordering::Relaxed);
            shared
                .stats
                .core_headers
                .store(info.headers, Ordering::Relaxed);
            shared.stats.core_verification_bps.store(
                (info.verificationprogress.clamp(0.0, 1.0) * 10_000.0) as u64,
                Ordering::Relaxed,
            );
            shared
                .stats
                .core_ibd
                .store(info.initialblockdownload, Ordering::Relaxed);
            if let Ok(net) = shared.core.network_info() {
                shared
                    .stats
                    .core_network_active
                    .store(net.networkactive, Ordering::Relaxed);
                shared
                    .stats
                    .core_connections
                    .store(net.connections, Ordering::Relaxed);
            }
            core_ibd = info.initialblockdownload;
            core_reachable = true;
            info.blocks
        }
        Err(e) => {
            eprintln!(
                "[e-lite] Core unavailable at startup ({e}); serving the cached index (height {tip})"
            );
            tip
        }
    };
    if !core_reachable {
        shared.set_state(state::CORE_UNAVAILABLE);
        if tip == 0 {
            eprintln!("[e-lite] Core is unavailable and the index is empty; waiting for Core data");
        } else {
            eprintln!("[e-lite] Core is unavailable; serving the cached index (height {tip})");
        }
    } else if tip >= core_tip && !core_ibd {
        shared.set_state(state::READY);
    } else {
        shared.set_state(state::BUILDING);
        eprintln!(
            "[e-lite] index is behind or Core is in IBD (indexed {tip}, core {core_tip}); queries will fail until caught up. Use `e-lite run` to index and serve."
        );
    }
    let sync = shared.sync_status();
    if sync.ready {
        let tls = shared
            .tls_listen
            .as_ref()
            .map(|addr| format!(" · SSL {addr}"))
            .unwrap_or_default();
        eprintln!(
            "{}",
            readiness_line(
                &shared,
                &sync,
                &tls,
                sync.fully_synced,
                &format_uptime(shared.stats.started.elapsed()),
            )
        );
    }
    println!(
        "serving Electrum {} on {} (network {}, indexed height {}, TLS {}, started {})",
        el_serve::PROTOCOL_MAX,
        cfg.server.listen,
        chain.name(),
        tip,
        if cfg.server.tls_enabled() {
            cfg.server.tls_listen.as_str()
        } else {
            "disabled"
        },
        format_local_time(started_at)
    );
    while !flag.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_secs(1));
    }
    shared.shutdown.store(true, Ordering::SeqCst);
    if let Some(rt) = &mut tor_runtime {
        rt.stop();
    }
    let _ = transports.mempool.join();
    let _ = transports.notifications.join();
    let _ = transports.memory.join();
    core_sup.stop();
    el_serve::log_rawtx_summary(&shared);
    println!(
        "[e-lite] stopped at {} (uptime {})",
        format_local_time(SystemTime::now()),
        format_uptime(started.elapsed())
    );
    Ok(())
}

fn cmd_run(cfg: &Config, chain: Chain) -> R<()> {
    let core_running = core_client(cfg, chain)?.tcp_reachable();
    preflight(cfg, chain, true, core_running)?;
    let started_at = SystemTime::now();
    let started = Instant::now();
    let mut prepared = cfg.clone();
    // TLS does not depend on Tor. Generate or reuse the certificate first so a
    // Tor failure only costs the onion service.
    let auto_cert = tls::prepare(&mut prepared, chain)?;
    if let Err(e) = tor::prepare(&mut prepared, chain) {
        eprintln!("[e-lite] WARNING: managed Tor unavailable: {e}");
    }
    let cfg = &prepared;
    let plan = tuning::MachinePlan::detect_with_core_reserve(
        cfg,
        chain,
        core_reserve_mib(cfg, core_running),
    );
    eprintln!("[e-lite] machine plan: {}", plan.describe());
    let rotational = plan.data_disk.kind == tuning::DiskKind::Hdd;
    let tunables = Arc::new(Tunables::new_with_plan(cfg, rotational, Some(&plan)));
    let store = open_store_with(cfg, chain, tunables.cache_mib(), tunables.background_jobs())?;
    tunables.apply_store(&store);
    warn_if_disk_low(cfg, chain);
    let flag = shutdown_flag();
    let from_zero = store.committed_tip().ok().flatten().is_none();
    let index_lock =
        if from_zero && cfg.core.manage != CoreManage::External && ibd_lock::IbdLock::enabled() {
            ibd_lock::IbdLock::acquire_waiting(&flag, chain)
        } else {
            None
        };
    if flag.load(Ordering::SeqCst) {
        println!(
            "[e-lite] stopped at {} (uptime {})",
            format_local_time(SystemTime::now()),
            format_uptime(started.elapsed())
        );
        return Ok(());
    }
    let mut core_sup = CoreSupervisor::new(cfg, chain);
    // `run` waits for Core. Building Shared only after RPC is ready means peer
    // discovery, mempool and the index loop never see a half-started Core.
    // A stored identity lets `run` serve the local index while Core is down
    // (state CORE_UNAVAILABLE); the index loop keeps retrying Core. A first run
    // with no identity still waits for Core, because the genesis is unknown.
    let Some(live) = ensure_identity_blocking(
        cfg,
        chain,
        &store,
        &core_client(cfg, chain)?,
        &mut core_sup,
        &flag,
        false,
        None,
    )?
    else {
        println!(
            "[e-lite] stopped at {} (uptime {})",
            format_local_time(SystemTime::now()),
            format_uptime(started.elapsed())
        );
        return Ok(());
    };
    let shared = build_shared(cfg, chain, Arc::clone(&store), live)?;
    log_startup_diagnostics(cfg, chain, &shared);
    if let Ok(mut p) = shared.stats.plan.lock() {
        *p = plan.to_json();
    }
    let mut tor_runtime = match tor::spawn(cfg, chain, Some(Arc::clone(&shared)), auto_cert) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[e-lite] WARNING: onion service unavailable: {e}");
            None
        }
    };
    publish_tor_ports(cfg, tor_runtime.as_ref(), &shared);
    if cfg.tor.managed() && tor_runtime.is_some() {
        // Let the managed Tor bootstrap before deciding to fall back to clearnet.
        let deadline =
            Instant::now() + Duration::from_secs(cfg.tor.start_timeout_seconds.clamp(5, 30));
        while !shared.stats.tor_bootstrapped.load(Ordering::Relaxed) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(500));
        }
    }
    let bulk = build_bulk(cfg, chain, shared.identity().genesis, tunables.readers())?;
    let can_control = core_sup.can_control_core();
    let idx = Indexer::new(
        Arc::clone(&store),
        shared.core.clone(),
        bulk,
        Some(Arc::clone(&shared.resolver)),
        Arc::clone(&tunables),
        cfg.clone(),
        Arc::clone(&shared.lock),
        Some(Arc::clone(&shared)),
        Arc::clone(&flag),
        Some(Arc::clone(&shared.readers)),
        can_control,
        chain,
    );
    // Start indexing before waiting for providers. A provider outage must not
    // stop local block progress; the readiness gate only controls when the
    // wallet listener opens.
    let index_cfg = cfg.clone();
    let index_shared = Arc::clone(&shared);
    let index_flag = Arc::clone(&flag);
    // The controller runs beside the index thread, not inside it: a long
    // catch-up call must never delay cache/memory protection.
    let controller = spawn_controller(
        Arc::clone(&tunables),
        Arc::clone(&store),
        Some(Arc::clone(&shared)),
        Arc::clone(&flag),
    );
    let index_thread = std::thread::spawn(move || {
        let mut core_sup = core_sup;
        let result = index_loop(
            &index_cfg,
            chain,
            Some(index_shared),
            idx,
            &mut core_sup,
            index_flag,
            false,
            index_lock,
        );
        core_sup.stop();
        result
    });

    let core_pruned = shared.core.blockchain_info().ok().map(|info| info.pruned);
    let has_esplora = !shared.public_servers.is_empty();
    let tx_sources = shared.tx_sources;
    let require_ready = provider_gate_required(cfg, chain, core_pruned, has_esplora);
    if cfg.rawtx.enabled
        && cfg.public_servers.require_ready
        && chain != Chain::Regtest
        && !require_ready
    {
        if has_esplora && core_pruned == Some(false) {
            eprintln!(
                "[e-lite] Core is not pruned: every raw transaction is served locally; \
                 Electrum opens without waiting for public providers"
            );
        } else if !has_esplora && tx_sources > 0 {
            eprintln!(
                "[e-lite] no Esplora endpoint to probe; the per-txid path ({tx_sources} Electrum \
                 servers) opens Electrum without holding the provider gate"
            );
        } else if !has_esplora {
            eprintln!(
                "[e-lite] no external source configured; Electrum opens with local data only \
                 (pruned raw transactions will fail)"
            );
        }
    }
    let gate_started = Instant::now();
    // Only wait inside the probe when the gate is actually holding the
    // listeners; otherwise one quick probe fills `/status`.
    refresh_providers(&shared, cfg, require_ready);
    let mut warned = false;
    while require_ready
        && !shared.provider_ready.load(Ordering::Relaxed)
        && !flag.load(Ordering::SeqCst)
        && !index_thread.is_finished()
    {
        if provider_gate_expired(cfg, gate_started.elapsed()) {
            eprintln!(
                "[e-lite] WARNING: no public provider answered within {}s; opening \
                 Electrum with local data (open_without_provider). History, balances \
                 and UTXOs are complete; raw transactions of pruned blocks fail until \
                 a provider answers",
                cfg.public_servers.ready_timeout_seconds
            );
            break;
        }
        if !warned {
            eprintln!(
                "[e-lite] no public provider is ready yet; indexing continues and \
                 Electrum will open when one answers (Core is pruned; set \
                 [public_servers] open_without_provider = true to open with local \
                 data after ready_timeout_seconds)"
            );
            warned = true;
        }
        std::thread::sleep(Duration::from_secs(5));
        refresh_providers(&shared, cfg, true);
    }
    if flag.load(Ordering::SeqCst) || index_thread.is_finished() {
        shared.shutdown.store(true, Ordering::SeqCst);
        if let Some(rt) = &mut tor_runtime {
            rt.stop();
        }
        let result = index_thread
            .join()
            .unwrap_or_else(|_| fail("index thread panicked"));
        flag.store(true, Ordering::SeqCst);
        let _ = controller.join();
        println!(
            "[e-lite] stopped at {} (uptime {})",
            format_local_time(SystemTime::now()),
            format_uptime(started.elapsed())
        );
        return result;
    }
    if !shared.provider_ready.load(Ordering::Relaxed)
        && cfg.rawtx.enabled
        && chain != Chain::Regtest
    {
        eprintln!(
            "[e-lite] WARNING: no public provider is ready; serving local data only \
             (raw transactions of pruned blocks are unavailable until one answers)"
        );
        if core_pruned != Some(false) {
            spawn_provider_reprobe(Arc::clone(&shared), cfg.clone());
        }
    }
    let transports = match start_transports(&shared, cfg) {
        Ok(t) => t,
        Err(e) => {
            flag.store(true, Ordering::SeqCst);
            shared.shutdown.store(true, Ordering::SeqCst);
            if let Some(rt) = &mut tor_runtime {
                rt.stop();
            }
            flag.store(true, Ordering::SeqCst);
            let _ = controller.join();
            let _ = index_thread.join();
            return Err(e);
        }
    };
    println!(
        "e-lite {}: started at {} — indexing and serving on {} (network {}, core {}, manage {:?})",
        VERSION,
        format_local_time(started_at),
        cfg.server.listen,
        chain.name(),
        cfg.rpc_url(chain),
        cfg.core.manage
    );
    while !flag.load(Ordering::SeqCst) && !index_thread.is_finished() {
        std::thread::sleep(Duration::from_secs(1));
    }
    shared.shutdown.store(true, Ordering::SeqCst);
    if let Some(rt) = &mut tor_runtime {
        rt.stop();
    }
    let _ = transports.mempool.join();
    let _ = transports.notifications.join();
    let _ = transports.memory.join();
    flag.store(true, Ordering::SeqCst);
    let _ = controller.join();
    let result = index_thread
        .join()
        .unwrap_or_else(|_| fail("index thread panicked"));
    el_serve::log_rawtx_summary(&shared);
    println!(
        "[e-lite] stopped at {} (uptime {})",
        format_local_time(SystemTime::now()),
        format_uptime(started.elapsed())
    );
    result
}

fn cmd_status(cfg: &Config, chain: Chain, as_json: bool) -> R<()> {
    if cfg.metrics.enabled {
        if let Ok(body) = http_get(&cfg.metrics.listen, "/status") {
            if as_json {
                let v: serde_json::Value = serde_json::from_str(&body).unwrap_or(json!({}));
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                let v: serde_json::Value = serde_json::from_str(&body).unwrap_or(json!({}));
                print_status_human(&v);
            }
            return Ok(());
        }
    }
    // Fallback: read the database directly (works when the server is stopped).
    let store = match open_store(cfg, chain) {
        Ok(s) => s,
        Err(e) => {
            let msg = e.to_string();
            if msg.to_ascii_lowercase().contains("lock") {
                return fail(
                    "e-lite is running but the metrics endpoint is disabled or unreachable; \
                     enable [metrics] or query the process directly",
                );
            }
            return Err(e);
        }
    };
    let tip = store.committed_tip()?;
    let v = json!({
        "version": VERSION,
        "network": chain.name(),
        "state": "STOPPED",
        "indexed_height": tip.as_ref().map(|t| t.height).unwrap_or(0),
        "txnum": tip.as_ref().map(|t| t.txnum).unwrap_or(0),
        "db_bytes": store.disk_usage_bytes(),
        "disk_free_bytes": el_serve::disk_free_bytes(&cfg.data_dir(chain)),
        "data_dir": cfg.data_dir(chain).display().to_string(),
        "onion": tor::read_hostname(&cfg.tor_dir(chain)).unwrap_or_default(),
        "onion_plain_port": tor::effective_onion_ports(cfg).0,
        "onion_tls_port": tor::effective_onion_ports(cfg).1.unwrap_or(0),
        "tor_bootstrapped": false,
        "tuning": {
            "cache_mib": tuning::memory_budget(cfg).cache_start_mib,
        },
        "memory": {
            "total_mib": tuning::memory_info().total_mib.unwrap_or(0),
            "available_mib": tuning::memory_info().available_mib,
            "cgroup_limit_mib": tuning::memory_info().cgroup_limit_mib.unwrap_or(0),
            "reclaimable_mib": tuning::memory_info().reclaimable_mib.unwrap_or(0),
            "cached_mib": tuning::memory_info().cached_mib,
            "cache_max_mib": tuning::memory_budget(cfg).cache_max_mib,
            "memtables_mib": tuning::memory_budget(cfg).memtables_mib,
            "reserve_mib": tuning::memory_budget(cfg).reserve_mib,
            "budget_mib": tuning::memory_budget(cfg).ceiling_mib,
        },
    });
    if as_json {
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        print_status_human(&v);
    }
    Ok(())
}

fn print_status_human(v: &serde_json::Value) {
    let g = |k: &str| v.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
    println!(
        "network   {}   state {}",
        v.get("network").and_then(|x| x.as_str()).unwrap_or("?"),
        v.get("state").and_then(|x| x.as_str()).unwrap_or("?")
    );
    println!(
        "sync      ready {}   at Core tip {}   fully synced {}   offline {}",
        v["ready"].as_bool().unwrap_or(false),
        v["at_core_tip"].as_bool().unwrap_or(false),
        v["fully_synced"].as_bool().unwrap_or(false),
        v["offline"].as_bool().unwrap_or(false)
    );
    println!(
        "index     height {}   txnum {}   lag {}",
        g("indexed_height"),
        g("txnum"),
        g("lag_blocks")
    );
    println!(
        "core      height {}/{} ({:.1}%)   pruneheight {}   networkactive {}",
        g("core_height"),
        g("core_headers"),
        if g("core_headers") > 0 {
            g("core_height") as f64 / g("core_headers") as f64 * 100.0
        } else {
            0.0
        },
        v["core"]["pruneheight"].as_u64().unwrap_or(0),
        v["core"]["network_active"].as_bool().unwrap_or(false)
    );
    println!(
        "memory    total {} MiB   available {} MiB   buff/cache {} MiB   cgroup limit {} MiB   \
         reclaimable {} MiB",
        v["memory"]["total_mib"].as_u64().unwrap_or(0),
        v["memory"]["available_mib"].as_u64().unwrap_or(0),
        v["memory"]["cached_mib"].as_u64().unwrap_or(0),
        v["memory"]["cgroup_limit_mib"].as_u64().unwrap_or(0),
        v["memory"]["reclaimable_mib"].as_u64().unwrap_or(0)
    );
    println!(
        "memory    cache {} / max {} MiB   memtables {} MiB   reserve {} MiB   budget {} MiB",
        v["tuning"]["cache_mib"].as_u64().unwrap_or(0),
        v["memory"]["cache_max_mib"].as_u64().unwrap_or(0),
        v["memory"]["memtables_mib"].as_u64().unwrap_or(0),
        v["memory"]["reserve_mib"].as_u64().unwrap_or(0),
        v["memory"]["budget_mib"].as_u64().unwrap_or(0)
    );
    let onion = v["onion"].as_str().unwrap_or("");
    if onion.is_empty() {
        println!("tor       onion not created yet");
    } else {
        println!(
            "tor       {}:{} (plain){}{}",
            onion,
            v["onion_plain_port"].as_u64().unwrap_or(50001),
            if v["onion_tls_port"].as_u64().unwrap_or(0) > 0 {
                format!(" · :{} (tls)", v["onion_tls_port"])
            } else {
                String::new()
            },
            if v["tor_bootstrapped"].as_bool().unwrap_or(false) {
                ""
            } else {
                " · bootstrapping"
            }
        );
    }
    println!("last block {} ms", g("last_block_ms"));
    println!(
        "storage   index {:.2} GiB   free {:.2} GiB   core free {:.2} GiB",
        g("db_bytes") as f64 / 1e9,
        g("disk_free_bytes") as f64 / 1e9,
        g("core_disk_free_bytes") as f64 / 1e9
    );
    println!(
        "clients   {} (max {})   requests {}   errors {}",
        g("clients"),
        g("max_clients"),
        g("requests_total"),
        g("errors_total")
    );
}

fn cmd_export(
    cfg: &Config,
    chain: Chain,
    output: Option<&str>,
    with_core: bool,
    hash_files: bool,
    compress_level: Option<u32>,
) -> R<()> {
    let store = open_store(cfg, chain)?;
    let tip = store.committed_tip()?;
    match output {
        None => {
            // Only the print-only mode needs the digest here; `export` computes
            // it once while writing the archive (AUD-140: it read the whole
            // index twice).
            let digest = store.canonical_digest()?;
            println!("network: {}", chain.name());
            match tip {
                Some(t) => println!("tip: {} {}", t.height, t.block_hash),
                None => println!("tip: empty"),
            }
            println!("logical_snapshot_hash: {digest}");
        }
        Some(path) => {
            let mut options = snapshot::ExportOptions {
                version: VERSION.to_string(),
                with_core,
                // An explicit --compress-level applies to both sections.
                index_level: compress_level.unwrap_or(0),
                core_level: compress_level.unwrap_or(3),
                hash_files,
                core: None,
                core_dir: None,
            };
            if with_core {
                // Starts and stops a short-lived Core to record tip, prune
                // height and version. Refuses while a real Core is running.
                let index_tip = tip
                    .as_ref()
                    .map(|t| t.height as u64)
                    .ok_or("cannot export an empty index")?;
                let facts = core_probe::export_facts(cfg, chain, index_tip)?;
                println!(
                    "[e-lite] Core probed: tip {} {} prune {} ({})",
                    facts.tip_height, facts.tip_hash, facts.prune_height, facts.version
                );
                options.core = Some(snapshot::CoreSnapshot {
                    version: facts.version,
                    tip_height: facts.tip_height,
                    tip_hash: facts.tip_hash,
                    pruned: facts.pruned,
                    prune_height: facts.prune_height,
                    bytes: 0,
                });
                options.core_dir = Some(cfg.core_datadir(chain));
            }
            let info = snapshot::export(&store, chain, std::path::Path::new(path), &options)?;
            println!(
                "snapshot: {} ({:.2} MiB)",
                info.path.display(),
                info.bytes as f64 / (1024.0 * 1024.0)
            );
            println!("network: {}", chain.name());
            println!("tip: {} {}", info.height, info.hash);
            println!("txnum: {}", info.txnum);
            println!("logical_snapshot_hash: {}", info.digest);
            println!(
                "keep this digest apart from the archive; restore with \
                 `import-snapshot --expect-digest {}`",
                info.digest
            );
            if let Some(core) = &info.core {
                println!(
                    "core: tip {} {}  prune {}{}  version {}",
                    core.tip_height,
                    core.tip_hash,
                    core.prune_height,
                    if core.pruned { "" } else { " (not pruned)" },
                    core.version
                );
            }
            if !info.core_skipped.is_empty() {
                // Only chain data is packed. Wallets, keys and configuration
                // must be backed up separately (AUD-126).
                println!(
                    "core: not copied (chain data only; back these up separately if needed): {}",
                    info.core_skipped.join(", ")
                );
            }
        }
    }
    Ok(())
}

fn cmd_import(
    cfg: &Config,
    chain: Chain,
    input: Option<&str>,
    verify: Option<&str>,
    expect_digest: Option<&str>,
) -> R<()> {
    let input = input.ok_or("import-snapshot requires --input <file.tar.gz>")?;
    let target = cfg.data_dir(chain);
    let info = snapshot::import(
        std::path::Path::new(input),
        &target,
        chain,
        &snapshot::ImportOptions {
            skip_hashes: false,
            expect_digest: expect_digest.map(str::to_string),
        },
    )?;
    println!("restored: {}", target.display());
    println!("network: {}", chain.name());
    println!("tip: {} {}", info.height, info.hash);
    println!("txnum: {}", info.txnum);
    println!("logical_snapshot_hash: {}", info.digest);
    let mut core_restored = None;
    if let Some(core) = &info.core {
        core_restored = Some((core.tip_height, core.tip_hash.clone()));
        println!(
            "core: tip {} {}  prune {}{}",
            core.tip_height,
            core.tip_hash,
            core.prune_height,
            if core.pruned { "" } else { " (not pruned)" }
        );
        // The archive restores Core to `<data-dir>/bitcoin`; an operator with
        // a custom [core] core_datadir must move it there.
        let default_core = target.join("bitcoin");
        let effective = cfg.core_datadir(chain);
        if effective != default_core {
            println!(
                "note: Core was restored to {}; the config points at {}; move the directory \
                 or adjust [core] core_datadir",
                default_core.display(),
                effective.display()
            );
        }
    }
    match verify {
        Some("basic") => {
            println!("verify: per-file sizes/hashes checked");
        }
        Some("full") => {
            let core_dir = target.join("bitcoin");
            if !core_dir.is_dir() {
                return fail(
                    "snapshot has no Core section to verify with --verify=full".to_string(),
                );
            }
            println!("[e-lite] verify=full: starting Core on the restored datadir...");
            let (tip_height, core_tip_hash) =
                core_restored.ok_or("snapshot has no Core section to verify with --verify=full")?;
            let report = core_probe::verify_restored(
                cfg,
                chain,
                &core_dir,
                info.height as u64,
                &info.hash,
                Some(&core_tip_hash),
            )?;
            if report.core_tip_height != tip_height {
                return fail(format!(
                    "restored Core tip {} does not match the manifest tip {}",
                    report.core_tip_height, tip_height
                ));
            }
            println!(
                "verify: core tip {} {}  prune {}  index-tip block {}  verifychain {}",
                report.core_tip_height,
                report.core_tip_hash,
                report.prune_height,
                if report.index_tip_block_ok {
                    "ok"
                } else {
                    "MISMATCH"
                },
                if report.verifychain_ok {
                    "ok"
                } else {
                    "FAILED"
                }
            );
            if !report.index_tip_block_ok || !report.verifychain_ok {
                return fail("restored pair did not pass full verification".to_string());
            }
            println!("verify=full: OK");
        }
        _ => {
            if !info.core.is_none() {
                println!("verify: per-file sizes and recorded hashes checked; use --verify=full to open the restored Core");
            }
        }
    }
    Ok(())
}

fn http_get(addr: &str, path: &str) -> R<String> {
    let mut stream = TcpStream::connect(addr)?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    let req = format!("GET {path} HTTP/1.0\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes())?;
    let mut resp = Vec::new();
    stream.read_to_end(&mut resp)?;
    let text = String::from_utf8_lossy(&resp);
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .ok_or("malformed HTTP response")?;
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(pruned: bool, pruneheight: u64) -> BlockchainInfo {
        BlockchainInfo {
            chain: "signet".to_string(),
            blocks: 1000,
            headers: 1000,
            pruned,
            pruneheight,
            prune_target_size: 0,
            size_on_disk: 0,
            initialblockdownload: false,
            signet_challenge: None,
            verificationprogress: 1.0,
        }
    }

    fn tip(height: u32) -> IndexedTip {
        IndexedTip {
            height,
            block_hash: Hash32::ZERO,
            schema_version: el_model::codec::SCHEMA_VERSION,
            txnum: 0,
        }
    }

    #[test]
    fn disk_resume_is_adaptive() {
        let mut cfg = Config::default();
        cfg.pruning.min_free_gib = 2;
        assert_eq!(disk_low_bytes(&cfg), 2 * GIB_BYTES);
        assert_eq!(disk_resume_bytes(&cfg), 3 * GIB_BYTES);
        cfg.pruning.min_free_gib = 10;
        assert_eq!(disk_resume_bytes(&cfg), 15 * GIB_BYTES);
    }

    #[test]
    fn auto_detects_chain_from_existing_database() {
        let dir = std::env::temp_dir().join(format!(
            "e-lite-auto-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let store = Store::open(&dir).unwrap();
            store
                .init_identity(&NetworkIdentity::new(
                    Chain::Regtest,
                    Chain::Regtest.known_genesis().unwrap(),
                ))
                .unwrap();
        }
        let mut cfg = Config::default();
        cfg.network.name = "auto".to_string();
        cfg.network.data_dir = Some(dir.display().to_string());
        assert_eq!(detect_chain_from_db(&cfg).unwrap(), Some(Chain::Regtest));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn full_node_allows_any_tip() {
        assert!(check_pruned(&info(false, 0), None, None).is_ok());
    }

    #[test]
    fn pruned_with_empty_index_is_rejected() {
        assert!(check_pruned(&info(true, 100), None, None).is_err());
    }

    #[test]
    fn pruned_gap_is_rejected() {
        // Index stops at 50, Core only has blocks from 100 onwards.
        assert!(check_pruned(&info(true, 100), Some(&tip(50)), None).is_err());
        assert!(check_pruned(&info(true, 100), Some(&tip(98)), None).is_err());
        // The same gap is allowed when a verified repair source is available.
        assert!(check_pruned(&info(true, 100), Some(&tip(98)), Some(2)).is_ok());
    }

    #[test]
    fn pruned_with_covered_tip_is_allowed() {
        assert!(check_pruned(&info(true, 100), Some(&tip(99)), None).is_ok());
        assert!(check_pruned(&info(true, 100), Some(&tip(150)), None).is_ok());
    }

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

    #[test]
    fn commit_sync_auto_relaxes_only_in_bulk() {
        assert!(!commit_sync_policy("auto", true, 100_000, 200_000));
        assert!(!commit_sync_policy("auto", false, 100_000, 200_000));
        assert!(commit_sync_policy("auto", false, 200_000, 200_000));
        assert!(commit_sync_policy("always", true, 100_000, 200_000));
        assert!(!commit_sync_policy("txids", false, 200_000, 200_000));
    }

    #[test]
    fn undo_window_is_measured_from_the_real_commit_tip() {
        // AUD-131: only the window of the tip actually committed is written.
        let dir = std::env::temp_dir().join(format!(
            "e-lite-undo-window-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = Store::open(&dir).unwrap();
        let window = 288u32;
        let records: Vec<el_model::UndoRecord> = (600..=1_200u32)
            .map(|height| el_model::UndoRecord {
                height,
                ..Default::default()
            })
            .collect();
        // A group that commits early at 1159 (time limit) still writes undo
        // for 1158 down to 1159 - 288, and nothing above the committed tip.
        let mut batch = WriteBatch::default();
        let written = write_window_undo(&store, &mut batch, &records, 1_159, window).unwrap();
        assert_eq!(written, window as usize + 1);
        // A tip below the window keeps everything up to the tip.
        let low: Vec<el_model::UndoRecord> = (0..=100u32)
            .map(|height| el_model::UndoRecord {
                height,
                ..Default::default()
            })
            .collect();
        let mut batch = WriteBatch::default();
        assert_eq!(
            write_window_undo(&store, &mut batch, &low, 100, window).unwrap(),
            101
        );
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bulk_drain_window_does_not_ratchet() {
        // 4 blocks/s, 30 min target -> pause at 7200, resume at 3600.
        assert_eq!(bulk_drain_window(4, 1_800, 100), (7_200, 3_600));
        // The same inputs produce the same thresholds after a resume; no
        // dependency on the lag observed at that moment.
        assert_eq!(bulk_drain_window(4, 1_800, 100), (7_200, 3_600));
        // Low rates still leave the configured minimum ahead.
        assert_eq!(bulk_drain_window(1, 1_800, 2_000), (4_000, 2_000));
        // A very high rate is bounded by the time target, not by a fixed lag.
        assert_eq!(bulk_drain_window(10, 600, 100), (6_000, 3_000));
    }

    #[test]
    fn bulk_lag_window_bounds_additional_growth() {
        // First call on a 317k backlog: pause only after 20k more.
        assert_eq!(
            bulk_lag_window(317_000, u64::MAX, 20_000, false),
            (317_000, 337_000, 327_000)
        );
        // If the index catches up below the baseline while downloading, the
        // window follows down.
        assert_eq!(
            bulk_lag_window(300_000, 317_000, 20_000, false),
            (300_000, 320_000, 310_000)
        );
        // While paused the baseline must NOT follow the lag down, otherwise
        // the resume threshold drops below the current lag and Core resumes
        // immediately.
        assert_eq!(
            bulk_lag_window(330_000, 317_000, 20_000, true),
            (317_000, 337_000, 327_000)
        );
        assert_eq!(
            bulk_lag_window(326_999, 317_000, 20_000, true),
            (317_000, 337_000, 327_000)
        );
    }

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

    #[test]
    fn uptime_is_human_readable() {
        assert_eq!(format_uptime(Duration::from_millis(500)), "0.5s");
        assert_eq!(format_uptime(Duration::from_secs(59)), "59.0s");
        assert_eq!(format_uptime(Duration::from_secs(3661)), "1h 01m 01s");
    }

    #[test]
    fn rate_limit_defaults_are_uniform_and_generous() {
        let mut cfg = Config::default();
        assert_eq!(effective_rate_limit(&cfg), (200, 5000), "loopback");

        cfg.server.listen = "0.0.0.0:50001".to_string();
        assert_eq!(effective_rate_limit(&cfg), (200, 5000), "exposed");

        cfg.server.rate_limit = Some(0);
        assert_eq!(effective_rate_limit(&cfg), (0, 5000), "explicit 0 disables");

        cfg.server.rate_limit = Some(10);
        cfg.server.rate_burst = Some(5);
        assert_eq!(effective_rate_limit(&cfg), (10, 5));

        let mut cfg = Config::default();
        cfg.server.tls_listen = "0.0.0.0:50002".to_string();
        assert_eq!(effective_rate_limit(&cfg), (200, 5000), "exposed TLS");
    }

    #[test]
    fn provider_readiness_is_required_only_for_networked_wallets() {
        let mut cfg = Config::default();
        assert!(!requires_ready_provider(&cfg, Chain::Regtest, true));
        assert!(requires_ready_provider(&cfg, Chain::Signet, true));
        // No Esplora endpoint to probe: the gate cannot succeed, so it is
        // skipped even with `require_ready` (AUD-167).
        assert!(!requires_ready_provider(&cfg, Chain::Signet, false));
        cfg.public_servers.require_ready = false;
        assert!(!requires_ready_provider(&cfg, Chain::Signet, true));
        cfg.public_servers.require_ready = true;
        cfg.rawtx.enabled = false;
        assert!(!requires_ready_provider(&cfg, Chain::Signet, true));
        cfg.rawtx.enabled = true;
        cfg.rawtx.use_esplora = false;
        assert!(!requires_ready_provider(&cfg, Chain::Signet, true));
    }

    #[test]
    fn p2p_peers_refresh_soon_when_empty_and_honour_pinned_peers() {
        assert_eq!(p2p_refresh_interval(0), Duration::from_secs(60));
        assert_eq!(p2p_refresh_interval(5), Duration::from_secs(600));
        let mut cfg = Config::default();
        assert!(p2p_discovery_enabled(&cfg));
        cfg.rawtx.p2p_peers = vec!["192.0.2.1:8333".into()];
        assert!(
            !p2p_discovery_enabled(&cfg),
            "pinned peers are never replaced"
        );
        cfg.rawtx.p2p_peers.clear();
        cfg.rawtx.use_p2p = false;
        assert!(!p2p_discovery_enabled(&cfg));
    }

    #[test]
    fn external_fetches_are_private_by_default() {
        // AUD-128: no clearnet fallback unless the operator opts in.
        // AUD-159: the prefetch is on, with decoys (see el-rawtx tests).
        let cfg = Config::default();
        assert!(!cfg.rawtx.tor_fallback_clearnet);
        assert!(!cfg.rawtx.allow_clearnet);
        assert!(cfg.rawtx.prefetch);
        assert!(cfg.rawtx.decoys >= 1);
        let parsed: Config = toml::from_str("[rawtx]\nenabled = true\n").unwrap();
        assert!(!parsed.rawtx.tor_fallback_clearnet);
        assert!(parsed.rawtx.prefetch);
        // AUD-155: a whole block over a fresh circuit gets an inactivity
        // timeout and a long absolute limit, not a short total one.
        assert!(cfg.rawtx.tor_request_timeout_seconds >= 60);
        assert!(cfg.rawtx.tor_stall_timeout_seconds < cfg.rawtx.tor_request_timeout_seconds);
        assert!(cfg.rawtx.p2p_tor_timeout_seconds > cfg.rawtx.p2p_timeout_seconds);
    }

    #[test]
    fn p2p_peers_from_core_are_outbound_full_witness_and_onion_first() {
        // AUD-154: inbound peers (ephemeral ports), I2P/CJDNS and peers that
        // cannot serve old witness blocks are never used.
        let peers: Vec<el_rpc::PeerInfo> = serde_json::from_str(
            r#"[
              {"addr":"198.51.100.1:51234","inbound":true,"services":1033,"network":"ipv4"},
              {"addr":"198.51.100.2:8333","inbound":false,"services":1033,"network":"ipv4","transport_protocol_type":"v1"},
              {"addr":"198.51.100.3:8333","inbound":false,"services":1032,"network":"ipv4"},
              {"addr":"198.51.100.4:8333","inbound":false,"services":1,"network":"ipv4"},
              {"addr":"abc.b32.i2p:0","inbound":false,"services":1033,"network":"i2p"},
              {"addr":"exampleonionaddress.onion:8333","inbound":false,"services":1033,"network":"onion","transport_protocol_type":"v2"}
            ]"#,
        )
        .unwrap();
        let addrman: Vec<el_rpc::NodeAddress> = serde_json::from_str(
            r#"[
              {"address":"2001:db8::1","port":8333,"services":1033,"network":"ipv6"},
              {"address":"fc00::1","port":8333,"services":1033,"network":"cjdns"},
              {"address":"198.51.100.2","port":8333,"services":1033,"network":"ipv4"}
            ]"#,
        )
        .unwrap();
        let (list, connected) = select_p2p_peers(&peers, &addrman);
        let addrs: Vec<&str> = list.iter().map(|p| p.addr.as_str()).collect();
        assert_eq!(
            addrs,
            vec![
                "exampleonionaddress.onion:8333",
                "198.51.100.2:8333",
                "[2001:db8::1]:8333"
            ]
        );
        assert_eq!(connected, 2);
        assert_eq!(list[0].v2, Some(true));
        assert_eq!(list[1].v2, Some(false));
        assert_eq!(list[2].v2, None);
    }

    #[test]
    fn provider_gate_skips_unpruned_core_and_expires() {
        let cfg = Config::default();
        // An unpruned Core serves every raw transaction: no gate.
        assert!(!provider_gate_required(
            &cfg,
            Chain::Signet,
            Some(false),
            true
        ));
        // Pruned, or Core did not answer: the gate holds the listeners.
        assert!(provider_gate_required(
            &cfg,
            Chain::Signet,
            Some(true),
            true
        ));
        assert!(provider_gate_required(&cfg, Chain::Signet, None, true));
        // Regtest never gates, whatever Core reports.
        assert!(!provider_gate_required(
            &cfg,
            Chain::Regtest,
            Some(true),
            true
        ));
        // Fail-closed by default (AUD-045): the gate never expires ...
        assert!(!provider_gate_expired(&cfg, Duration::from_secs(86_400)));
        // ... unless the operator opts in, and then only after the timeout.
        let mut open = Config::default();
        open.public_servers.open_without_provider = true;
        open.public_servers.ready_timeout_seconds = 60;
        assert!(!provider_gate_expired(&open, Duration::from_secs(59)));
        assert!(provider_gate_expired(&open, Duration::from_secs(60)));
    }

    #[test]
    fn directory_usage_counts_regular_files() {
        let dir = std::env::temp_dir().join(format!(
            "e-lite-dir-usage-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a"), vec![0u8; 100]).unwrap();
        std::fs::write(dir.join("sub/b"), vec![0u8; 50]).unwrap();
        assert_eq!(dir_usage_bytes(&dir), 150);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
