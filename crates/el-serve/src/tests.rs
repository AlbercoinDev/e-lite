//! Unit tests (split from lib.rs in alpha.29.17.0).

use super::*;

#[test]
fn electrumx_compat_banner_still_names_e_lite() {
    assert_eq!(banner(false), format!("e-lite {VERSION}"));
    let compat = banner(true);
    // Sparrow lowercases the banner and enables batching when it
    // contains "electrumx"; prefix checks still see e-lite.
    assert!(compat.starts_with("e-lite "), "{compat}");
    assert!(compat.to_lowercase().contains("electrumx"), "{compat}");
}

#[test]
fn version_negotiation() {
    assert_eq!(
        server_version(&json!(["client", "1.4"]), None).unwrap(),
        json!([format!("e-lite {VERSION}"), "1.4"])
    );
    assert_eq!(
        server_version(&json!(["client", ["1.4", "1.4.2"]]), None).unwrap()[1],
        json!("1.4.2")
    );
    assert!(server_version(&json!(["client", ["1.4", "1.4"]]), None).is_ok());
    assert!(server_version(&json!(["client", "1.2"]), None).is_err());
    assert!(server_version(&json!(["client", ["1.0", "1.2"]]), None).is_err());
    // Gaps and future versions fall back to the previous available one.
    for requested in ["1.4.3", "1.5"] {
        assert_eq!(
            server_version(&json!(["client", requested]), None).unwrap()[1],
            json!("1.4.2"),
            "{requested} must fall back to 1.4.2"
        );
    }
    assert_eq!(
        server_version(&json!(["client", "1.6"]), None).unwrap()[1],
        json!("1.6"),
        "1.6 must negotiate 1.6"
    );
    for requested in ["1.7", "1.8"] {
        assert_eq!(
            server_version(&json!(["client", requested]), None).unwrap()[1],
            json!("1.7"),
            "{requested} must negotiate the newest available version below it"
        );
    }
    assert_eq!(
        server_version(&json!(["client", ["1.5", "1.8"]]), None).unwrap()[1],
        json!("1.7")
    );
    // An inverted range is a parameter error, not a version mismatch.
    assert_eq!(
        server_version(&json!(["client", ["1.6", "1.4"]]), None)
            .unwrap_err()
            .code,
        -32602
    );
    // Extraneous arguments are tolerated for future protocol extensions.
    assert!(server_version(&json!(["client", "1.4", "junk", 9]), None).is_ok());
}

#[test]
fn verbose_transaction_gets_chain_context() {
    let block = BlockRecord {
        height: 7,
        hash: Hash32([0x11; 32]),
        header: [0u8; 80],
        tx_count: 1,
        txnum_base: 0,
    };
    let mut decoded = json!({"txid": "aa", "vout": []});
    decorate_verbose_tx(&mut decoded, "deadbeef", &block, 9).unwrap();
    assert_eq!(decoded["hex"], json!("deadbeef"));
    assert_eq!(decoded["blockhash"], json!(block.hash.to_hex()));
    assert_eq!(decoded["confirmations"], json!(3));
    assert_eq!(decoded["time"], json!(0));
    assert_eq!(decoded["blocktime"], json!(0));
    assert_eq!(decoded["in_active_chain"], json!(true));
}

#[test]
fn submit_package_summary_maps_core_errors() {
    let success = json!({
        "package_msg": "success",
        "tx-results": {"wtxid": {"txid": "aa"}}
    });
    assert_eq!(submit_package_summary(&success), json!({"success": true}));

    let failure = json!({
        "package_msg": "transaction failed",
        "tx-results": {
            "wtxid": {"txid": "aa", "error": "min relay fee not met"}
        }
    });
    assert_eq!(
        submit_package_summary(&failure),
        json!({
            "success": false,
            "errors": [{"txid": "aa", "error": "min relay fee not met"}]
        })
    );
}

