//! The `index` command and the indexing loop: startup, the tuning controller,
//! the Core observer, hot reload (SIGUSR1) and shutdown.

use super::*;

/// Background controller: runs independently of the indexing pipeline so a
/// single long `catch_up_limited` call cannot delay memory protection. The
/// step itself is rate-limited by `maybe_adapt`.
pub(crate) fn spawn_controller(
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
        })
        .unwrap_or_else(|e| {
            eprintln!("[e-lite] cannot start the tuning thread: {e}");
            std::thread::spawn(|| {})
        })
}

pub(crate) fn cmd_index(cfg: &Config, chain: Chain) -> R<()> {
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

pub(crate) fn sleep_with_flag(seconds: u64, flag: &Arc<AtomicBool>) -> bool {
    for _ in 0..seconds {
        if flag.load(Ordering::SeqCst) {
            return false;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    !flag.load(Ordering::SeqCst)
}

/// Reload the hot subset of the config. Cold fields are reported but require a
/// restart; the last valid config stays in effect on parse/validation errors.
pub(crate) fn reload_hot(
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
                || new.pruning.max_bulk_lag_gib != old.pruning.max_bulk_lag_gib
                || new.pruning.core_resume_ahead_gib != old.pruning.core_resume_ahead_gib
                || new.pruning.pause_core_during_bulk != old.pruning.pause_core_during_bulk
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

/// Poll Core once per second into a shared snapshot. The block-application
/// loop then reads that snapshot instead of opening an RPC connection every
/// 100 ms; auto-prune keeps a synchronous fallback for safety.
pub(crate) fn spawn_core_observer(idx: &Indexer, flag: Arc<AtomicBool>) {
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
pub(crate) fn index_loop(
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
                match classify_error(e.as_ref()) {
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
                        let loading = el_rpc::is_core_loading(&msg);
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
                idx.supervisor_step(&mut supervisor);
                last_tick = Instant::now();
            } else {
                std::thread::sleep(Duration::from_millis(200));
            }
        } else if last_tick.elapsed() >= poll
            || (idx.downloads_paused() && last_tick.elapsed() >= Duration::from_secs(1))
        {
            idx.supervisor_step(&mut supervisor);
            last_tick = Instant::now();
        }
    }
}
