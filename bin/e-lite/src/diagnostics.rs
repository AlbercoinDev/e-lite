//! diagnostics module (split from main.rs in alpha.29.17.0).

use super::*;

/// `e-lite arc-setup`: print (and optionally apply, as root) the setup that
/// lets e-lite manage the ZFS ARC cap without running as root: the sysfs
/// parameter becomes group-writable and a systemd oneshot restores the
/// permissions after every boot.
pub(crate) fn cmd_arc_setup(apply: bool) -> R<()> {
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
pub(crate) fn group_of_user(user: &str) -> Option<String> {
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

pub(crate) fn cmd_init(cfg: &Config, chain: Chain) -> R<()> {
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
pub(crate) fn publish_tor_ports(cfg: &Config, rt: Option<&tor::TorRuntime>, shared: &Shared) {
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

pub(crate) fn cmd_doctor(cfg: &Config, chain: Chain) -> R<()> {
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

    // The transport actually in force (rawtx.use_tor, AUD-168).
    let tor = cfg.rawtx.effective_tor_socks();
    let external_enabled = cfg.rawtx.enabled
        && (cfg.rawtx.use_p2p || cfg.rawtx.use_esplora)
        && (tor.is_some() || cfg.rawtx.effective_allow_clearnet());

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

pub(crate) fn cmd_core(cfg: &Config, chain: Chain, action: Option<&str>) -> R<()> {
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

pub(crate) fn cmd_onion(
    cfg: &Config,
    chain: Chain,
    action: Option<&str>,
    purge: bool,
    yes: bool,
) -> R<()> {
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
pub(crate) fn free_bytes(path: &std::path::Path) -> Option<u64> {
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
pub(crate) fn cmd_cache(
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

pub(crate) fn print_problems(p: &[String]) {
    for x in p {
        eprintln!("PROBLEM: {x}");
    }
}

pub(crate) fn bind_ok(addr: &str) -> bool {
    std::net::TcpListener::bind(addr).is_ok()
}

pub(crate) fn cmd_status(cfg: &Config, chain: Chain, as_json: bool) -> R<()> {
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

pub(crate) fn print_status_human(v: &serde_json::Value) {
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

pub(crate) fn cmd_export(
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

pub(crate) fn cmd_import(
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

pub(crate) fn http_get(addr: &str, path: &str) -> R<String> {
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