#[test]
fn rate_limit_waits_instead_of_rejecting() {
    let start = Instant::now();
    let mut limit = RateLimit::new(1, 10);
    assert!(!limit.acquire(start), "first request is immediate");
    let before = Instant::now();
    assert!(limit.acquire(start), "second request waits for a token");
    assert!(
        before.elapsed() >= Duration::from_millis(80),
        "the limiter must delay, not reject"
    );

    let mut disabled = RateLimit::new(1, 0);
    for _ in 0..10 {
        assert!(!disabled.acquire(start), "rate 0 never throttles");
    }
}

#[test]
fn dirty_scripts_dedupe_and_force_all() {
    let dirty = DirtyScripts::new();
    let a = ScriptHash::from_script(b"a");
    let b = ScriptHash::from_script(b"b");
    dirty.mark(a);
    dirty.mark(a);
    dirty.mark_all([b]);
    let (set, force) = dirty.take();
    assert_eq!(set.len(), 2);
    assert!(set.contains(&a));
    assert!(set.contains(&b));
    assert!(!force);
    let (set, force) = dirty.take();
    assert!(set.is_empty());
    assert!(!force);

    dirty.mark_all_subscriptions();
    let (set, force) = dirty.take();
    assert!(set.is_empty());
    assert!(force);
}

#[test]
fn request_line_is_bounded() {
    use std::io::Cursor;
    let mut r = Cursor::new(b"{\"a\":1}\nnext\n".to_vec());
    let mut buf = Vec::new();
    assert!(read_line_limited(&mut r, &mut buf, 64).unwrap());
    assert_eq!(buf, b"{\"a\":1}\n");
    buf.clear();
    assert!(read_line_limited(&mut r, &mut buf, 64).unwrap());
    assert_eq!(buf, b"next\n");

    let mut r = Cursor::new(vec![b'x'; 100]);
    let mut buf = Vec::new();
    assert!(read_line_limited(&mut r, &mut buf, 16).is_err());
}

#[test]
fn scripthash_param_is_reversed() {
    let sh = ScriptHash::from_script(b"\x51");
    let v = json!([sh.to_electrum_hex()]);
    assert_eq!(param_scripthash(&v).unwrap(), sh);
    assert!(param_scripthash(&json!(["zz"])).is_err());
}

#[test]
fn banner_sections_and_formatting() {
    let (indexing, ready) = split_banner("[indexing]\nINDEX {height}\n[ready]\nREADY {percent}\n");
    assert_eq!(indexing.as_deref(), Some("INDEX {height}\n"));
    assert_eq!(ready.as_deref(), Some("READY {percent}\n"));
    let (both, _) = split_banner("plain {state}\n");
    assert_eq!(both.as_deref(), Some("plain {state}\n"));
    assert_eq!(progress_bar(50.0), "[###############---------------]");
    assert_eq!(format_num(974_584), "974,584");
    assert_eq!(eta_text(0, 10), "0m");
    assert_eq!(eta_text(100, 0), "unknown");
}

