mod config;
mod core_supervisor;
mod logging;
mod snapshot;
mod tor;
mod tuning;

use config::{Config, CoreManage};
use core_supervisor::{CoreStartOutcome, CoreSupervisor};
use el_chain::ReaderTracker;
use el_index::{
    apply_prepared_with_prefetch, prefetch_prevouts, prepare_block_bytes, prune_undo, rewind_to,
    GroupOverlay,
};
use el_mempool::Mempool;
use el_model::{Chain, Hash32, IndexedTip, NetworkIdentity, ScriptHash};
use el_rawtx::{CoreBlockSource, RawtxConfig, Resolver};
use el_rpc::{BlockchainInfo, CoreClient, CoreConfig};
use el_serve::{state, Shared, Stats, VERSION};
use el_store::Store;
use rocksdb::WriteBatch;
use serde_json::json;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime};
use tuning::{Mode, Tunables};

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

fn readiness_line(shared: &Shared, sync: &el_serve::SyncStatus, tls: &str, fully: bool) -> String {
    let log = shared.log_file.display();
    if fully {
        format!(
            "[e-lite] FULLY SYNCED: height {}/{}; Electrum {}{}; log {}",
            sync.height, sync.headers, shared.listen, tls, log
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
            "[e-lite] READY: index caught up to Core at height {} ({} headers; {}); \
             Electrum {}{}; log {}",
            sync.height, sync.headers, note, shared.listen, tls, log
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
}

#[derive(Default)]
struct Args {
    command: Option<Command>,
    subcommand: Option<String>,
    config: Option<String>,
    network: Option<String>,
    data_dir: Option<String>,
    rpc_url: Option<String>,
    rpc_user: Option<String>,
    rpc_password: Option<String>,
    core_datadir: Option<String>,
    blocks_dir: Option<String>,
    cookie_file: Option<String>,
    listen: Option<String>,
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
    set: Vec<String>,
    profile: Option<String>,
    adaptive: Option<bool>,
    cache_mib: Option<u64>,
    commit_interval_ms: Option<u64>,
    commit_sync: Option<String>,
    prefetch_blocks: Option<usize>,
    write_buffer_mib: Option<u64>,
    memory_reserve_mib: Option<u64>,
    memory_fraction: Option<f64>,
    background_jobs: Option<usize>,
    milestone_blocks: Option<u64>,
    milestone_seconds: Option<u64>,
    max_core_lag_blocks: Option<u32>,
    max_bulk_lag_blocks: Option<u32>,
    pause_core_during_bulk: Option<bool>,
    core_resume_ahead_blocks: Option<u32>,
    prune_defer_free_gib: Option<u64>,
    min_gain_blocks: Option<u32>,
    prune_interval_seconds: Option<u64>,
    poll_seconds: Option<u64>,
    repair_max_gap_blocks: Option<u32>,
    critical_free_gib: Option<u64>,
    max_clients: Option<usize>,
    banner_file: Option<String>,
    core_args: Vec<String>,
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
    stop_on_exit: Option<bool>,
    restart: Option<bool>,
    log_file: Option<String>,
    log_max_mib: Option<u64>,
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
            "--memory-reserve-mib" => {
                args.memory_reserve_mib = Some(value("--memory-reserve-mib")?.parse()?)
            }
            "--memory-fraction" => {
                args.memory_fraction = Some(value("--memory-fraction")?.parse()?)
            }
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
            "--pause-core-during-bulk" => args.pause_core_during_bulk = Some(true),
            "--no-pause-core-during-bulk" => args.pause_core_during_bulk = Some(false),
            "--core-resume-ahead-blocks" => {
                args.core_resume_ahead_blocks = Some(value("--core-resume-ahead-blocks")?.parse()?)
            }
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
            "--adaptive" => args.adaptive = Some(true),
            "--no-adaptive" => args.adaptive = Some(false),
            "--no-p2p" => args.no_p2p = true,
            "--no-esplora" => args.no_esplora = true,
            "--log-file" => args.log_file = Some(value("--log-file")?),
            "--log-max-mib" => args.log_max_mib = Some(value("--log-max-mib")?.parse()?),
            "--no-log-file" => args.no_log_file = true,
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
                if matches!(args.command, Some(Command::Core) | Some(Command::Onion))
                    && args.subcommand.is_none()
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
                    "export-snapshot" => Command::ExportSnapshot,
                    "import-snapshot" => Command::ImportSnapshot,
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
    let mut cfg = Config::load_with_overrides(args.config.as_deref(), &args.set)?;
    if let Some(n) = &args.network {
        cfg.network.name = n.clone();
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
    if let Some(v) = args.memory_reserve_mib {
        cfg.index.memory_reserve_mib = v;
    }
    if let Some(v) = args.memory_fraction {
        cfg.index.memory_fraction = v;
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
    if let Some(v) = args.pause_core_during_bulk {
        cfg.pruning.pause_core_during_bulk = v;
    }
    if let Some(v) = args.core_resume_ahead_blocks {
        cfg.pruning.core_resume_ahead_blocks = v;
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
    if cfg.index.block_source != "auto"
        && cfg.index.block_source != "rpc"
        && cfg.index.block_source != "files"
    {
        return fail(format!(
            "unknown block source \"{}\": use auto, rpc or files",
            cfg.index.block_source
        ));
    }
    cfg.validate()?;
    let chain = resolve_chain(&cfg)?;
    if chain.requires_explicit_opt_in() && !cfg.safety.allow_mainnet {
        return fail(
            "mainnet requires explicit opt-in: pass --allow-mainnet or set [safety] allow_mainnet = true",
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
        core_datadir: Some(cfg.core_datadir()),
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
        el_chain::disk_free_bytes(&cfg.core_datadir()),
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
    let opts = el_store::StoreOptions {
        cache_bytes,
        write_buffer_total_bytes: (write_mib as usize) * 1024 * 1024,
        max_write_buffer_number: max_buffers,
        max_total_wal_bytes: ((write_mib / 2).clamp(64, 1024) as usize) * 1024 * 1024,
        background_jobs: jobs,
        ..Default::default()
    };
    Ok(Arc::new(Store::open_with(dir, opts)?))
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

    let (cfg, chain) = effective_config(&args)?;
    if matches!(cmd, Command::Index | Command::Serve | Command::Run) && !args.no_log_file {
        let path = cfg.app_log_file(chain);
        if let Err(e) = logging::init(&path, cfg.logging.max_mib) {
            eprintln!("[e-lite] WARNING: {e}; continuing without a log file");
        }
    }
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
        Command::ExportSnapshot => cmd_export(&cfg, chain, args.output.as_deref()),
        Command::ImportSnapshot => cmd_import(&cfg, chain, args.input.as_deref()),
        Command::Help | Command::Version => unreachable!(),
    }
}

fn print_help() {
    println!(
        r#"e-lite {VERSION} — Electrum 1.4 server for Bitcoin Core

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
    export-snapshot  print the canonical digest, or write a backup with --output
    import-snapshot  restore a backup with --input (target must be empty)
    version          print version

OPTIONS:
    --config <path>         TOML config file (default ~/.config/e-lite/config.toml)
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
    --listen <addr>         Electrum listen address (default 127.0.0.1:50001)
    --tls-listen <addr>     optional Electrum TLS listener (e.g. 127.0.0.1:50002)
    --tls-cert <path>       PEM certificate chain for --tls-listen
    --tls-key <path>        PEM private key for --tls-listen
    --metrics <addr>        status/metrics listen address (default 127.0.0.1:9184)
    --batch <n>             blocks per atomic commit (default 256)
    --reorg-window <n>      undo window in blocks (default 288)
    --allow-mainnet         required for mainnet
    --allow-clearnet        allow external rawtx over clearnet (privacy risk)
    --no-rawtx              disable external rawtx sources
    --no-follow             stop when caught up (only for `index`)
    --json                  JSON output for status
    --output <path>         snapshot file for export-snapshot
    --input <path>          snapshot file for import-snapshot
    --set <key=value>       override any TOML field, e.g. index.profile=ssd
                            (values: true, 42, ["a","b"], or a bare string)

TUNING (all networks):
    --profile <name>        auto | hdd | ssd | nvme | manual
    --adaptive              enable the controller (default)
    --no-adaptive           freeze the configured/derived values
    --cache-mib <n>         RocksDB block cache (0 = profile/RAM)
    --write-buffer-mib <n>  total RocksDB memtable budget (0 = RAM/8)
    --memory-reserve-mib <n> RAM left for OS/Core/page cache (0 = max(1024, RAM/8))
    --memory-fraction <f>   e-lite ceiling as a share of total RAM (0 = 0.75)
    --background-jobs <n>   RocksDB jobs (0 = profile, capped by CPUs)
    --commit-interval-ms <n> commit/fsync interval (0 = profile; 500..30000)
    --commit-sync <mode>    auto | always | txids (bulk durability tradeoff)
    --prefetch-blocks <n>   UTXO prefetch depth (reserved; 0 = default)
    --milestone-blocks <n>  progress report every N blocks (0 = off)
    --milestone-seconds <n> progress report every N seconds (0 = off)

PRUNING AND CORE:
    --max-core-lag-blocks <n>   auto-prune download limit (0 = off)
    --max-bulk-lag-blocks <n>   opt-in manual/bulk limit (0 = off)
    --pause-core-during-bulk    pause Core during a long catch-up (default)
    --no-pause-core-during-bulk disable the full bulk pause
    --core-resume-ahead-blocks <n> resume Core this early to pre-warm peers
    --prune-defer-free-gib <n>  skip normal prune above this free space (default 50)
    --min-gain-blocks <n>       minimum safe-height gain between prunes (default 10000)
    --prune-interval-seconds <n> minimum time between prunes
    --poll-seconds <n>          supervisor poll interval
    --repair-max-gap-blocks <n> largest external prune-gap repair
    --critical-free-gib <n>     pause indexing below this free space
    --allow-auto-prune          opt in to prune_mib >= 550 (automatic pruning)
    --core-arg <arg>            extra bitcoind argument (repeatable)
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
    --yes, -y               onion reset: do not ask for confirmation

LOGGING:
    --log-file <path>       application log (default <data-dir>/e-lite.log)
    --log-max-mib <n>       rotate to e-lite.log.1 above N MiB (0 = never)
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

fn cmd_doctor(cfg: &Config, chain: Chain) -> R<()> {
    let mut problems: Vec<String> = Vec::new();
    println!("e-lite {} doctor", VERSION);
    println!("network: {}", chain.name());
    println!("data dir: {}", cfg.data_dir(chain).display());

    let core = match core_client(cfg, chain) {
        Ok(c) => {
            println!("core rpc: {}", cfg.rpc_url(chain));
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

    let core_free = el_chain::disk_free_bytes(&cfg.core_datadir()).unwrap_or(0);
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
        "memory budget: total {} MiB, available {} MiB, cached {} MiB, reserve {} MiB, \
         ceiling {} MiB, cache max {} MiB, memtables {} MiB",
        budget.total_mib,
        budget.available_mib,
        budget.cached_mib,
        budget.reserve_mib,
        budget.ceiling_mib,
        budget.cache_max_mib,
        budget.memtables_mib
    );
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
        println!("electrum TLS: disabled (set [server] tls_listen to enable)");
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
    if cfg.tor.managed() {
        if let Err(e) = tor::loopback_target(&cfg.server.listen) {
            problems.push(e);
        }
        if cfg.tor.tls {
            if let Err(e) = tor::loopback_target(&tor_status.target_tls) {
                problems.push(e);
            }
        }
    } else if cfg.tor.tls && cfg.server.tls_listen.trim().is_empty() {
        println!(
            "tor TLS: will use {} with a self-signed certificate when Tor runs",
            tor::DEFAULT_TLS_LISTEN
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
    last_disk_check: Mutex<Instant>,
    progress_started: Instant,
    progress_start_height: u64,
    progress_start_txnum: u64,
    milestone: Mutex<MilestoneState>,
    milestone_sizes_running: Arc<AtomicBool>,
    bulk_lag_baseline: AtomicU64,
    bulk_started: AtomicBool,
    bulk_full_pause: AtomicBool,
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
        let core_datadir = cfg.core_datadir();
        let pause_marker = data_dir.join("core-paused-by-e-lite");
        let tip = store.committed_tip().ok().flatten();
        let start_height = tip.as_ref().map(|t| t.height as u64).unwrap_or(0);
        let start_txnum = tip.as_ref().map(|t| t.txnum).unwrap_or(0);
        let now = Instant::now();
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
            last_disk_check: Mutex::new(
                Instant::now()
                    .checked_sub(Duration::from_secs(60))
                    .unwrap_or_else(Instant::now),
            ),
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
        }
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
        let free = match (
            el_chain::disk_free_bytes(&self.data_dir),
            el_chain::disk_free_bytes(&self.core_datadir),
        ) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };
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

        // Between the critical floor and the low watermark: stop new blocks,
        // keep draining the backlog so pruning can free space. An external
        // Core cannot be paused, so indexing must stop instead.
        if self.is_paused() && self.can_control_core {
            self.set_paused(false);
            eprintln!(
                "[e-lite] disk above critical floor: {} MiB free; resumed indexing while downloads stay paused",
                free / (1024 * 1024)
            );
        }
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
        self.pause_downloads("disk low");
        false
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
        self.mark_paused(true);
        if self.can_control_core {
            if let Err(e) = self.core.set_network_active(false) {
                eprintln!("[e-lite] cannot pause Core downloads: {e}");
            }
        }
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
        self.mark_paused(false);
        if self.can_control_core {
            if let Err(e) = self.core.set_network_active(true) {
                eprintln!("[e-lite] cannot resume Core downloads: {e}");
            }
        }
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
                    sh.set_state(state::READY);
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

    fn commit_group(
        &self,
        batch: WriteBatch,
        tip: &IndexedTip,
        txids: &[Hash32],
        last_block_ms: u64,
        prune_undo_records: bool,
        sync: bool,
    ) -> R<()> {
        let mut batch = batch;
        if prune_undo_records {
            prune_undo(
                &self.store,
                tip.height,
                self.cfg.index.reorg_window,
                &mut batch,
            )?;
        }
        {
            let _g = self.lock.write().map_err(|_| "lock poisoned")?;
            let commit_started = Instant::now();
            let result = self.store.commit_with_sync(batch, tip, txids, sync);
            self.tunables.record_commit(commit_started.elapsed());
            if let Err(e) = result {
                // Whatever happened, keep the flat file aligned with the DB.
                if let Ok(Some(t)) = self.store.committed_tip() {
                    self.store.truncate_txids(t.txnum)?;
                }
                return Err(e.into());
            }
        }
        self.stats_tip(tip, last_block_ms);
        Ok(())
    }

    /// Pause Core's P2P when automatic pruning would outrun the index, or when
    /// the opt-in bulk guard is enabled for manual pruning. Core keeps at least
    /// `MIN_BLOCKS_TO_KEEP` (288) blocks, so the auto limit is capped below it.
    ///
    /// Manual pruning (`prune_mib = 1`) does not pause by default: Core cannot
    /// delete a block on its own and the prune gate keeps the cut below the
    /// committed index. `max_bulk_lag_blocks` is an explicit opt-in to bound
    /// the gap (and the disk contention) during a long catch-up.
    fn lag_guard(&self, info: &BlockchainInfo, committed: u64) {
        if !self.can_control_core {
            return;
        }
        let paused = self.lag_paused.load(Ordering::Relaxed);
        let lag = info.blocks.saturating_sub(committed);
        let bulk_limit = if self.cfg.core.prune_mib == 1 {
            self.cfg.pruning.max_bulk_lag_blocks as u64
        } else {
            0
        };
        let auto_enabled = self.cfg.core.prune_mib != 1 && self.cfg.pruning.max_core_lag_blocks > 0;
        let mut is_bulk = false;
        let mut bulk_resume = None;

        if bulk_limit == 0 && !auto_enabled {
            self.bulk_lag_baseline.store(u64::MAX, Ordering::Relaxed);
            if paused && self.core.set_network_active(true).is_ok() {
                self.lag_paused.store(false, Ordering::Relaxed);
                self.mark_paused(false);
                if let Some(sh) = &self.shared {
                    sh.stats.core_network_active.store(true, Ordering::Relaxed);
                    sh.set_state(state::READY);
                }
                eprintln!("[e-lite] resumed Core downloads");
            }
            return;
        }

        let limit = if bulk_limit > 0 {
            if !info.initialblockdownload {
                self.bulk_lag_baseline.store(u64::MAX, Ordering::Relaxed);
                self.bulk_started.store(false, Ordering::Relaxed);
                self.bulk_full_pause.store(false, Ordering::Relaxed);
                if paused && self.core.set_network_active(true).is_ok() {
                    self.lag_paused.store(false, Ordering::Relaxed);
                    self.mark_paused(false);
                    if let Some(sh) = &self.shared {
                        sh.stats.core_network_active.store(true, Ordering::Relaxed);
                        sh.set_state(state::READY);
                    }
                    eprintln!("[e-lite] resumed Core downloads after IBD");
                }
                return;
            }
            is_bulk = true;
            let ahead = self.cfg.pruning.core_resume_ahead_blocks as u64;
            if self.cfg.pruning.pause_core_during_bulk {
                let first = !self.bulk_started.swap(true, Ordering::SeqCst);
                if first && lag > ahead {
                    self.bulk_full_pause.store(true, Ordering::SeqCst);
                    self.tunables.reset_resume_shrink();
                }
                if self.bulk_full_pause.load(Ordering::SeqCst) {
                    // Full bulk pause: keep Core stopped until the index is
                    // close enough; then the common resume path pre-warms P2P.
                    bulk_resume = Some(ahead);
                    ahead.max(1)
                } else {
                    let baseline = self.bulk_lag_baseline.load(Ordering::Relaxed);
                    let (new_baseline, pause_at, resume_at) =
                        bulk_lag_window(lag, baseline, bulk_limit, paused);
                    self.bulk_lag_baseline
                        .store(new_baseline, Ordering::Relaxed);
                    bulk_resume = Some(resume_at);
                    pause_at
                }
            } else {
                self.bulk_started.store(true, Ordering::Relaxed);
                // Bound *additional* separation, not the absolute lag. With a
                // 300k-block backlog, an absolute 20k threshold would freeze Core
                // for the whole catch-up. The baseline follows the lowest lag seen
                // while Core is downloading, so Core works in phases.
                let baseline = self.bulk_lag_baseline.load(Ordering::Relaxed);
                let (new_baseline, pause_at, resume_at) =
                    bulk_lag_window(lag, baseline, bulk_limit, paused);
                self.bulk_lag_baseline
                    .store(new_baseline, Ordering::Relaxed);
                bulk_resume = Some(resume_at);
                pause_at
            }
        } else {
            if !info.pruned {
                if paused && self.core.set_network_active(true).is_ok() {
                    self.lag_paused.store(false, Ordering::Relaxed);
                    self.mark_paused(false);
                    if let Some(sh) = &self.shared {
                        sh.stats.core_network_active.store(true, Ordering::Relaxed);
                        sh.set_state(state::READY);
                    }
                    eprintln!("[e-lite] Core is no longer pruned: resumed downloads");
                }
                return;
            }
            if info.pruneheight == 0 {
                if paused && self.core.set_network_active(true).is_ok() {
                    self.lag_paused.store(false, Ordering::Relaxed);
                    self.mark_paused(false);
                    if let Some(sh) = &self.shared {
                        sh.stats.core_network_active.store(true, Ordering::Relaxed);
                        sh.set_state(state::READY);
                    }
                    eprintln!("[e-lite] Core has no pruned blocks: resumed downloads");
                }
                return;
            }
            el_chain::lag_limit(
                info.blocks.min(u32::MAX as u64) as u32,
                info.pruneheight.min(u32::MAX as u64) as u32,
                self.cfg.pruning.max_core_lag_blocks,
            ) as u64
        };
        let baseline = self.bulk_lag_baseline.load(Ordering::Relaxed);
        let resume = if let Some(r) = bulk_resume {
            r
        } else {
            (limit / 2).max(1)
        };
        let reader_pressure = self
            .readers
            .as_ref()
            .and_then(|r| r.earliest())
            .map(|h| (h as u64) < info.pruneheight.saturating_add(32))
            .unwrap_or(false);

        if !paused && (lag > limit || reader_pressure) {
            match self.core.set_network_active(false) {
                Ok(_) => {
                    self.lag_paused.store(true, Ordering::Relaxed);
                    self.mark_paused(true);
                    if let Some(sh) = &self.shared {
                        sh.stats.core_network_active.store(false, Ordering::Relaxed);
                        sh.set_state(state::PAUSE_DOWNLOAD);
                    }
                    if is_bulk && self.bulk_full_pause.load(Ordering::SeqCst) {
                        eprintln!(
                            "[e-lite] bulk: paused Core downloads until {limit} blocks remain \
                             (lag {lag})"
                        );
                    } else if is_bulk {
                        eprintln!(
                            "[e-lite] lag {lag} blocks (baseline {baseline}, max growth \
                             {bulk_limit}, reader {reader_pressure}): paused Core downloads \
                             until the index catches up"
                        );
                    } else {
                        eprintln!(
                            "[e-lite] lag {lag} blocks (limit {limit}, reader {reader_pressure}): \
                             paused Core downloads until the index catches up"
                        );
                    }
                }
                Err(e) => eprintln!("[e-lite] cannot pause Core downloads: {e}"),
            }
        } else if paused && lag <= resume && !reader_pressure {
            if self.downloads_paused() {
                // Keep Core paused: disk backpressure still owns the network.
                self.lag_paused.store(false, Ordering::Relaxed);
                return;
            }
            // Core is about to grow again: give RAM back before resuming.
            self.tunables.shrink_for_core_resume(&self.store);
            match self.core.set_network_active(true) {
                Ok(_) => {
                    self.lag_paused.store(false, Ordering::Relaxed);
                    self.mark_paused(false);
                    let prewarm = is_bulk && self.bulk_full_pause.swap(false, Ordering::SeqCst);
                    if is_bulk {
                        self.bulk_lag_baseline.store(lag, Ordering::Relaxed);
                    }
                    if let Some(sh) = &self.shared {
                        sh.stats.core_network_active.store(true, Ordering::Relaxed);
                        sh.set_state(state::READY);
                    }
                    if prewarm {
                        eprintln!(
                            "[e-lite] pre-warming Core networking at lag {lag} blocks \
                             (peers reconnect while the index finishes)"
                        );
                    } else {
                        eprintln!("[e-lite] lag down to {lag} blocks: resumed Core downloads");
                    }
                }
                Err(e) => eprintln!("[e-lite] cannot resume Core downloads: {e}"),
            }
        } else if paused {
            // A disk-supervisor resume may have re-enabled P2P behind our back.
            let _ = self.core.set_network_active(false);
            self.mark_paused(true);
        }
    }

    /// Index at most `max_blocks` before returning, so the supervisor can act
    /// between steps (disk backpressure, pruning).
    fn catch_up_limited(&self, max_blocks: u64) -> R<u64> {
        let tip_at_start = self.store.committed_tip()?;
        // Check the disks before any Core RPC or block write. If Core is slow
        // or unreachable, this still stops the indexer before the disk fills.
        if self.disk_guard() {
            return Ok(tip_at_start.map(|t| t.height as u64).unwrap_or(0));
        }
        let info = self.core.blockchain_info()?;
        let core_tip = info.blocks;
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

        let start = tip.map(|t| t.height as u64 + 1).unwrap_or(0);
        if start > core_tip || max_blocks == 0 {
            self.refresh_core_height();
            return Ok(committed_height);
        }
        let target = core_tip.min(start.saturating_add(max_blocks.saturating_sub(1)));
        let wanted = target - start + 1;
        let hashes = self.core.block_hashes(start, wanted)?;
        if hashes.is_empty() {
            self.refresh_core_height();
            return Ok(committed_height);
        }
        let hashes_len = hashes.len();

        let workers = self.tunables.readers();
        let hashes = Arc::new(hashes);
        let next_index = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (tx, rx) = std::sync::mpsc::sync_channel::<(usize, FetchResult)>(workers * 2);
        let stop = Arc::new(AtomicBool::new(false));
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
            let tx = tx.clone();
            readers.push(std::thread::spawn(move || loop {
                if stop_thread.load(Ordering::Relaxed) {
                    break;
                }
                let i = next_index.fetch_add(1, Ordering::Relaxed);
                if i >= hashes.len() {
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
                                let prevouts = prefetch_prevouts(&store, &p);
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
        let mut last_ms = 0u64;
        let mut group_undo = false;
        let mut result: R<()> = Ok(());
        let mut paused_by_disk = false;
        let mut group_started = Instant::now();
        let mut last_lag_check = Instant::now();

        let mut pending: PendingMap = PendingMap::new();
        let mut next_apply = 0usize;
        while next_apply < hashes_len {
            let wait_started = Instant::now();
            while !pending.contains_key(&next_apply) {
                match rx.recv() {
                    Ok((i, item)) => {
                        pending.insert(i, item);
                    }
                    Err(_) => break,
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
            let (height, prepared, prevouts) = match item {
                Ok(v) => v,
                Err(e) => {
                    result = Err(e.into());
                    break;
                }
            };
            let t0 = Instant::now();
            // Undo is only useful for the reorg window. Writing it for every
            // historical block and deleting it a few commits later was pure
            // write amplification; keep a safety margin for an interrupted
            // catch-up call.
            let keep_undo = height
                .saturating_add(self.cfg.index.reorg_window as u64)
                .saturating_add(64)
                >= core_tip;
            match apply_prepared_with_prefetch(
                &self.store,
                height as u32,
                &prepared,
                tip,
                &mut overlay,
                &mut batch,
                keep_undo,
                Some(&prevouts),
            ) {
                Ok(out) => {
                    group_txids.extend_from_slice(&out.txids);
                    if self.shared.is_some() {
                        group_scripts.extend(out.touched_scripts.iter().copied());
                    }
                    tip = Some(out.tip);
                    count += 1;
                    if keep_undo {
                        group_undo = true;
                    }
                    last_ms = t0.elapsed().as_millis() as u64;
                    self.tunables.record_apply(t0.elapsed());
                    if (self.cfg.core.prune_mib >= 550
                        || (self.cfg.core.prune_mib == 1
                            && self.cfg.pruning.max_bulk_lag_blocks > 0))
                        && info.pruned
                        && last_lag_check.elapsed() >= Duration::from_millis(100)
                    {
                        if let Ok(live) = self.core.blockchain_info() {
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
                            let committed = tip.map(|t| t.height as u64).unwrap_or(0);
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
                    result = Err(e.into());
                    break;
                }
            }
            let commit_due = count >= self.tunables.batch_blocks().max(1)
                || group_txids.len() >= self.tunables.max_group_txs()
                || group_started.elapsed() >= self.tunables.commit_interval();
            if commit_due {
                let current = tip.expect("tip set");
                let sync =
                    self.commit_sync(info.initialblockdownload, current.height as u64, core_tip);
                if let Err(e) = self.commit_group(
                    std::mem::take(&mut batch),
                    &current,
                    &group_txids,
                    last_ms,
                    group_undo,
                    sync,
                ) {
                    result = Err(e);
                    break;
                }
                group_undo = false;
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
                    self.tunables.publish(&sh.stats);
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
                group_started = Instant::now();
            }
        }

        if result.is_ok() && count > 0 && !paused_by_disk && !self.disk_guard() {
            let current = tip.expect("tip set");
            let sync = self.commit_sync(info.initialblockdownload, current.height as u64, core_tip);
            self.commit_group(
                std::mem::take(&mut batch),
                &current,
                &group_txids,
                last_ms,
                group_undo,
                sync,
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
                self.tunables.publish(&sh.stats);
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
        drop(rx);
        for reader in readers {
            let _ = reader.join();
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
            } else {
                format!("{:.1}%", (value as f64 / total as f64 * 100.0).min(100.0))
            }
        };
        let read_ms = self.tunables.read_ms.load(Ordering::Relaxed);
        let decode_ms = self.tunables.decode_ms();
        let prefetch_ms = self.tunables.prefetch_ms();
        let wait_ms = self.tunables.wait_ms();
        let apply_ms = self.tunables.apply_ms.load(Ordering::Relaxed);
        let commit_ms = self.tunables.commit_ewma.load(Ordering::Relaxed);
        let commit_per_block = commit_ms / self.tunables.batch_blocks().max(1) as u64;
        eprintln!(
            "[e-lite] height {} ({})  txnum {}  core {}/{} ({})  prune {}  lag {}  \
             {:.1} blocks/s  {:.0} tx/s  elapsed {}  read {}ms decode {}ms prefetch {}ms wait {}ms \
             apply {}ms commit {}ms commit/blk {}ms{}",
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
            read_ms,
            decode_ms,
            prefetch_ms,
            wait_ms,
            apply_ms,
            commit_ms,
            commit_per_block,
            if ibd {
                format!("  (core in IBD, verify {:.1}%)", verification * 100.0)
            } else {
                String::new()
            }
        );
    }

    /// One line every N blocks or N seconds, whichever happens first. Disk
    /// sizes are walked in a background thread so a huge Core datadir does not
    /// stall the indexer.
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

/// Bulk lag guard window. `u64::MAX` means "no baseline yet". The baseline
/// follows the lowest lag seen **while Core is downloading**. While paused it
/// must stay fixed; otherwise every block indexed would lower the resume
/// threshold and Core would resume immediately.
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
        prune_interval: Duration::from_secs(cfg.pruning.prune_interval_seconds),
        prune_defer_free_bytes: {
            let gib = cfg
                .pruning
                .prune_defer_free_gib
                .max(cfg.pruning.min_free_gib);
            (gib > 0).then_some(gib.saturating_mul(GIB_BYTES))
        },
    }
}

fn supervisor_tick(
    sup: &mut el_chain::Supervisor,
    core: &CoreClient,
    store: &Store,
    readers: Option<&el_chain::ReaderTracker>,
    data_dir: &std::path::Path,
    core_dir: &std::path::Path,
    shared: Option<&Arc<Shared>>,
) -> R<Vec<el_chain::Action>> {
    // Watch both filesystems: e-lite's index and Core's datadir may be on
    // different disks, and either one running out must stop both writers.
    // Read this before Core RPC so a slow/unreachable Core cannot hide a full
    // disk.
    let free = match (
        el_chain::disk_free_bytes(data_dir),
        el_chain::disk_free_bytes(core_dir),
    ) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };
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
    };
    Ok(sup.tick(&input))
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
                            sh.set_state(state::READY);
                        }
                        eprintln!("[e-lite] disk recovered: resumed indexing");
                    }
                } else {
                    if let Some(sh) = shared {
                        sh.stats.disk_low.store(false, Ordering::Relaxed);
                        sh.set_state(state::READY);
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
                sup.on_prune_requested(height);
                if let Some(sh) = shared {
                    sh.stats.prune_requests.fetch_add(1, Ordering::Relaxed);
                    sh.set_state(state::PRUNING);
                }
                match core.prune_blockchain(height as u64) {
                    Ok(got) if got > 0 => {
                        let got = (got as u64).min(u32::MAX as u64) as u32;
                        sup.on_pruned(got);
                        eprintln!("[e-lite] pruned Core blocks below {got} (requested {height})");
                    }
                    Ok(got) => {
                        // Core had no whole block file to remove. Cooldown until
                        // the safe height advances.
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
                    sh.set_state(state::READY);
                }
            }
        }
    }
}

fn cmd_index(cfg: &Config, chain: Chain) -> R<()> {
    let started_at = SystemTime::now();
    let started = Instant::now();
    let rotational = tuning::is_rotational(&cfg.data_dir(chain));
    let tunables = Arc::new(Tunables::new(cfg, rotational));
    let store = open_store_with(cfg, chain, tunables.cache_mib(), tunables.background_jobs())?;
    tunables.apply_store(&store);
    warn_if_disk_low(cfg, chain);
    let core = core_client(cfg, chain)?;
    let flag = shutdown_flag();
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
    let result = index_loop(
        cfg,
        chain,
        None,
        idx,
        &mut core_sup,
        flag,
        !cfg.index.follow,
    );
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
) {
    let Some(path) = config_path() else {
        eprintln!("[e-lite] reload requested but no config file was resolved");
        return;
    };
    let path_str = path.to_string_lossy().to_string();
    match Config::load(Some(&path_str)) {
        Ok(new) => {
            if let Err(e) = new.validate() {
                eprintln!("[e-lite] reload rejected ({e}); keeping the previous config");
                return;
            }
            let changed = idx.tunables.reload(&new);
            idx.tunables.apply_store(&idx.store);
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
                idx.tunables.publish(&sh.stats);
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
            {
                eprintln!(
                    "[e-lite] reload: cold fields changed (prune_mib, block_source, \
                     reorg_window, data_dir, server listeners/TLS, write buffers, \
                     milestones, lag limits); restart required"
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

/// Shared index loop for `index` and `run`. `stop_at_tip` implements
/// `--no-follow`; `run` always follows.
fn index_loop(
    cfg: &Config,
    chain: Chain,
    shared: Option<Arc<Shared>>,
    idx: Indexer,
    core_sup: &mut CoreSupervisor,
    flag: Arc<AtomicBool>,
    stop_at_tip: bool,
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
    let (mem_total, mem_reserve, mem_ceiling, mem_memtables, mem_runtime) =
        idx.tunables.memory_numbers();
    let mem_now = tuning::memory_info();
    eprintln!(
        "[e-lite] memory: total {} MiB, available {} MiB, buff/cache {} MiB, \
         reserve {} MiB, ceiling {} MiB -> cache {} MiB, memtables {} MiB, runtime {} MiB",
        mem_total,
        mem_now.available_mib,
        mem_now.cached_mib,
        mem_reserve,
        mem_ceiling,
        idx.tunables.cache_mib(),
        mem_memtables,
        mem_runtime
    );
    if let Some(sh) = &shared {
        idx.tunables.publish(&sh.stats);
    }

    loop {
        if reload_flag().swap(false, Ordering::SeqCst) {
            reload_hot(cfg, &idx, &mut supervisor, shared.as_ref(), can_control);
        }
        if flag.load(Ordering::SeqCst) {
            if shared.is_none() {
                println!("shutdown requested; index stopped");
            }
            return Ok(());
        }
        match idx.catch_up_limited(CATCH_UP_BLOCKS) {
            Ok(height) => {
                if flag.load(Ordering::SeqCst) {
                    return Ok(());
                }
                backoff = 1;
                loading_logged = false;
                let core_height = shared
                    .as_ref()
                    .map(|s| s.stats.core_height.load(Ordering::Relaxed))
                    .unwrap_or(0);
                let core_ibd = shared
                    .as_ref()
                    .map(|s| s.stats.core_ibd.load(Ordering::Relaxed))
                    .unwrap_or(false);
                let lag = core_height.saturating_sub(height);
                let mode = if core_ibd || lag > 1_000 {
                    Mode::Bulk
                } else {
                    Mode::Live
                };
                if idx
                    .tunables
                    .maybe_adapt(mode, lag, &idx.store, shared.as_ref().map(|s| &s.stats))
                    .is_some()
                {
                    idx.tunables.apply_store(&idx.store);
                }
                if let Some(sh) = &shared {
                    let caught_up = !core_ibd && height + 1 >= core_height;
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
                    if print_fully {
                        eprintln!("{}", readiness_line(sh, &sync, &tls, true));
                    } else if print_ready {
                        eprintln!("{}", readiness_line(sh, &sync, &tls, false));
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
                match supervisor_tick(
                    &mut supervisor,
                    &idx.core,
                    &idx.store,
                    idx.readers.as_deref(),
                    &cfg.data_dir(chain),
                    &cfg.core_datadir(),
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
            match supervisor_tick(
                &mut supervisor,
                &idx.core,
                &idx.store,
                idx.readers.as_deref(),
                &cfg.data_dir(chain),
                &cfg.core_datadir(),
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

fn default_esplora_urls(chain: Chain) -> Vec<String> {
    match chain {
        Chain::Mainnet => vec![
            "http://mempoolhqx4isw62xs7abwphsq7ldayuidyx2v2oethdhhj6mlo2r6ad.onion/api".to_string(),
            "https://blockstream.info/api".to_string(),
            "https://mempool.space/api".to_string(),
        ],
        Chain::Testnet4 => vec!["https://mempool.space/testnet4/api".to_string()],
        Chain::Signet => vec!["https://mempool.space/signet/api".to_string()],
        Chain::Regtest => Vec::new(),
    }
}

fn build_resolver(
    cfg: &Config,
    chain: Chain,
    store: &Arc<Store>,
    core: &CoreClient,
) -> R<Arc<Resolver>> {
    let mut peers = cfg.rawtx.p2p_peers.clone();
    if cfg.rawtx.enabled && cfg.rawtx.use_p2p && peers.is_empty() {
        match core.node_addresses(64) {
            Ok(addrs) => {
                peers = addrs
                    .into_iter()
                    .filter(|a| a.services & 1 != 0)
                    .map(|a| {
                        let host = if a.address.contains(':') && !a.address.starts_with('[') {
                            format!("[{}]", a.address)
                        } else {
                            a.address.clone()
                        };
                        format!("{host}:{}", a.port)
                    })
                    .collect();
                println!("p2p peers discovered from Core: {}", peers.len());
            }
            Err(_) => {
                // Core may be offline (serve mode) or still starting. P2P
                // discovery is optional; do not print a startup error. The
                // operator can configure [rawtx] p2p_peers explicitly.
            }
        }
    }
    let tor = cfg.rawtx.tor_socks.clone().filter(|s| !s.trim().is_empty());
    let esplora_urls = if cfg.rawtx.esplora_urls.is_empty() {
        default_esplora_urls(chain)
    } else {
        cfg.rawtx.esplora_urls.clone()
    };
    let rc = RawtxConfig {
        enabled: cfg.rawtx.enabled,
        cache_mib: cfg.rawtx.cache_mib,
        decoys: cfg.rawtx.decoys,
        tor_socks: tor,
        allow_clearnet: cfg.rawtx.allow_clearnet,
        use_p2p: cfg.rawtx.use_p2p,
        use_esplora: cfg.rawtx.use_esplora,
        p2p_v2: cfg.rawtx.p2p_v2,
        esplora_urls,
        p2p_peers: peers,
        p2p_timeout: Duration::from_secs(cfg.core.timeout_seconds.max(1)),
        chain,
    };
    let resolver = Resolver::new(Arc::clone(store), &rc)?;
    Ok(Arc::new(
        resolver.with_local(Arc::new(CoreBlockSource::new(core.clone()))),
    ))
}

fn build_shared(
    cfg: &Config,
    chain: Chain,
    store: Arc<Store>,
    identity: NetworkIdentity,
) -> R<Arc<Shared>> {
    let core = core_client(cfg, chain)?;
    let resolver = build_resolver(cfg, chain, &store, &core)?;
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
    let shared = Arc::new(Shared {
        store,
        core,
        resolver,
        mempool: Arc::new(RwLock::new(Mempool::new())),
        relayfee: Mutex::new(0.0),
        fee_estimates: Mutex::new(std::collections::HashMap::new()),
        error_log: Mutex::new(std::collections::HashMap::new()),
        clients: Arc::new(el_serve::ClientRegistry::new()),
        readers: el_chain::ReaderTracker::new(),
        silent_payments_enabled: cfg.silent_payments.enabled,
        identity: RwLock::new(identity),
        headers: el_serve::HeaderCache::new(),
        db_bytes: Mutex::new(el_serve::DbBytesCache::default()),
        dirty_scripts: el_serve::DirtyScripts::new(),
        data_dir: cfg.data_dir(chain),
        core_datadir: cfg.core_datadir(),
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
    publish_memory(&shared);
    Ok(shared)
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
    let started_at = SystemTime::now();
    let started = Instant::now();
    let mut prepared = cfg.clone();
    let auto_cert = match tor::prepare(&mut prepared, chain) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[e-lite] WARNING: Tor TLS disabled: {e}");
            prepared.server.tls_listen.clear();
            prepared.server.tls_cert.clear();
            prepared.server.tls_key.clear();
            false
        }
    };
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
    let transports = start_transports(&shared, cfg)?;
    let mut tor_runtime = match tor::spawn(cfg, chain, Some(Arc::clone(&shared)), auto_cert) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[e-lite] WARNING: onion service unavailable: {e}");
            None
        }
    };
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
            readiness_line(&shared, &sync, &tls, sync.fully_synced)
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
    println!(
        "[e-lite] stopped at {} (uptime {})",
        format_local_time(SystemTime::now()),
        format_uptime(started.elapsed())
    );
    Ok(())
}

fn cmd_run(cfg: &Config, chain: Chain) -> R<()> {
    let started_at = SystemTime::now();
    let started = Instant::now();
    let mut prepared = cfg.clone();
    let auto_cert = match tor::prepare(&mut prepared, chain) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[e-lite] WARNING: Tor TLS disabled: {e}");
            prepared.server.tls_listen.clear();
            prepared.server.tls_cert.clear();
            prepared.server.tls_key.clear();
            false
        }
    };
    let cfg = &prepared;
    let rotational = tuning::is_rotational(&cfg.data_dir(chain));
    let tunables = Arc::new(Tunables::new(cfg, rotational));
    let store = open_store_with(cfg, chain, tunables.cache_mib(), tunables.background_jobs())?;
    tunables.apply_store(&store);
    warn_if_disk_low(cfg, chain);
    let flag = shutdown_flag();
    let mut core_sup = CoreSupervisor::new(cfg, chain);
    // `run` waits for Core. Building Shared only after RPC is ready means peer
    // discovery, mempool and the index loop never see a half-started Core.
    let Some(live) = ensure_identity_blocking(
        cfg,
        chain,
        &store,
        &core_client(cfg, chain)?,
        &mut core_sup,
        &flag,
        true,
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
    let transports = start_transports(&shared, cfg)?;
    let mut tor_runtime = match tor::spawn(cfg, chain, Some(Arc::clone(&shared)), auto_cert) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[e-lite] WARNING: onion service unavailable: {e}");
            None
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
    let result = index_loop(
        cfg,
        chain,
        Some(Arc::clone(&shared)),
        idx,
        &mut core_sup,
        Arc::clone(&flag),
        false,
    );
    shared.shutdown.store(true, Ordering::SeqCst);
    if let Some(rt) = &mut tor_runtime {
        rt.stop();
    }
    let _ = transports.mempool.join();
    let _ = transports.notifications.join();
    let _ = transports.memory.join();
    core_sup.stop();
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

fn cmd_export(cfg: &Config, chain: Chain, output: Option<&str>) -> R<()> {
    let store = open_store(cfg, chain)?;
    let tip = store.committed_tip()?;
    let digest = store.canonical_digest()?;
    match output {
        None => {
            println!("network: {}", chain.name());
            match tip {
                Some(t) => println!("tip: {} {}", t.height, t.block_hash),
                None => println!("tip: empty"),
            }
            println!("logical_snapshot_hash: {digest}");
        }
        Some(path) => {
            let info = snapshot::export(&store, chain, std::path::Path::new(path), VERSION)?;
            println!(
                "snapshot: {} ({:.2} MiB)",
                info.path.display(),
                info.bytes as f64 / (1024.0 * 1024.0)
            );
            println!("network: {}", chain.name());
            println!("tip: {} {}", info.height, info.hash);
            println!("txnum: {}", info.txnum);
            println!("logical_snapshot_hash: {}", info.digest);
        }
    }
    Ok(())
}

fn cmd_import(cfg: &Config, chain: Chain, input: Option<&str>) -> R<()> {
    let input = input.ok_or("import-snapshot requires --input <file.tar.gz>")?;
    let target = cfg.data_dir(chain);
    let info = snapshot::import(std::path::Path::new(input), &target, chain)?;
    println!("restored: {}", target.display());
    println!("network: {}", chain.name());
    println!("tip: {} {}", info.height, info.hash);
    println!("txnum: {}", info.txnum);
    println!("logical_snapshot_hash: {}", info.digest);
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
    fn uptime_is_human_readable() {
        assert_eq!(format_uptime(Duration::from_millis(500)), "0.5s");
        assert_eq!(format_uptime(Duration::from_secs(59)), "59.0s");
        assert_eq!(format_uptime(Duration::from_secs(3661)), "1h 01m 01s");
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
