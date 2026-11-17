//! Unit tests (split from main.rs in alpha.29.17.0).

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

#[test]
fn help_flags_are_documented() {
    let help = help_text();
    let docs = include_str!("../../../docs/CLI.md");
    for word in help.split_whitespace() {
        if !word.starts_with("--") {
            continue;
        }
        let flag = word.trim_end_matches([',', ':', ';']);
        assert!(
            docs.contains(flag),
            "{flag} is in `help` but not in docs/CLI.md"
        );
    }
}

/// AUD-173: the panic hook keeps the node up when a wallet-facing thread
/// panics and aborts the process for any other thread. Abort cannot be
/// observed in-process, so the test runs itself again as a child.
#[test]
fn panic_hook_contains_wallet_threads_and_aborts_the_rest() {
    use std::os::unix::process::ExitStatusExt;
    const CHILD: &str = "E_LITE_PANIC_HOOK_CHILD";
    if let Ok(which) = std::env::var(CHILD) {
        crate::install_panic_hook();
        let name = if which == "contained" {
            el_model::sync::THREAD_CLIENT
        } else {
            "el-prune"
        };
        let _ = std::thread::Builder::new()
            .name(name.to_string())
            .spawn(|| panic!("injected"))
            .unwrap()
            .join();
        std::process::exit(0);
    }
    let exe = std::env::current_exe().unwrap();
    for (which, survives) in [("contained", true), ("critical", false)] {
        let out = std::process::Command::new(&exe)
            .args([
                "--exact",
                "tests::panic_hook_contains_wallet_threads_and_aborts_the_rest",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, which)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        if survives {
            assert!(out.status.success(), "{which}: {:?}\n{stderr}", out.status);
            assert!(stderr.contains("the node keeps running"), "{stderr}");
            assert!(
                !stderr.contains("injected"),
                "wallet panic message leaked: {stderr}"
            );
        } else {
            assert_eq!(
                out.status.signal(),
                Some(libc::SIGABRT),
                "{which}: {stderr}"
            );
            assert!(
                stderr.contains("FATAL: internal error in el-prune"),
                "{stderr}"
            );
        }
    }
}

/// AUD-175: the class of an indexing error comes from its type, built where
/// it is detected, never from the words in the message.
#[test]
fn indexing_errors_are_classified_by_type_not_by_text() {
    let class = |r: R<()>| classify_error(r.unwrap_err().as_ref());
    // Producers of the failures that used to be matched by text.
    assert_eq!(
        class(check_pruned(&info(true, 100), None, None)),
        ErrorClass::Permanent
    );
    assert_eq!(
        class(check_pruned(&info(true, 100), Some(&tip(50)), None)),
        ErrorClass::Permanent
    );
    let regtest = NetworkIdentity::new(Chain::Regtest, Chain::Regtest.known_genesis().unwrap());
    let other = NetworkIdentity::new(Chain::Regtest, Hash32([7; 32]));
    assert_eq!(
        class(verify_identity(&regtest, &other)),
        ErrorClass::Permanent
    );
    let mut signet = regtest.clone();
    signet.signet_challenge = Some(Hash32([1; 32]));
    assert_eq!(
        class(verify_identity(&regtest, &signet)),
        ErrorClass::Permanent
    );
    // A database of another network.
    let dir = std::env::temp_dir().join(format!("e-lite-classify-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let store = Store::open(&dir).unwrap();
        store.init_identity(&regtest).unwrap();
        let wrong = stored_identity(&store, Chain::Signet).map(|_| ());
        assert_eq!(class(wrong), ErrorClass::Permanent);
    }
    let _ = std::fs::remove_dir_all(&dir);

    // The words alone decide nothing any more: an RPC hiccup that mentions
    // "schema" or "txids.dat" is retried, and a fatal error reworded stays
    // fatal.
    assert_eq!(
        class(fail("rpc timed out reading the schema")),
        ErrorClass::Transient
    );
    assert_eq!(
        class(fail("txids.dat: Resource temporarily unavailable")),
        ErrorClass::Transient
    );
    assert_eq!(
        class(fatal("something new and unrecoverable")),
        ErrorClass::Permanent
    );

    // A wrapped Fatal is still fatal.
    #[derive(Debug)]
    struct Wrapper(Fatal);
    impl std::fmt::Display for Wrapper {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "context: {}", self.0)
        }
    }
    impl std::error::Error for Wrapper {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }
    let wrapped: E = Box::new(Wrapper(Fatal("deep".into())));
    assert_eq!(classify_error(wrapped.as_ref()), ErrorClass::Permanent);

    // Core still starting is transient, and recognised for the log line.
    assert!(el_rpc::is_core_loading(
        "getblockchaininfo failed: \"Loading block index…\" (code -28)"
    ));
    assert!(!el_rpc::is_core_loading("connection refused"));
}

/// A sample value for a retired flag, different from the default.
fn retired_sample(kind: Retired) -> Option<&'static str> {
    match kind {
        Retired::Str => Some("sample-value"),
        Retired::Int | Retired::IntMin1 => Some("3"),
        Retired::Float => Some("0.42"),
        Retired::List => Some("10.0.0.1:8333, 10.0.0.2:8333"),
        Retired::Const(_) => None,
    }
}

