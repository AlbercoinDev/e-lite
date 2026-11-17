//! serve module (split from main.rs in alpha.29.17.0).

use super::*;

pub(crate) fn cmd_serve(cfg: &Config, chain: Chain) -> R<()> {
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

pub(crate) fn cmd_run(cfg: &Config, chain: Chain) -> R<()> {
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
