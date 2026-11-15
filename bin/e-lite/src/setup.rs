//! setup module (split from main.rs in alpha.29.17.0).

use super::*;

pub(crate) fn effective_config(args: &Args) -> R<(Config, Chain)> {
    effective_config_with(args, true)
}

/// `probe_core = false` is for maintenance commands that only need the data
/// directory (`cache`): resolving `network = auto` must not wait on Core or
/// open the database (which a running daemon holds locked).
pub(crate) fn effective_config_with(args: &Args, probe_core: bool) -> R<(Config, Chain)> {
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
    if let Some(v) = args.use_tor {
        cfg.rawtx.use_tor = v;
    }
    if !cfg.rawtx.use_tor && cfg.rawtx.tor_fallback_clearnet {
        eprintln!(
            "[e-lite] WARNING: rawtx.use_tor = false makes tor_fallback_clearnet meaningless; \
             external fetches are direct"
        );
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

pub(crate) fn chain_from_core(name: &str) -> Option<Chain> {
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
pub(crate) fn resolve_chain(cfg: &Config) -> R<Chain> {
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

pub(crate) fn resolve_chain_offline(cfg: &Config) -> R<Chain> {
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

pub(crate) fn detect_chain_from_db(cfg: &Config) -> R<Option<Chain>> {
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

pub(crate) fn core_client_with_timeout(
    cfg: &Config,
    chain: Chain,
    timeout: Duration,
) -> R<CoreClient> {
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

pub(crate) fn core_client(cfg: &Config, chain: Chain) -> R<CoreClient> {
    core_client_with_timeout(
        cfg,
        chain,
        Duration::from_secs(cfg.core.timeout_seconds.max(1)),
    )
}

pub(crate) fn core_identity(core: &CoreClient, chain: Chain) -> R<NetworkIdentity> {
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

pub(crate) fn total_ram_mib() -> Option<u64> {
    tuning::total_ram_mib()
}

pub(crate) fn effective_cache_mib(cfg: &Config) -> u64 {
    tuning::initial_cache_mib(cfg)
}

pub(crate) fn min_free_bytes(cfg: &Config, chain: Chain) -> Option<u64> {
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

pub(crate) const GIB_BYTES: u64 = 1024 * 1024 * 1024;

pub(crate) fn disk_low_bytes(cfg: &Config) -> u64 {
    cfg.pruning.min_free_gib.saturating_mul(GIB_BYTES)
}

pub(crate) fn disk_critical_bytes(cfg: &Config) -> u64 {
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
pub(crate) fn disk_resume_bytes(cfg: &Config) -> u64 {
    let low = disk_low_bytes(cfg);
    low.saturating_add((low / 2).clamp(512 * 1024 * 1024, 5 * GIB_BYTES))
}

pub(crate) fn warn_if_disk_low(cfg: &Config, chain: Chain) {
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

pub(crate) fn open_store_with(
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

pub(crate) fn open_store(cfg: &Config, chain: Chain) -> R<Arc<Store>> {
    let jobs = std::thread::available_parallelism()
        .map(|n| n.get().clamp(1, 8))
        .unwrap_or(4);
    open_store_with(cfg, chain, effective_cache_mib(cfg), jobs)
}

pub(crate) fn build_bulk(
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

pub(crate) fn stored_identity(store: &Store, chain: Chain) -> R<Option<NetworkIdentity>> {
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

pub(crate) fn verify_identity(stored: &NetworkIdentity, live: &NetworkIdentity) -> R<()> {
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

/// Fitted Core `-dbcache` that e-lite must leave out of its own budget. Zero
/// when Core is external or already answering RPC: its cache is already
/// reflected in `MemAvailable`.
pub(crate) fn core_reserve_mib(cfg: &Config, core_running: bool) -> u64 {
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
pub(crate) fn preflight(
    cfg: &Config,
    chain: Chain,
    require_min_ram: bool,
    core_running: bool,
) -> R<()> {
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

pub(crate) fn build_resolver(
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
    // `rawtx.use_tor = false` is the explicit direct opt-out (AUD-168): no
    // SOCKS proxy for any external fetch, and clearnet implied.
    let tor = cfg.rawtx.effective_tor_socks();
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
        allow_clearnet: cfg.rawtx.effective_allow_clearnet(),
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
pub(crate) fn p2p_discovery_enabled(cfg: &Config) -> bool {
    cfg.rawtx.enabled && cfg.rawtx.use_p2p && cfg.rawtx.p2p_peers.is_empty()
}

/// Up to 8 peers from Core that can serve old witness blocks: its current
/// **outbound** connections first (addrman holds many stale addresses and
/// each dead peer costs a timeout), then addrman. Returns `(peers,
/// connected)`.
pub(crate) fn discover_p2p_peers(core: &CoreClient) -> (Vec<el_rawtx::PeerHint>, usize) {
    let connected = core.connected_peers().unwrap_or_default();
    let need_addrman = select_p2p_peers(&connected, &[]).0.len() < P2P_PEERS;
    let addrman = if need_addrman {
        core.node_addresses(64).unwrap_or_default()
    } else {
        Vec::new()
    };
    select_p2p_peers(&connected, &addrman)
}

pub(crate) const P2P_PEERS: usize = 8;

/// Whether e-lite can reach a peer of this Core network: through Tor or
/// directly, never I2P/CJDNS (`""` = an old Core that does not say).
pub(crate) fn p2p_network_reachable(network: &str) -> bool {
    matches!(network, "" | "ipv4" | "ipv6" | "onion")
}

/// Filter and order Core's peers for block fetches (AUD-154):
/// - inbound peers are skipped: their `addr` is an ephemeral source port, a
///   connection to it is refused (and used to trip the Tor breaker);
/// - `NODE_NETWORK` and `NODE_WITNESS` are required (old witness blocks);
/// - I2P/CJDNS are unreachable through Tor;
/// - onion peers go first so they survive the cap: they need no exit node,
///   and exit policies often reject port 8333.
pub(crate) fn select_p2p_peers(
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
pub(crate) fn p2p_refresh_interval(peers: usize) -> Duration {
    if peers == 0 {
        Duration::from_secs(60)
    } else {
        Duration::from_secs(600)
    }
}

/// Refresh the P2P peer list from Core for the whole session (AUD-137).
pub(crate) fn spawn_p2p_refresh(shared: Arc<Shared>, cfg: &Config) {
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

pub(crate) fn requires_ready_provider(cfg: &Config, chain: Chain, has_esplora: bool) -> bool {
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
pub(crate) fn provider_gate_required(
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
pub(crate) fn provider_gate_expired(cfg: &Config, waited: Duration) -> bool {
    cfg.public_servers.open_without_provider
        && waited >= Duration::from_secs(cfg.public_servers.ready_timeout_seconds)
}

/// Keep probing in the background after the listeners opened without a
/// ready provider, so `/status.public_servers.provider_ready` and the log
/// reflect a provider that comes back. Only constant, wallet-free requests
/// are sent (`/block-height/0` and the genesis block).
pub(crate) fn spawn_provider_reprobe(shared: Arc<Shared>, cfg: Config) {
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

pub(crate) fn effective_rate_limit(cfg: &Config) -> (u64, u64) {
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
pub(crate) fn log_startup_diagnostics(cfg: &Config, chain: Chain, shared: &Shared) {
    if cfg.rawtx.enabled && !cfg.rawtx.use_tor {
        eprintln!(
            "[e-lite] WARNING: external fetches use DIRECT connections (rawtx.use_tor = false); \
             public providers and P2P peers see this machine's IP"
        );
    }
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
        "[e-lite] debug: rawtx: path {} | probe {}s (tor >= {}s), ready {}s, tor \
         connect/stall/attempt {}/{}/{}s (onion x2), p2p {}s (tor {}s), race {} (hedge tor \
         {}/{}s, direct {}/{}s), prefetch {}, decoys {}, max_parallel {}, clearnet fallback {}",
        cfg.rawtx.transport(),
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

pub(crate) fn build_shared(
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
        rawtx_path: cfg.rawtx.transport().to_string(),
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

pub(crate) fn refresh_providers(shared: &Shared, cfg: &Config, wait: bool) {
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

pub(crate) fn publish_memory(shared: &Shared) {
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

pub(crate) fn is_loopback_listen(addr: &str) -> bool {
    let host = addr.rsplit_once(':').map(|(h, _)| h).unwrap_or(addr);
    host == "localhost" || host == "[::1]" || host == "::1" || host.starts_with("127.")
}

pub(crate) fn warn_public_listen(what: &str, addr: &str) {
    if !is_loopback_listen(addr) {
        eprintln!(
            "[e-lite] WARNING: {what} is listening on {addr}, not loopback. \
             {what} has no transport encryption or client authentication; use a \
             firewall, an SSH/VPN tunnel, a TLS reverse proxy or Tor, and restrict \
             who can connect."
        );
    }
}

pub(crate) fn start_tls_transport(shared: &Arc<Shared>, cfg: &Config) -> R<()> {
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

pub(crate) struct Transports {
    pub(crate) mempool: std::thread::JoinHandle<()>,
    pub(crate) notifications: std::thread::JoinHandle<()>,
    pub(crate) memory: std::thread::JoinHandle<()>,
}

pub(crate) fn start_transports(shared: &Arc<Shared>, cfg: &Config) -> R<Transports> {
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