fn argv(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// Every retired flag gives exactly the configuration of its `--set`, and
/// the key exists with the right type (sections refuse unknown fields).
#[test]
fn every_retired_flag_equals_its_set() {
    for (flag, key, kind) in DEPRECATED_FLAGS {
        let mut old = vec!["run".to_string(), flag.to_string()];
        old.extend(retired_sample(*kind).map(str::to_string));
        let parsed = parse_args_from(old).unwrap_or_else(|e| panic!("{flag}: {e}"));
        assert_eq!(parsed.set.len(), 1, "{flag}");
        assert!(
            parsed.set[0].starts_with(&format!("{key}=")),
            "{flag}: {}",
            parsed.set[0]
        );
        assert_eq!(parsed.deprecations.len(), 1, "{flag}");
        assert!(parsed.deprecations[0].contains(&parsed.set[0]), "{flag}");
        let via_flag = Config::load_with_overrides(None, &parsed.set)
            .unwrap_or_else(|e| panic!("{flag} -> {}: {e}", parsed.set[0]));
        let explicit = parse_args_from(argv(&["run", "--set", &parsed.set[0]])).unwrap();
        let via_set = Config::load_with_overrides(None, &explicit.set).unwrap();
        assert_eq!(via_flag, via_set, "{flag}");
        let key_retired = config::RETIRED_KEYS.iter().any(|(k, _)| k == key);
        if key_retired {
            // The setting itself is gone: the flag is accepted and ignored.
            assert_eq!(via_flag, Config::default(), "{flag}");
        } else if retired_sample(*kind).is_some() {
            assert_ne!(via_flag, Config::default(), "{flag} changed nothing");
        }
    }
}

#[test]
fn retired_flags_keep_their_old_value_rules() {
    let cfg = |items: &[&str]| {
        let args = parse_args_from(argv(items)).unwrap();
        Config::load_with_overrides(None, &args.set).unwrap()
    };
    // Clamped to 1 as the flags did.
    assert_eq!(cfg(&["run", "--batch", "0"]).index.batch_blocks, 1);
    assert_eq!(cfg(&["run", "--min-free-gib", "0"]).pruning.min_free_gib, 1);
    // Inline values and lists.
    assert_eq!(cfg(&["run", "--readers=3"]).index.readers, 3);
    assert_eq!(
        cfg(&["run", "--p2p-peers", "a:1,,b:2 "]).rawtx.p2p_peers,
        vec!["a:1".to_string(), "b:2".to_string()]
    );
    // A string that looks like a number stays a string.
    assert_eq!(cfg(&["run", "--profile", "123"]).index.profile, "123");
    // Each pair of switches gives opposite values.
    assert!(!cfg(&["run", "--no-adaptive"]).index.adaptive);
    assert!(cfg(&["run", "--no-adaptive", "--adaptive"]).index.adaptive);
    // A bad number is still an error naming the flag.
    let err = parse_args_from(argv(&["run", "--readers", "many"]))
        .err()
        .unwrap()
        .to_string();
    assert!(err.contains("--readers"), "{err}");
}

/// A retired flag is a `--set` in its position: the later one wins.
#[test]
fn a_later_set_overrides_a_retired_flag_and_the_other_way_round() {
    let readers = |items: &[&str]| {
        let args = parse_args_from(argv(items)).unwrap();
        Config::load_with_overrides(None, &args.set)
            .unwrap()
            .index
            .readers
    };
    assert_eq!(
        readers(&["run", "--readers", "5", "--set", "index.readers=6"]),
        6
    );
    assert_eq!(
        readers(&["run", "--set", "index.readers=6", "--readers", "5"]),
        5
    );
}

/// The flags that stay are not in the retired table.
#[test]
fn kept_flags_are_not_retired() {
    for flag in [
        "--config",
        "--network",
        "--data-dir",
        "--set",
        "--log-level",
        "--core-rpc",
        "--core-datadir",
        "--cookie-file",
        "--rpc-user",
        "--rpc-password",
        "--manage",
        "--prune-mib",
        "--listen",
        "--tls-listen",
        "--no-tls",
        "--plain-lan",
        "--metrics",
        "--no-tor",
        "--no-rawtx-tor",
        "--allow-clearnet",
        "--no-supervise",
        "--no-follow",
        "--allow-mainnet",
        "--allow-low-memory",
        "--allow-auto-prune",
        "--yes",
        "--dry-run",
        "--output",
        "--input",
        "--with-core",
        "--verify",
        "--expect-digest",
        "--compress-level",
        "--hash-files",
        "--no-hash-files",
        "--purge",
        "--public-servers",
        "--core-arg",
        "--no-log-file",
        "--json",
    ] {
        assert!(retired_flag(flag).is_none(), "{flag}");
    }
}

/// A configuration that still has a removed key starts (with a warning);
/// any other unknown key is still an error.
#[test]
fn retired_config_keys_are_ignored_and_unknown_keys_still_fail() {
    let dir = std::env::temp_dir().join(format!("e-lite-retired-keys-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    let mut text = String::new();
    for (key, _) in config::RETIRED_KEYS {
        let (section, field) = key.split_once('.').unwrap();
        text.push_str(&format!("[{section}]\n{field} = 1\n"));
    }
    // Several retired keys can share a section: write each section once.
    let mut by_section: std::collections::BTreeMap<&str, Vec<&str>> = Default::default();
    for (key, _) in config::RETIRED_KEYS {
        let (section, field) = key.split_once('.').unwrap();
        by_section.entry(section).or_default().push(field);
    }
    text.clear();
    for (section, fields) in &by_section {
        text.push_str(&format!("[{section}]\n"));
        for field in fields {
            text.push_str(&format!("{field} = 1\n"));
        }
    }
    std::fs::write(&path, &text).unwrap();
    let cfg = Config::load(Some(path.to_str().unwrap())).expect("retired keys must not fail");
    assert_eq!(cfg, Config::default());
    std::fs::write(&path, "[index]\nnot_a_real_key = 1\n").unwrap();
    assert!(Config::load(Some(path.to_str().unwrap())).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}
