mod config;
mod snapshot;

use bitcoin::consensus::deserialize;
use bitcoin::Block;
use config::Config;
use el_index::{apply_block, prune_undo, rewind_to, GroupOverlay};
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

type E = Box<dyn std::error::Error + Send + Sync>;
type R<T> = std::result::Result<T, E>;

fn fail<T>(msg: impl Into<String>) -> R<T> {
    Err(msg.into().into())
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

/// Errors that retrying cannot fix: wrong network, prune gap, corrupt schema...
fn is_permanent_error(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("pruned")
        || m.contains("genesis mismatch")
        || m.contains("needs_recovery")
        || m.contains("not initialized")
        || m.contains("schema")
        || m.contains("identity")
        || m.contains("txids.dat")
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
}

#[derive(Default)]
struct Args {
    command: Option<Command>,
    config: Option<String>,
    network: Option<String>,
    data_dir: Option<String>,
    rpc_url: Option<String>,
    core_datadir: Option<String>,
    cookie_file: Option<String>,
    listen: Option<String>,
    metrics: Option<String>,
    allow_mainnet: bool,
    json: bool,
    no_follow: bool,
    batch: Option<u32>,
    reorg_window: Option<u32>,
    mode: Option<String>,
    output: Option<String>,
    input: Option<String>,
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
            "--cookie-file" => args.cookie_file = Some(value("--cookie-file")?),
            "--listen" => args.listen = Some(value("--listen")?),
            "--metrics" => args.metrics = Some(value("--metrics")?),
            "--batch" => args.batch = Some(value("--batch")?.parse()?),
            "--reorg-window" => args.reorg_window = Some(value("--reorg-window")?.parse()?),
            "--mode" => args.mode = Some(value("--mode")?),
            "--output" => args.output = Some(value("--output")?),
            "--input" => args.input = Some(value("--input")?),
            "--allow-mainnet" => args.allow_mainnet = true,
            "--json" => args.json = true,
            "--no-follow" => args.no_follow = true,
            other if other.starts_with('-') => return fail(format!("unknown flag: {other}")),
            other => {
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
    let mut cfg = Config::load(args.config.as_deref())?;
    if let Some(n) = &args.network {
        cfg.network.name = n.clone();
    }
    if let Some(d) = &args.data_dir {
        cfg.network.data_dir = Some(d.clone());
    }
    if let Some(r) = &args.rpc_url {
        cfg.core.rpc_url = Some(r.clone());
    }
    if let Some(d) = &args.core_datadir {
        cfg.core.core_datadir = Some(d.clone());
    }
    if let Some(c) = &args.cookie_file {
        cfg.core.cookie_file = Some(c.clone());
    }
    if let Some(l) = &args.listen {
        cfg.server.listen = l.clone();
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
    if let Some(m) = &args.mode {
        cfg.index.mode = m.clone();
    }
    if args.no_follow {
        cfg.index.follow = false;
    }
    if args.allow_mainnet {
        cfg.safety.allow_mainnet = true;
    }
    if cfg.index.mode != "full-core" && cfg.index.mode != "pruned-ibd" {
        return fail(format!(
            "unknown index mode \"{}\": use full-core or pruned-ibd",
            cfg.index.mode
        ));
    }
    let chain = cfg.chain()?;
    if chain.requires_explicit_opt_in() && !cfg.safety.allow_mainnet {
        return fail(
            "mainnet requires explicit opt-in: pass --allow-mainnet or set [safety] allow_mainnet = true",
        );
    }
    Ok((cfg, chain))
}

fn core_client(cfg: &Config, chain: Chain) -> R<CoreClient> {
    let cc = CoreConfig {
        rpc_url: cfg.rpc_url(chain),
        cookie_file: cfg.cookie_file(),
        core_datadir: Some(cfg.core_datadir()),
        chain,
        timeout: Duration::from_secs(cfg.core.timeout_seconds.max(1)),
    };
    Ok(CoreClient::connect(&cc)?)
}

fn core_identity(core: &CoreClient, chain: Chain) -> R<NetworkIdentity> {
    let info = core.blockchain_info()?;
    let core_chain = match info.chain.as_str() {
        "main" | "mainnet" => Chain::Mainnet,
        "testnet4" => Chain::Testnet4,
        "signet" => Chain::Signet,
        "regtest" => Chain::Regtest,
        other => return fail(format!("Core reports unsupported chain: {other}")),
    };
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

fn open_store(cfg: &Config, chain: Chain) -> R<Arc<Store>> {
    let dir = cfg.data_dir(chain);
    Ok(Arc::new(Store::open(dir)?))
}

fn require_identity(
    store: &Store,
    cfg: &Config,
    chain: Chain,
    core: &CoreClient,
) -> R<NetworkIdentity> {
    let stored = store
        .network_identity()?
        .ok_or("database is not initialized; run `e-lite init` first")?;
    if stored.chain != chain {
        return fail(format!(
            "database belongs to {}, config says {}",
            stored.chain.name(),
            chain.name()
        ));
    }
    match core_identity(core, chain) {
        Ok(live) => {
            if stored.genesis != live.genesis {
                return fail(format!(
                    "genesis mismatch: db {} vs core {}",
                    stored.genesis, live.genesis
                ));
            }
            if stored.signet_challenge != live.signet_challenge {
                return fail("signet challenge mismatch");
            }
        }
        Err(e) => {
            // Core may be restarting; the database identity is pinned and can
            // be trusted while we serve the cached index. The indexer will
            // re-verify the chain as soon as Core returns.
            eprintln!(
                "[e-lite] Core unavailable at startup ({e}); using the pinned database identity"
            );
            stored.check_known_genesis()?;
        }
    }
    let _ = cfg;
    Ok(stored)
}

fn main() {
    if let Err(e) = run() {
        eprintln!("e-lite: error: {e}");
        std::process::exit(1);
    }
}

fn run() -> R<()> {
    let args = parse_args()?;
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
    match cmd {
        Command::Init => cmd_init(&cfg, chain),
        Command::Doctor => cmd_doctor(&cfg, chain),
        Command::Index => cmd_index(&cfg, chain),
        Command::Serve => cmd_serve(&cfg, chain),
        Command::Run => cmd_run(&cfg, chain),
        Command::Status => cmd_status(&cfg, chain, args.json),
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
    run              index and serve in one process (recommended)
    status           print health (via local HTTP, or the database if stopped)
    export-snapshot  print the canonical digest, or write a backup with --output
    import-snapshot  restore a backup with --input (target must be empty)
    version          print version

OPTIONS:
    --config <path>         TOML config file
    --network <name>        regtest | signet | testnet4 | mainnet
    --data-dir <path>       index directory
    --core-rpc <url>        http://127.0.0.1:<port>
    --core-datadir <path>   Core datadir (cookie is read from there)
    --cookie-file <path>    explicit Core cookie
    --listen <addr>         Electrum listen address (default 127.0.0.1:50001)
    --metrics <addr>        status/metrics listen address (default 127.0.0.1:9184)
    --batch <n>             blocks per atomic commit (default 32)
    --reorg-window <n>      undo window in blocks (default 288)
    --allow-mainnet         required for mainnet
    --mode <mode>           full-core | pruned-ibd
    --no-follow             stop when caught up
    --json                  JSON output for status
    --output <path>         snapshot file for export-snapshot
    --input <path>          snapshot file for import-snapshot
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

    match core.network_info() {
        Ok(n) => println!("core version: {} ({})", n.version, n.subversion.trim()),
        Err(e) => problems.push(format!("getnetworkinfo: {e}")),
    }
    let mut core_pruned = false;
    let mut pruneheight = 0u64;
    match core.blockchain_info() {
        Ok(info) => {
            println!(
                "chain: {}  blocks: {}  headers: {}  pruned: {}  ibd: {}",
                info.chain, info.blocks, info.headers, info.pruned, info.initialblockdownload
            );
            core_pruned = info.pruned;
            pruneheight = info.pruneheight;
        }
        Err(e) => problems.push(format!("getblockchaininfo: {e}")),
    }

    match core_identity(&core, chain) {
        Ok(id) => println!("genesis: {}", id.genesis),
        Err(e) => problems.push(format!("identity: {e}")),
    }

    let mut db_tip: Option<IndexedTip> = None;
    let db_dir = cfg.data_dir(chain);
    if !db_dir.exists() {
        println!("db: not initialized (run `e-lite init`)");
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
                    Ok(None) => println!("db: not initialized (run `e-lite init`)"),
                    Err(e) => problems.push(format!("db identity: {e}")),
                }
                println!("db size: {} bytes", store.disk_usage_bytes());
            }
            Err(e) => problems.push(format!("open db: {e}")),
        }
    }

    let tor = cfg.rawtx.tor_socks.clone().filter(|s| !s.trim().is_empty());
    let external_enabled =
        (cfg.rawtx.use_p2p || cfg.rawtx.use_esplora) && (tor.is_some() || cfg.rawtx.allow_clearnet);

    if core_pruned {
        match &db_tip {
            None => {
                if pruneheight == 0 && cfg.index.mode == "pruned-ibd" {
                    println!("core pruned (manual, pruneheight 0): pruned-ibd can start indexing");
                } else {
                    problems.push(
                        "Core is pruned and the index is empty: build it with a full node or restore a snapshot"
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
    if (cfg.rawtx.use_p2p || cfg.rawtx.use_esplora) && tor.is_none() && !cfg.rawtx.allow_clearnet {
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
    if cfg.rawtx.cache_gib > 0 {
        let dir = cfg.data_dir(chain).join("rawtx-cache");
        match std::fs::create_dir_all(&dir) {
            Ok(_) => println!(
                "rawtx cache: {} ({} GiB max, {} decoys)",
                dir.display(),
                cfg.rawtx.cache_gib,
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

    let free = el_serve::disk_free_bytes(&cfg.data_dir(chain));
    println!("disk free: {} GiB", free / (1024 * 1024 * 1024));

    if bind_ok(&cfg.server.listen) {
        println!("electrum listen {}: available", cfg.server.listen);
    } else {
        problems.push(format!(
            "electrum listen {} is already in use",
            cfg.server.listen
        ));
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

fn print_problems(p: &[String]) {
    for x in p {
        eprintln!("PROBLEM: {x}");
    }
}

fn bind_ok(addr: &str) -> bool {
    std::net::TcpListener::bind(addr).is_ok()
}

// ------------------------------------------------------------------- indexing

struct Indexer {
    store: Arc<Store>,
    core: CoreClient,
    cfg: Config,
    lock: Arc<RwLock<()>>,
    shared: Option<Arc<Shared>>,
    shutdown: Arc<AtomicBool>,
}

impl Indexer {
    fn new(
        store: Arc<Store>,
        core: CoreClient,
        cfg: Config,
        lock: Arc<RwLock<()>>,
        shared: Option<Arc<Shared>>,
        shutdown: Arc<AtomicBool>,
    ) -> Self {
        Indexer {
            store,
            core,
            cfg,
            lock,
            shared,
            shutdown,
        }
    }

    fn stats_core_height(&self, h: u64) {
        if let Some(sh) = &self.shared {
            sh.stats.core_height.store(h, Ordering::Relaxed);
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
        Ok(())
    }

    fn commit_group(
        &self,
        batch: WriteBatch,
        tip: &IndexedTip,
        txids: &[Hash32],
        last_block_ms: u64,
    ) -> R<()> {
        let mut batch = batch;
        prune_undo(
            &self.store,
            tip.height,
            self.cfg.index.reorg_window,
            &mut batch,
        )?;
        {
            let _g = self.lock.write().map_err(|_| "lock poisoned")?;
            if let Err(e) = self.store.commit(batch, tip, txids) {
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

    /// Index at most `max_blocks` before returning, so the supervisor can act
    /// between steps (disk backpressure, pruning).
    fn catch_up_limited(&self, max_blocks: u64) -> R<u64> {
        let started = Instant::now();
        let info = self.core.blockchain_info()?;
        let core_tip = info.blocks;
        self.stats_core_height(core_tip);
        let tip_at_start = self.store.committed_tip()?;
        check_pruned(&info, tip_at_start.as_ref())?;
        self.handle_reorg(core_tip)?;

        let mut tip = tip_at_start;
        let start = tip.map(|t| t.height as u64 + 1).unwrap_or(0);
        if start > core_tip || max_blocks == 0 {
            return Ok(tip.map(|t| t.height as u64).unwrap_or(0));
        }
        let target = core_tip.min(start.saturating_add(max_blocks.saturating_sub(1)));

        let (tx, rx) = std::sync::mpsc::sync_channel::<
            std::result::Result<(u64, String, Vec<u8>), String>,
        >(16);
        let core = self.core.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let fetcher = std::thread::spawn(move || {
            for h in start..=target {
                if stop_thread.load(Ordering::Relaxed) {
                    break;
                }
                let item = (|| -> std::result::Result<(u64, String, Vec<u8>), String> {
                    let hash = core.block_hash(h).map_err(|e| e.to_string())?;
                    let raw = core.block_hex(&hash).map_err(|e| e.to_string())?;
                    let bytes = hex::decode(&raw).map_err(|e| e.to_string())?;
                    Ok((h, hash, bytes))
                })();
                if tx.send(item).is_err() {
                    break;
                }
            }
        });

        let mut overlay = GroupOverlay::new();
        let mut batch = WriteBatch::default();
        let mut group_txids: Vec<Hash32> = Vec::new();
        let mut count = 0u32;
        let mut last_ms = 0u64;
        let mut result: R<()> = Ok(());

        for item in rx {
            if self.shutdown.load(Ordering::SeqCst) {
                break;
            }
            let (height, expected_hash, bytes) = match item {
                Ok(v) => v,
                Err(e) => {
                    result = Err(e.into());
                    break;
                }
            };
            let block: Block = match deserialize(&bytes) {
                Ok(b) => b,
                Err(e) => {
                    result = Err(format!("cannot decode block {height}: {e}").into());
                    break;
                }
            };
            let actual_hash = block.block_hash().to_string();
            if actual_hash != expected_hash {
                result = Err(format!(
                    "block hash mismatch at {height}: core {expected_hash}, computed {actual_hash}"
                )
                .into());
                break;
            }
            let t0 = Instant::now();
            match apply_block(
                &self.store,
                height as u32,
                &block,
                tip,
                &mut overlay,
                &mut batch,
            ) {
                Ok(out) => {
                    group_txids.extend_from_slice(&out.txids);
                    tip = Some(out.tip);
                    count += 1;
                    last_ms = t0.elapsed().as_millis() as u64;
                }
                Err(e) => {
                    result = Err(e.into());
                    break;
                }
            }
            if count >= self.cfg.index.batch_blocks {
                let current = tip.expect("tip set");
                if let Err(e) =
                    self.commit_group(std::mem::take(&mut batch), &current, &group_txids, last_ms)
                {
                    result = Err(e);
                    break;
                }
                print_progress(&current, started, info.initialblockdownload);
                group_txids.clear();
                overlay.clear();
                count = 0;
            }
        }

        if result.is_ok() && count > 0 {
            let current = tip.expect("tip set");
            self.commit_group(std::mem::take(&mut batch), &current, &group_txids, last_ms)?;
            print_progress(&current, started, info.initialblockdownload);
            if self.shutdown.load(Ordering::SeqCst) {
                eprintln!(
                    "[e-lite] shutdown requested; flushed at height {}",
                    current.height
                );
            }
        }

        stop.store(true, Ordering::Relaxed);
        let _ = fetcher.join();
        result?;
        Ok(tip.map(|t| t.height as u64).unwrap_or(0))
    }
}

fn print_progress(tip: &IndexedTip, started: Instant, ibd: bool) {
    let secs = started.elapsed().as_secs_f64().max(0.001);
    let rate = tip.txnum as f64 / secs;
    eprintln!(
        "[e-lite] height {}  txnum {}  {:.0} tx/s{}",
        tip.height,
        tip.txnum,
        rate,
        if ibd { "  (core in IBD)" } else { "" }
    );
}

fn supervisor_config(cfg: &Config) -> el_chain::SupervisorConfig {
    const GIB: u64 = 1024 * 1024 * 1024;
    el_chain::SupervisorConfig {
        // Disk backpressure only applies to Mode B: a full-core node is not
        // pruning, and pausing its downloads would be surprising.
        disk_enabled: cfg.pruning.enabled && cfg.index.mode == "pruned-ibd",
        disk_low_bytes: cfg.pruning.disk_low_free_gib.saturating_mul(GIB),
        disk_resume_bytes: cfg.pruning.disk_resume_free_gib.saturating_mul(GIB),
        prune_enabled: cfg.pruning.enabled && cfg.index.mode == "pruned-ibd",
        undo_margin: cfg.index.reorg_window.max(cfg.pruning.undo_margin_blocks),
        core_recent_margin: cfg.pruning.core_recent_blocks_margin,
        min_gain: cfg.pruning.min_gain_blocks.max(1),
    }
}

fn supervisor_tick(
    sup: &mut el_chain::Supervisor,
    core: &CoreClient,
    store: &Store,
    readers: Option<&el_chain::ReaderTracker>,
    data_dir: &std::path::Path,
) -> R<Vec<el_chain::Action>> {
    let info = core.blockchain_info()?;
    let committed = store.committed_tip()?.map(|t| t.height as u64).unwrap_or(0);
    let free = el_chain::disk_free_bytes(data_dir);
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
) {
    for action in actions {
        match action {
            el_chain::Action::PauseDownloads => match core.set_network_active(false) {
                Ok(_) => {
                    if let Some(sh) = shared {
                        sh.set_state(state::PAUSE_DOWNLOAD);
                    }
                    eprintln!("[e-lite] disk low: paused Core downloads");
                }
                Err(e) => eprintln!("[e-lite] cannot pause Core downloads: {e}"),
            },
            el_chain::Action::ResumeDownloads => match core.set_network_active(true) {
                Ok(_) => {
                    if let Some(sh) = shared {
                        sh.set_state(state::READY);
                    }
                    eprintln!("[e-lite] disk recovered: resumed Core downloads");
                }
                Err(e) => eprintln!("[e-lite] cannot resume Core downloads: {e}"),
            },
            el_chain::Action::Prune(height) => {
                sup.on_prune_requested();
                if let Some(sh) = shared {
                    sh.set_state(state::PRUNING);
                }
                match core.prune_blockchain(height as u64) {
                    Ok(got) if got >= 0 => {
                        sup.on_pruned(height);
                        eprintln!(
                            "[e-lite] pruned Core blocks below {height} (Core reports {got})"
                        );
                    }
                    Ok(got) => {
                        // Core had no whole block file to remove. Cooldown until
                        // the safe height advances by min_gain.
                        sup.on_pruned(height);
                        eprintln!(
                            "[e-lite] prune at {height} not possible yet (Core reports {got}); waiting for more blocks"
                        );
                    }
                    Err(e) => {
                        sup.on_prune_failed();
                        eprintln!("[e-lite] prune request failed: {e}");
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
    let store = open_store(cfg, chain)?;
    let core = core_client(cfg, chain)?;
    require_identity(&store, cfg, chain, &core)?;
    let lock = Arc::new(RwLock::new(()));
    let flag = shutdown_flag();
    let idx = Indexer::new(store, core, cfg.clone(), lock, None, Arc::clone(&flag));
    let mut supervisor = el_chain::Supervisor::new(supervisor_config(cfg));
    let poll = Duration::from_secs(cfg.pruning.poll_seconds.max(1));
    let mut last_tick = Instant::now()
        .checked_sub(poll)
        .unwrap_or_else(Instant::now);
    loop {
        if flag.load(Ordering::SeqCst) {
            println!("shutdown requested; index stopped");
            return Ok(());
        }
        let height = idx.catch_up_limited(512)?;
        if last_tick.elapsed() >= poll {
            match supervisor_tick(
                &mut supervisor,
                &idx.core,
                &idx.store,
                None,
                &cfg.data_dir(chain),
            ) {
                Ok(actions) => apply_supervisor_actions(&mut supervisor, &idx.core, actions, None),
                Err(e) => eprintln!("[e-lite] supervisor: {e}"),
            }
            last_tick = Instant::now();
        }
        if !cfg.index.follow {
            let core_tip = idx.core.blockchain_info()?.blocks;
            if height >= core_tip {
                println!("caught up at height {height}");
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// A pruned Core can serve only if the local index already covers the gap.
fn check_pruned(info: &BlockchainInfo, tip: Option<&IndexedTip>) -> R<()> {
    if !info.pruned {
        return Ok(());
    }
    let tip = match tip {
        Some(t) => t,
        None => {
            // pruned-ibd can bootstrap while Core has not pruned anything yet.
            if info.pruneheight == 0 {
                return Ok(());
            }
            return fail(
                "Core is pruned and the index is empty: build the index with a full node or restore a snapshot",
            );
        }
    };
    if (tip.height as u64) + 1 < info.pruneheight {
        return fail(format!(
            "Core is pruned from height {} but the index stops at {}; blocks {}..{} are unavailable. Use a full node to build the index or restore a snapshot.",
            info.pruneheight,
            tip.height,
            tip.height + 1,
            info.pruneheight.saturating_sub(1)
        ));
    }
    Ok(())
}

fn build_resolver(
    cfg: &Config,
    chain: Chain,
    store: &Arc<Store>,
    core: &CoreClient,
) -> R<Arc<Resolver>> {
    let mut peers = cfg.rawtx.p2p_peers.clone();
    if cfg.rawtx.use_p2p && peers.is_empty() {
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
            Err(e) => eprintln!("[e-lite] p2p discovery failed: {e}"),
        }
    }
    let tor = cfg.rawtx.tor_socks.clone().filter(|s| !s.trim().is_empty());
    let rc = RawtxConfig {
        cache_gib: cfg.rawtx.cache_gib,
        decoys: cfg.rawtx.decoys,
        tor_socks: tor,
        allow_clearnet: cfg.rawtx.allow_clearnet,
        use_p2p: cfg.rawtx.use_p2p,
        use_esplora: cfg.rawtx.use_esplora,
        p2p_v2: cfg.rawtx.p2p_v2,
        esplora_urls: cfg.rawtx.esplora_urls.clone(),
        p2p_peers: peers,
        p2p_timeout: Duration::from_secs(cfg.rawtx.p2p_timeout_seconds.max(1)),
        chain,
    };
    let resolver = Resolver::new(Arc::clone(store), &rc)?;
    Ok(Arc::new(
        resolver.with_local(Arc::new(CoreBlockSource::new(core.clone()))),
    ))
}

fn build_shared(cfg: &Config, chain: Chain) -> R<Arc<Shared>> {
    let store = open_store(cfg, chain)?;
    let core = core_client(cfg, chain)?;
    let identity = require_identity(&store, cfg, chain, &core)?;
    let resolver = build_resolver(cfg, chain, &store, &core)?;
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
        identity,
        data_dir: cfg.data_dir(chain),
        listen: cfg.server.listen.clone(),
        max_clients: cfg.server.max_clients,
        state: std::sync::atomic::AtomicU8::new(state::BUILDING),
        lock: Arc::new(RwLock::new(())),
        stats: Stats::default(),
    });
    Ok(shared)
}

fn start_transports(shared: &Arc<Shared>, cfg: &Config) -> R<()> {
    let server = Arc::clone(shared);
    el_serve::start_server(server).map_err(|e| format!("cannot bind {}: {e}", shared.listen))?;
    if cfg.metrics.enabled {
        let m = Arc::clone(shared);
        el_serve::start_metrics(m, cfg.metrics.listen.clone())
            .map_err(|e| format!("cannot bind metrics {}: {e}", cfg.metrics.listen))?;
    }
    el_serve::start_mempool(Arc::clone(shared));
    el_serve::start_notifications(Arc::clone(shared));
    Ok(())
}

fn cmd_serve(cfg: &Config, chain: Chain) -> R<()> {
    let shared = build_shared(cfg, chain)?;
    let tip = shared
        .store
        .committed_tip()?
        .map(|t| t.height as u64)
        .unwrap_or(0);
    let core_tip = match shared.core.blockchain_info() {
        Ok(info) => {
            check_pruned(&info, shared.store.committed_tip()?.as_ref())?;
            info.blocks
        }
        Err(e) => {
            eprintln!(
                "[e-lite] Core unavailable at startup ({e}); serving the cached index (height {tip})"
            );
            tip
        }
    };
    shared.stats.core_height.store(core_tip, Ordering::Relaxed);
    if tip >= core_tip && tip > 0 {
        shared.set_state(state::READY);
    } else {
        shared.set_state(state::BUILDING);
        eprintln!(
            "[e-lite] index is behind (indexed {tip}, core {core_tip}); queries will fail until caught up. Use `e-lite run` to index and serve."
        );
    }
    start_transports(&shared, cfg)?;
    println!(
        "serving Electrum {} on {} (network {}, indexed height {})",
        el_serve::PROTOCOL_MAX,
        cfg.server.listen,
        chain.name(),
        tip
    );
    let flag = shutdown_flag();
    while !flag.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_secs(1));
    }
    println!("[e-lite] shutdown requested; serving stopped");
    Ok(())
}

fn cmd_run(cfg: &Config, chain: Chain) -> R<()> {
    let shared = build_shared(cfg, chain)?;
    start_transports(&shared, cfg)?;
    shared.set_state(state::BUILDING);
    println!(
        "e-lite {}: indexing and serving on {} (network {}, core {})",
        VERSION,
        cfg.server.listen,
        chain.name(),
        cfg.rpc_url(chain)
    );
    let flag = shutdown_flag();
    let idx = Indexer::new(
        Arc::clone(&shared.store),
        shared.core.clone(),
        cfg.clone(),
        Arc::clone(&shared.lock),
        Some(Arc::clone(&shared)),
        Arc::clone(&flag),
    );
    let mut supervisor = el_chain::Supervisor::new(supervisor_config(cfg));
    let poll = Duration::from_secs(cfg.pruning.poll_seconds.max(1));
    let mut last_tick = Instant::now()
        .checked_sub(poll)
        .unwrap_or_else(Instant::now);
    let mut first = true;
    let mut backoff = 1u64;
    loop {
        if flag.load(Ordering::SeqCst) {
            break;
        }
        match idx.catch_up_limited(512) {
            Ok(height) => {
                shared.set_state(if supervisor.paused() {
                    state::PAUSE_DOWNLOAD
                } else {
                    state::READY
                });
                backoff = 1;
                if first {
                    println!("index ready at height {height}");
                    first = false;
                }
            }
            Err(e) => {
                let msg = e.to_string();
                if is_permanent_error(&msg) {
                    shared.set_state(state::FATAL);
                    return Err(e);
                }
                // A transient Core outage must not kill a server that can still
                // answer confirmed reads from its own index.
                shared.set_state(state::CORE_UNAVAILABLE);
                eprintln!(
                    "[e-lite] transient error: {msg}; retrying in {backoff}s (serving cached index)"
                );
                for _ in 0..backoff {
                    if flag.load(Ordering::SeqCst) {
                        break;
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
                backoff = (backoff * 2).min(30);
                continue;
            }
        }
        if last_tick.elapsed() >= poll {
            match supervisor_tick(
                &mut supervisor,
                &shared.core,
                &shared.store,
                Some(shared.readers.as_ref()),
                &shared.data_dir,
            ) {
                Ok(actions) => {
                    apply_supervisor_actions(&mut supervisor, &shared.core, actions, Some(&shared))
                }
                Err(e) => eprintln!("[e-lite] supervisor: {e}"),
            }
            last_tick = Instant::now();
        }
        // `run` always follows the tip: it is the daemon mode that indexes and
        // serves. `--no-follow` only applies to the one-shot `index` command.
        std::thread::sleep(Duration::from_secs(1));
    }
    println!("[e-lite] shutdown requested; stopped cleanly");
    Ok(())
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
    let store = open_store(cfg, chain)?;
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
        "index     height {}   txnum {}   lag {}",
        g("indexed_height"),
        g("txnum"),
        g("lag_blocks")
    );
    println!(
        "core      height {}   last block {} ms",
        g("core_height"),
        g("last_block_ms")
    );
    println!(
        "storage   {:.2} GiB   free {:.2} GiB",
        g("db_bytes") as f64 / 1e9,
        g("disk_free_bytes") as f64 / 1e9
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
    fn full_node_allows_any_tip() {
        assert!(check_pruned(&info(false, 0), None).is_ok());
    }

    #[test]
    fn pruned_with_empty_index_is_rejected() {
        assert!(check_pruned(&info(true, 100), None).is_err());
    }

    #[test]
    fn pruned_gap_is_rejected() {
        // Index stops at 50, Core only has blocks from 100 onwards.
        assert!(check_pruned(&info(true, 100), Some(&tip(50))).is_err());
        assert!(check_pruned(&info(true, 100), Some(&tip(98))).is_err());
    }

    #[test]
    fn pruned_with_covered_tip_is_allowed() {
        assert!(check_pruned(&info(true, 100), Some(&tip(99))).is_ok());
        assert!(check_pruned(&info(true, 100), Some(&tip(150))).is_ok());
    }
}
