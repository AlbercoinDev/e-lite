//! C7 — Electrum 1.4 fixtures exercised through the real JSON-RPC dispatcher.

use bitcoin::blockdata::constants::genesis_block;
use bitcoin::Network;
use el_mempool::Mempool;
use el_model::{
    BlockRecord, Chain, Hash32, HistoryEffect, IndexedTip, NetworkIdentity, Outpoint, ScriptHash,
    UtxoValue,
};
use el_rawtx::{BlockSource, RawtxConfig, RawtxError, Resolver};
use el_rpc::{CoreClient, CoreConfig};
use el_serve::{process_line, state, ClientRegistry, Shared, Stats};
use el_store::Store;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::AtomicU8;
use std::sync::{Arc, RwLock};
use std::time::Duration;

struct Fixture {
    dir: PathBuf,
    shared: Arc<Shared>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn fixture() -> (Fixture, ScriptHash, Hash32) {
    let dir = std::env::temp_dir().join(format!(
        "e-lite-protocol-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Arc::new(Store::open(&dir).unwrap());

    let chain = Chain::Regtest;
    let genesis = Chain::Regtest.known_genesis().unwrap();
    let identity = NetworkIdentity::new(chain, genesis);
    store.init_identity(&identity).unwrap();

    let blockchain = Hash32::from_hex(&"aa".repeat(32)).unwrap();
    let txid = Hash32::from_hex(&"bb".repeat(32)).unwrap();
    let script = ScriptHash::from_script(b"\x51");
    let op = Outpoint { txid, vout: 0 };
    let utxo = UtxoValue {
        value_sat: 1000,
        script_hash: script,
        creation_height: 0,
        coinbase: true,
    };

    let mut batch = Store::batch();
    store
        .put_block(
            &mut batch,
            &BlockRecord {
                height: 0,
                hash: blockchain,
                header: [0x42; 80],
                tx_count: 1,
                txnum_base: 0,
            },
        )
        .unwrap();
    store.put_occurrence(&mut batch, txid, 0, 0).unwrap();
    store
        .put_history(
            &mut batch,
            script,
            0,
            0,
            &HistoryEffect {
                received_sat: 1000,
                spent_sat: 0,
                flags: 0,
            },
        )
        .unwrap();
    store.put_utxo(&mut batch, &op, &utxo).unwrap();
    store
        .commit(
            batch,
            &IndexedTip {
                height: 0,
                block_hash: blockchain,
                schema_version: el_model::codec::SCHEMA_VERSION,
                txnum: 1,
            },
            &[txid],
        )
        .unwrap();

    let cookie = dir.join("cookie");
    std::fs::write(&cookie, "user:pass").unwrap();
    let core = CoreClient::connect(&CoreConfig {
        rpc_url: "http://127.0.0.1:1".to_string(),
        cookie_file: Some(cookie),
        core_datadir: None,
        rpc_user: None,
        rpc_password: None,
        chain,
        timeout: Duration::from_millis(200),
    })
    .unwrap();

    let resolver = Arc::new(
        Resolver::new(
            Arc::clone(&store),
            &RawtxConfig {
                enabled: true,
                cache_mib: 0,
                decoys: 0,
                tor_socks: None,
                allow_clearnet: true,
                use_p2p: false,
                use_esplora: false,
                p2p_v2: false,
                esplora_urls: Vec::new(),
                p2p_peers: Vec::new(),
                p2p_timeout: Duration::from_secs(1),
                chain,
            },
        )
        .unwrap(),
    );

    let shared = Arc::new(Shared {
        store,
        core,
        resolver,
        mempool: Arc::new(RwLock::new(Mempool::new())),
        relayfee: std::sync::Mutex::new(0.0),
        fee_estimates: std::sync::Mutex::new(std::collections::HashMap::new()),
        error_log: std::sync::Mutex::new(std::collections::HashMap::new()),
        clients: Arc::new(ClientRegistry::new()),
        readers: el_chain::ReaderTracker::new(),
        silent_payments_enabled: false,
        identity: RwLock::new(identity),
        headers: el_serve::HeaderCache::new(),
        db_bytes: std::sync::Mutex::new(el_serve::DbBytesCache::default()),
        dirty_scripts: el_serve::DirtyScripts::new(),
        data_dir: dir.clone(),
        core_datadir: dir.clone(),
        log_file: dir.join("e-lite.log"),
        listen: "127.0.0.1:0".to_string(),
        tls_listen: None,
        onion: std::sync::Mutex::new(None),
        onion_plain_port: 50001,
        onion_tls_port: 50002,
        banner_file: None,
        max_clients: 4,
        state: AtomicU8::new(state::READY),
        shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        lock: Arc::new(RwLock::new(())),
        stats: Stats::default(),
    });

    (
        Fixture {
            dir,
            shared: Arc::clone(&shared),
        },
        script,
        txid,
    )
}

fn call(shared: &Shared, id: u64, method: &str, params: Value) -> Value {
    let line = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string();
    process_line(shared, &line)
}

#[test]
fn electrum_14_fixtures() {
    let (fx, script, txid) = fixture();
    let s = &fx.shared;

    let r = call(s, 1, "server.version", json!(["test", "1.4"]));
    assert_eq!(r["result"][1], "1.4", "{r}");
    assert!(r["result"][0].as_str().unwrap().starts_with("e-lite "));

    let r = call(s, 2, "server.features", json!([]));
    assert_eq!(
        r["result"]["genesis_hash"],
        Chain::Regtest.known_genesis().unwrap().to_hex()
    );
    assert_eq!(r["result"]["hash_function"], "sha256");
    assert_eq!(r["result"]["protocol_min"], "1.4");
    assert_eq!(r["result"]["protocol_max"], "1.4.2");

    let r = call(s, 3, "server.ping", json!([]));
    assert!(r["result"].is_null());

    let r = call(s, 4, "blockchain.headers.subscribe", json!([]));
    assert_eq!(r["result"]["height"], 0);
    assert_eq!(
        r["result"]["hex"].as_str().unwrap(),
        hex::encode([0x42u8; 80])
    );

    let r = call(s, 5, "blockchain.block.header", json!([0]));
    assert_eq!(r["result"].as_str().unwrap().len(), 160);

    let r = call(s, 6, "blockchain.block.headers", json!([0, 10]));
    assert_eq!(r["result"]["count"], 1);
    assert_eq!(r["result"]["max"], 2016);

    let sh = script.to_electrum_hex();
    let r = call(s, 7, "blockchain.scripthash.get_history", json!([sh]));
    assert_eq!(r["result"].as_array().unwrap().len(), 1);
    assert_eq!(r["result"][0]["tx_hash"], txid.to_hex());
    assert_eq!(r["result"][0]["height"], 0);

    let r = call(s, 8, "blockchain.scripthash.get_balance", json!([sh]));
    assert_eq!(r["result"]["confirmed"], 1000);
    assert_eq!(r["result"]["unconfirmed"], 0);

    let r = call(s, 9, "blockchain.scripthash.listunspent", json!([sh]));
    assert_eq!(r["result"].as_array().unwrap().len(), 1);
    assert_eq!(r["result"][0]["tx_hash"], txid.to_hex());
    assert_eq!(r["result"][0]["value"], 1000);

    let r = call(s, 10, "blockchain.scripthash.subscribe", json!([sh]));
    assert_eq!(r["result"].as_str().unwrap().len(), 64);

    let r = call(
        s,
        11,
        "blockchain.transaction.get_merkle",
        json!([txid.to_hex(), 0]),
    );
    assert_eq!(r["result"]["pos"], 0);
    assert_eq!(r["result"]["merkle"].as_array().unwrap().len(), 0);

    let r = call(
        s,
        12,
        "blockchain.transaction.id_from_pos",
        json!([0, 0, true]),
    );
    assert_eq!(r["result"]["tx_hash"], txid.to_hex());

    let r = call(
        s,
        13,
        "blockchain.scripthash.get_history",
        json!(["00".repeat(32)]),
    );
    assert_eq!(r["result"], json!([]));

    let r = call(s, 14, "does.not.exist", json!([]));
    assert_eq!(r["error"]["code"], -32601);

    let r = call(s, 15, "server.add_peer", json!([{}]));
    assert_eq!(r["result"], json!(false), "{r}");
}

#[test]
fn waiting_for_core_rejects_features() {
    let (fx, _, _) = fixture();
    fx.shared.set_state(state::WAITING_FOR_CORE);
    let r = call(&fx.shared, 1, "server.features", json!([]));
    assert_eq!(r["error"]["code"], -32000);
    assert!(r["error"]["message"]
        .as_str()
        .unwrap()
        .contains("waiting for Bitcoin Core"));
}

#[test]
fn building_state_is_not_an_empty_answer() {
    let (fx, script, _) = fixture();
    fx.shared.set_state(state::BUILDING);
    let r = call(
        &fx.shared,
        1,
        "blockchain.scripthash.get_history",
        json!([script.to_electrum_hex()]),
    );
    assert_eq!(r["error"]["code"], -32000);
    assert!(r["error"]["message"].as_str().unwrap().contains("building"));
}

#[test]
fn malformed_json_and_bad_params() {
    let (fx, _, _) = fixture();
    let r = process_line(&fx.shared, "{not json");
    assert_eq!(r["error"]["code"], -32700);
    let r = call(
        &fx.shared,
        1,
        "blockchain.scripthash.get_history",
        json!(["zz"]),
    );
    assert_eq!(r["error"]["code"], -32602);
    let r = call(&fx.shared, 2, "blockchain.block.header", json!(["nope"]));
    assert_eq!(r["error"]["code"], -32602);
}

#[test]
fn incompatible_protocol_is_rejected() {
    let (fx, _, _) = fixture();
    let r = call(&fx.shared, 1, "server.version", json!(["old", "1.2"]));
    assert_eq!(r["error"]["code"], -32000);
}

#[test]
fn silent_payments_are_gated_with_an_explicit_error() {
    let (fx, _, _) = fixture();
    for method in [
        "blockchain.silentpayments.subscribe",
        "blockchain.silentpayments.unsubscribe",
    ] {
        let r = call(&fx.shared, 1, method, json!(["00", "00", 0, []]));
        assert_eq!(r["error"]["code"], -32000, "{r}");
        assert!(
            r["error"]["message"]
                .as_str()
                .unwrap()
                .contains("silent payments"),
            "{r}"
        );
    }
}

struct SlowSource {
    block: Vec<u8>,
}

impl BlockSource for SlowSource {
    fn name(&self) -> &str {
        "slow"
    }

    fn fetch_block(&self, _hash: &Hash32) -> std::result::Result<Vec<u8>, RawtxError> {
        std::thread::sleep(Duration::from_millis(500));
        Ok(self.block.clone())
    }
}

#[test]
fn slow_transaction_get_does_not_hold_the_index_lock() {
    let dir = std::env::temp_dir().join(format!(
        "e-lite-lock-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Arc::new(Store::open(&dir).unwrap());
    let block = genesis_block(Network::Regtest);
    let block_hash = Hash32::from_hex(&block.block_hash().to_string()).unwrap();
    let header: [u8; 80] = bitcoin::consensus::serialize(&block.header)
        .try_into()
        .unwrap();
    let txid = Hash32::from_hex(&block.txdata[0].compute_txid().to_string()).unwrap();
    let identity = NetworkIdentity::new(Chain::Regtest, block_hash);
    store.init_identity(&identity).unwrap();
    let mut batch = Store::batch();
    store
        .put_block(
            &mut batch,
            &BlockRecord {
                height: 0,
                hash: block_hash,
                header,
                tx_count: 1,
                txnum_base: 0,
            },
        )
        .unwrap();
    store.put_occurrence(&mut batch, txid, 0, 0).unwrap();
    store
        .commit(
            batch,
            &IndexedTip {
                height: 0,
                block_hash,
                schema_version: el_model::codec::SCHEMA_VERSION,
                txnum: 1,
            },
            &[txid],
        )
        .unwrap();

    let cookie = dir.join("cookie");
    std::fs::write(&cookie, "user:pass").unwrap();
    let core = CoreClient::connect(&CoreConfig {
        rpc_url: "http://127.0.0.1:1".to_string(),
        cookie_file: Some(cookie),
        core_datadir: None,
        rpc_user: None,
        rpc_password: None,
        chain: Chain::Regtest,
        timeout: Duration::from_millis(100),
    })
    .unwrap();
    let resolver = Arc::new(
        Resolver::new(
            Arc::clone(&store),
            &RawtxConfig {
                enabled: false,
                cache_mib: 0,
                decoys: 0,
                tor_socks: None,
                allow_clearnet: true,
                use_p2p: false,
                use_esplora: false,
                p2p_v2: false,
                esplora_urls: Vec::new(),
                p2p_peers: Vec::new(),
                p2p_timeout: Duration::from_secs(1),
                chain: Chain::Regtest,
            },
        )
        .unwrap()
        .with_external(Arc::new(SlowSource {
            block: bitcoin::consensus::serialize(&block),
        })),
    );
    let shared = Arc::new(Shared {
        store,
        core,
        resolver,
        mempool: Arc::new(RwLock::new(Mempool::new())),
        relayfee: std::sync::Mutex::new(0.0),
        fee_estimates: std::sync::Mutex::new(std::collections::HashMap::new()),
        error_log: std::sync::Mutex::new(std::collections::HashMap::new()),
        clients: Arc::new(ClientRegistry::new()),
        readers: el_chain::ReaderTracker::new(),
        silent_payments_enabled: false,
        identity: RwLock::new(identity),
        headers: el_serve::HeaderCache::new(),
        db_bytes: std::sync::Mutex::new(el_serve::DbBytesCache::default()),
        dirty_scripts: el_serve::DirtyScripts::new(),
        data_dir: dir.clone(),
        core_datadir: dir.clone(),
        log_file: dir.join("e-lite.log"),
        listen: "127.0.0.1:0".to_string(),
        tls_listen: None,
        onion: std::sync::Mutex::new(None),
        onion_plain_port: 50001,
        onion_tls_port: 50002,
        banner_file: None,
        max_clients: 4,
        state: AtomicU8::new(state::READY),
        shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        lock: Arc::new(RwLock::new(())),
        stats: Stats::default(),
    });

    let worker = Arc::clone(&shared);
    let call = std::thread::spawn(move || {
        let line = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "blockchain.transaction.get",
            "params": [txid.to_hex()]
        })
        .to_string();
        process_line(&worker, &line)
    });
    std::thread::sleep(Duration::from_millis(80));
    // The resolver is still fetching the block. The index commit lock must be
    // free: a slow transaction.get must never stall indexing.
    let guard = shared
        .lock
        .try_write()
        .expect("index lock must not be held during resolver I/O");
    drop(guard);
    let response = call.join().unwrap();
    assert!(response.get("error").is_none(), "{response}");
    assert!(response["result"].as_str().unwrap().len() > 100);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fee_and_mempool_methods_never_return_method_not_found() {
    let (fx, script, _) = fixture();
    let r = call(&fx.shared, 1, "blockchain.relayfee", json!([]));
    assert!(r["result"].as_f64().unwrap() > 0.0, "{r}");
    let r = call(&fx.shared, 2, "blockchain.estimatefee", json!([6]));
    assert!(r["result"].as_f64().unwrap() > 0.0, "{r}");
    let r = call(&fx.shared, 3, "mempool.get_fee_histogram", json!([]));
    assert_eq!(r["result"], json!([]), "{r}");
    let r = call(
        &fx.shared,
        4,
        "blockchain.scripthash.get_mempool",
        json!([script.to_electrum_hex()]),
    );
    assert_eq!(r["result"], json!([]), "{r}");
}

#[test]
fn tls_listener_completes_handshake_and_serves() {
    use rustls::pki_types::pem::PemObject;
    use std::io::{BufRead, Write};

    let (fx, _script, _txid) = fixture();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let cert = fixtures.join("tls-cert.pem");
    let key = fixtures.join("tls-key.pem");
    let ca = fixtures.join("tls-ca.pem");
    let watcher = Arc::new(el_serve::CertWatcher::load(cert.clone(), key, Duration::ZERO).unwrap());
    el_serve::start_listener_on(listener, Arc::clone(&fx.shared), Some(watcher));

    let mut roots = rustls::RootCertStore::empty();
    for item in rustls::pki_types::CertificateDer::pem_file_iter(&ca).unwrap() {
        roots.add(item.unwrap()).unwrap();
    }
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let conn = rustls::ClientConnection::new(Arc::new(config), name).unwrap();
    let sock = std::net::TcpStream::connect(addr).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut tls = rustls::StreamOwned::new(conn, sock);

    tls.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"server.ping\",\"params\":[]}\n")
        .unwrap();
    tls.flush().unwrap();

    let mut reader = std::io::BufReader::new(tls);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let response: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(response["id"], json!(7));
    assert_eq!(response["result"], Value::Null);
    assert!(
        fx.shared
            .stats
            .tls_handshakes
            .load(std::sync::atomic::Ordering::Relaxed)
            >= 1
    );

    // The notifier must be able to write while the reader is idle: this is the
    // reason the read loop releases the TLS connection lock between attempts.
    let clients = fx.shared.clients.snapshot();
    assert_eq!(clients.len(), 1);
    assert!(clients[0].send(&json!({
        "jsonrpc": "2.0",
        "method": "e-lite.test.notification",
        "params": ["ok"],
    })));
    line.clear();
    reader.read_line(&mut line).unwrap();
    let notification: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(notification["method"], "e-lite.test.notification");
    assert_eq!(notification["params"], json!(["ok"]));
}

#[test]
fn notifications_are_not_answered() {
    let (fx, _script, _txid) = fixture();
    let outcome = el_serve::process_line_for(
        &fx.shared,
        None,
        r#"{"jsonrpc":"2.0","method":"server.ping","params":[]}"#,
    );
    assert!(
        outcome.response.is_none(),
        "notification must not be answered"
    );
    assert!(!outcome.close);

    let outcome = el_serve::process_line_for(
        &fx.shared,
        None,
        r#"{"jsonrpc":"2.0","id":1,"method":"server.ping","params":[]}"#,
    );
    assert_eq!(outcome.response.unwrap()["result"], Value::Null);
}

#[test]
fn server_version_negotiation_and_connection_lifecycle() {
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};

    let (fx, script, _txid) = fixture();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    el_serve::start_listener_on(listener, Arc::clone(&fx.shared), None);

    let sock = TcpStream::connect(addr).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut writer = sock.try_clone().unwrap();
    let mut reader = BufReader::new(sock);

    let mut send = |value: Value| {
        writer.write_all(value.to_string().as_bytes()).unwrap();
        writer.write_all(b"\n").unwrap();
        writer.flush().unwrap();
    };
    let mut read = || {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        serde_json::from_str::<Value>(&line).unwrap()
    };

    send(
        json!({"jsonrpc":"2.0","id":1,"method":"server.version","params":["test",["1.4","1.4.2"]]}),
    );
    let response = read();
    assert_eq!(response["result"][1], "1.4.2", "{response}");

    send(json!({"jsonrpc":"2.0","id":2,"method":"server.version","params":["test","1.4"]}));
    let response = read();
    assert_eq!(response["error"]["code"], -32600, "{response}");

    let sh = script.to_electrum_hex();
    send(json!({"jsonrpc":"2.0","id":3,"method":"blockchain.scripthash.subscribe","params":[sh]}));
    let response = read();
    assert!(response["result"].is_string(), "{response}");

    send(
        json!({"jsonrpc":"2.0","id":4,"method":"blockchain.scripthash.unsubscribe","params":[sh]}),
    );
    let response = read();
    assert_eq!(response["result"], json!(true), "{response}");

    send(
        json!({"jsonrpc":"2.0","id":5,"method":"blockchain.scripthash.unsubscribe","params":[sh]}),
    );
    let response = read();
    assert_eq!(response["result"], json!(false), "{response}");

    // A connection with no common protocol gets the error and is closed.
    let sock2 = TcpStream::connect(addr).unwrap();
    sock2
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut writer2 = sock2.try_clone().unwrap();
    let mut reader2 = BufReader::new(sock2);
    writer2
        .write_all(br#"{"jsonrpc":"2.0","id":1,"method":"server.version","params":["old","1.2"]}"#)
        .unwrap();
    writer2.write_all(b"\n").unwrap();
    writer2.flush().unwrap();
    let mut line = String::new();
    reader2.read_line(&mut line).unwrap();
    let response: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(response["error"]["code"], -32000, "{response}");
    line.clear();
    assert_eq!(
        reader2.read_line(&mut line).unwrap(),
        0,
        "server must close after a fatal version mismatch"
    );
}