#[test]
fn tls_config_rejects_missing_and_invalid_files() {
    assert!(build_tls_config(
        Path::new("/nonexistent/e-lite-cert.pem"),
        Path::new("/nonexistent/e-lite-key.pem"),
    )
    .is_err());

    let dir = std::env::temp_dir().join(format!(
        "e-lite-tls-config-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let cert = dir.join("cert.pem");
    std::fs::write(&cert, b"not a certificate").unwrap();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let key = dir.join("key.pem");
    std::fs::copy(fixtures.join("tls-key.pem"), &key).unwrap();
    assert!(build_tls_config(&cert, &key).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn tls_watcher_reloads_changed_pem_and_keeps_old_on_error() {
    let dir = std::env::temp_dir().join(format!(
        "e-lite-tls-watch-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");
    std::fs::copy(fixtures.join("tls-cert.pem"), &cert).unwrap();
    std::fs::copy(fixtures.join("tls-key.pem"), &key).unwrap();

    let watcher = CertWatcher::load(cert.clone(), key, Duration::ZERO).unwrap();
    let first = watcher.config();

    // A new mtime with the same valid certificate rebuilds the config.
    filetime::set_file_mtime(&cert, filetime::FileTime::from_unix_time(2_000_000_000, 0)).unwrap();
    let second = watcher.config();
    assert!(!Arc::ptr_eq(&first, &second));

    // A corrupt certificate keeps the previous configuration in use.
    std::fs::write(&cert, b"not a certificate").unwrap();
    filetime::set_file_mtime(&cert, filetime::FileTime::from_unix_time(2_000_000_001, 0)).unwrap();
    let third = watcher.config();
    assert!(Arc::ptr_eq(&second, &third));

    // Restoring a valid certificate recovers.
    std::fs::copy(fixtures.join("tls-cert.pem"), &cert).unwrap();
    filetime::set_file_mtime(&cert, filetime::FileTime::from_unix_time(2_000_000_002, 0)).unwrap();
    let fourth = watcher.config();
    assert!(!Arc::ptr_eq(&third, &fourth));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn tx_get_failures_are_counted_by_cause() {
    use el_rawtx::RawtxError;
    let of = |e: RawtxError| TxGetFailure::of(&e);
    assert_eq!(of(RawtxError::NoSource("x".into())), TxGetFailure::NoSource);
    assert_eq!(
        of(RawtxError::Unavailable("x".into())),
        TxGetFailure::ExternalFailed
    );
    assert_eq!(
        of(RawtxError::Verify("x".into())),
        TxGetFailure::VerifyFailed
    );
    let counters = TxGetFailures::default();
    counters.add(TxGetFailure::ExternalFailed);
    assert_eq!(counters.add(TxGetFailure::ExternalFailed), 2);
    let json = counters.to_json();
    assert_eq!(json["external_failed"], 2);
    assert_eq!(json["no_source"], 0);
    assert_eq!(json.as_object().unwrap().len(), TxGetFailure::ALL.len());
}

#[test]
fn notify_never_waits_for_a_busy_connection() {
    use std::io::BufRead;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let peer = std::net::TcpStream::connect(addr).unwrap();
    let (server_side, _) = listener.accept().unwrap();
    let registry = ClientRegistry::new();
    let client = registry.register(Wire::plain(server_side).unwrap());

    // The client's own thread holds the lock (a long response or poll).
    let busy = client.writer.lock().unwrap();
    let started = Instant::now();
    assert!(client.notify(&json!({"method": "blockchain.headers.subscribe"})));
    assert!(started.elapsed() < Duration::from_millis(200));
    drop(busy);
    // Releasing through `send` delivers the queued notification after
    // the response, on its own line.
    assert!(client.send(&json!({"id": 1, "result": null})));

    let mut reader = std::io::BufReader::new(peer);
    let mut first = String::new();
    let mut second = String::new();
    reader.read_line(&mut first).unwrap();
    reader.read_line(&mut second).unwrap();
    assert!(first.contains("\"id\":1"), "{first}");
    assert!(second.contains("headers.subscribe"), "{second}");
}

#[test]
fn notifier_wakes_on_a_mark_instead_of_polling() {
    // AUD-146: a commit or mempool change wakes the notifier at once.
    let dirty = Arc::new(DirtyScripts::new());
    let marker = Arc::clone(&dirty);
    let started = Instant::now();
    let handle = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        marker.mark(ScriptHash(Hash32([1; 32])));
    });
    dirty.wait(Duration::from_secs(10));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "woken by the mark"
    );
    handle.join().unwrap();
    // Pending marks return immediately; an empty set waits the timeout.
    let started = Instant::now();
    dirty.wait(Duration::from_secs(10));
    assert!(started.elapsed() < Duration::from_secs(1));
    let (taken, _) = dirty.take();
    assert_eq!(taken.len(), 1);
    let started = Instant::now();
    dirty.wait(Duration::from_millis(100));
    assert!(started.elapsed() >= Duration::from_millis(90));
}
