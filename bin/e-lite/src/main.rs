mod config;
mod core_plan;
mod core_probe;
mod core_supervisor;
mod ibd_lock;
mod logging;
mod privacy;
mod public_servers;
mod reader_pool;
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
use reader_pool::ReaderPool;
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

mod cli;
mod diagnostics;
mod pipeline;
mod serve;
mod setup;

pub(crate) use cli::*;
pub(crate) use diagnostics::*;
pub(crate) use pipeline::*;
pub(crate) use serve::*;
pub(crate) use setup::*;

/// Panic policy (AUD-173). The binary unwinds, and this hook decides what a
/// panic costs:
///
/// * in a thread that serves one wallet connection
///   (`el_model::sync::CONTAINED_THREADS`), that thread ends and the node
///   keeps running; its `max_clients` slot and pipeline place are released by
///   drop guards and the shared locks do not poison;
/// * anywhere else (index, commit, prune gate, tuning, mempool, notifier,
///   startup) the process aborts, as it always did: a half-applied state must
///   never be served, and RocksDB/WAL recovery on restart is the tested path.
///
/// The line carries the thread and the source location, never the panic
/// message of a wallet thread: it could quote request data.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("unnamed");
        let at = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "unknown location".to_string());
        if el_model::sync::panic_is_contained(thread.name()) {
            eprintln!(
                "[e-lite] ERROR: internal error in {name} at {at}; that request \
                 fails (its connection may close), the node keeps running. Please report it."
            );
            return;
        }
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_default();
        eprintln!(
            "[e-lite] FATAL: internal error in {name} at {at}: {}; stopping. \
             The index recovers from its log on the next start.",
            el_rawtx::redact_hashes(&message)
        );
        logging::shutdown();
        std::process::abort();
    }));
}

fn main() {
    install_panic_hook();
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

#[cfg(test)]
mod tests;
