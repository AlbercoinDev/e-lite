//! Electrum JSON-RPC 1.4 server and local metrics.
//!
//! Each request reads the index through a shared read lock; the indexer commits
//! under the write lock, so no response mixes two tips.

use el_chain::ReaderTracker;
use el_mempool::Mempool;
use el_model::{merkle_branch, Hash32, NetworkIdentity, ScriptHash};
use el_rawtx::Resolver;
use el_rpc::CoreClient;
use el_store::Store;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

pub const PROTOCOL_MIN: &str = "1.4";
pub const PROTOCOL_MAX: &str = "1.4";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const MAX_HEADERS_CHUNK: usize = 2016;

pub mod state {
    pub const BUILDING: u8 = 0;
    pub const READY: u8 = 1;
    pub const DEGRADED: u8 = 2;
    pub const REORG: u8 = 3;
    pub const FATAL: u8 = 4;
    pub const CORE_UNAVAILABLE: u8 = 5;
    pub const PAUSE_DOWNLOAD: u8 = 6;
    pub const PRUNING: u8 = 7;

    pub fn name(v: u8) -> &'static str {
        match v {
            BUILDING => "BUILDING_INDEX",
            READY => "READY",
            DEGRADED => "DEGRADED_RAWTX",
            REORG => "REORG_RECOVERY",
            FATAL => "FATAL_INTEGRITY",
            CORE_UNAVAILABLE => "CORE_UNAVAILABLE",
            PAUSE_DOWNLOAD => "PAUSE_DOWNLOAD",
            PRUNING => "PRUNING",
            _ => "UNKNOWN",
        }
    }
}

pub struct Stats {
    pub indexed_height: AtomicU64,
    pub core_height: AtomicU64,
    pub txnum: AtomicU64,
    pub requests_total: AtomicU64,
    pub errors_total: AtomicU64,
    pub clients: AtomicUsize,
    pub last_block_ms: AtomicU64,
    pub notifier_runs: AtomicU64,
    pub notifications_sent: AtomicU64,
    pub notifier_errors: AtomicU64,
    pub started: Instant,
}

impl Default for Stats {
    fn default() -> Self {
        Stats {
            indexed_height: AtomicU64::new(0),
            core_height: AtomicU64::new(0),
            txnum: AtomicU64::new(0),
            requests_total: AtomicU64::new(0),
            errors_total: AtomicU64::new(0),
            clients: AtomicUsize::new(0),
            last_block_ms: AtomicU64::new(0),
            notifier_runs: AtomicU64::new(0),
            notifications_sent: AtomicU64::new(0),
            notifier_errors: AtomicU64::new(0),
            started: Instant::now(),
        }
    }
}

pub struct ClientState {
    pub id: u64,
    pub writer: Mutex<TcpStream>,
    pub subs: Mutex<HashMap<ScriptHash, Option<String>>>,
    pub headers: AtomicBool,
    pub last_height: AtomicU64,
    pub alive: AtomicBool,
}

impl ClientState {
    pub fn send(&self, value: &Value) -> bool {
        if !self.alive.load(Ordering::Relaxed) {
            return false;
        }
        let mut bytes = match serde_json::to_vec(value) {
            Ok(b) => b,
            Err(_) => return false,
        };
        bytes.push(b'\n');
        let mut writer = match self.writer.lock() {
            Ok(w) => w,
            Err(_) => return false,
        };
        if writer
            .write_all(&bytes)
            .and_then(|_| writer.flush())
            .is_err()
        {
            self.alive.store(false, Ordering::SeqCst);
            return false;
        }
        true
    }
}

#[derive(Default)]
pub struct ClientRegistry {
    next: AtomicU64,
    clients: Mutex<HashMap<u64, Arc<ClientState>>>,
}

impl ClientRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, writer: TcpStream) -> Arc<ClientState> {
        let id = self.next.fetch_add(1, Ordering::SeqCst) + 1;
        let state = Arc::new(ClientState {
            id,
            writer: Mutex::new(writer),
            subs: Mutex::new(HashMap::new()),
            headers: AtomicBool::new(false),
            last_height: AtomicU64::new(0),
            alive: AtomicBool::new(true),
        });
        if let Ok(mut c) = self.clients.lock() {
            c.insert(id, Arc::clone(&state));
        }
        state
    }

    pub fn unregister(&self, id: u64) {
        if let Ok(mut c) = self.clients.lock() {
            if let Some(state) = c.remove(&id) {
                state.alive.store(false, Ordering::SeqCst);
            }
        }
    }

    pub fn snapshot(&self) -> Vec<Arc<ClientState>> {
        self.clients
            .lock()
            .map(|c| c.values().cloned().collect())
            .unwrap_or_default()
    }

    pub fn len(&self) -> usize {
        self.clients.lock().map(|c| c.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

pub struct Shared {
    pub store: Arc<Store>,
    pub core: CoreClient,
    pub resolver: Arc<Resolver>,
    pub mempool: Arc<RwLock<Mempool>>,
    pub relayfee: Mutex<f64>,
    pub fee_estimates: Mutex<HashMap<u32, (f64, Instant)>>,
    pub error_log: Mutex<HashMap<String, Instant>>,
    pub clients: Arc<ClientRegistry>,
    pub readers: Arc<ReaderTracker>,
    pub silent_payments_enabled: bool,
    pub identity: NetworkIdentity,
    pub data_dir: std::path::PathBuf,
    pub listen: String,
    pub max_clients: usize,
    pub state: AtomicU8,
    pub lock: Arc<RwLock<()>>,
    pub stats: Stats,
}

impl Shared {
    pub fn set_state(&self, s: u8) {
        self.state.store(s, Ordering::SeqCst);
    }

    pub fn get_state(&self) -> u8 {
        self.state.load(Ordering::SeqCst)
    }

    pub fn status_json(&self) -> Value {
        let (height, txnum, hash) = match self.store.committed_tip() {
            Ok(Some(t)) => (t.height as u64, t.txnum, t.block_hash.to_hex()),
            _ => (0, 0, String::new()),
        };
        let core_height = self.stats.core_height.load(Ordering::Relaxed);
        let rawtx = self.resolver.stats();
        json!({
            "version": VERSION,
            "network": self.identity.chain.name(),
            "genesis": self.identity.genesis.to_hex(),
            "state": state::name(self.get_state()),
            "indexed_height": height,
            "core_height": core_height,
            "lag_blocks": core_height.saturating_sub(height),
            "txnum": txnum,
            "tip_hash": hash,
            "db_bytes": self.store.disk_usage_bytes(),
            "disk_free_bytes": disk_free_bytes(&self.data_dir),
            "clients": self.stats.clients.load(Ordering::Relaxed) as u64,
            "max_clients": self.max_clients as u64,
            "requests_total": self.stats.requests_total.load(Ordering::Relaxed),
            "errors_total": self.stats.errors_total.load(Ordering::Relaxed),
            "last_block_ms": self.stats.last_block_ms.load(Ordering::Relaxed),
            "uptime_seconds": self.stats.started.elapsed().as_secs(),
            "listen": self.listen,
            "protocol": PROTOCOL_MAX,
            "notifier": {
                "runs": self.stats.notifier_runs.load(Ordering::Relaxed),
                "sent": self.stats.notifications_sent.load(Ordering::Relaxed),
                "errors": self.stats.notifier_errors.load(Ordering::Relaxed),
            },
            "mempool": {
                "count": self.mempool.read().map(|m| m.len()).unwrap_or(0),
                "version": self.mempool.read().map(|m| m.version()).unwrap_or(0),
                "unresolved_inputs": self.mempool.read().map(|m| m.unresolved_inputs()).unwrap_or(0),
                "histogram_buckets": self.mempool.read().map(|m| m.fee_histogram().len()).unwrap_or(0),
            },
            "rawtx": {
                "cache_hits": rawtx.cache_hits.load(Ordering::Relaxed),
                "cache_puts": rawtx.cache_puts.load(Ordering::Relaxed),
                "fetches": rawtx.fetches.load(Ordering::Relaxed),
                "bytes": rawtx.bytes.load(Ordering::Relaxed),
                "verify_failures": rawtx.verify_failures.load(Ordering::Relaxed),
                "decoys": rawtx.decoys.load(Ordering::Relaxed),
                "unavailable": rawtx.unavailable.load(Ordering::Relaxed),
                "external_sources": self.resolver.external_sources(),
            },
        })
    }
}

pub fn disk_free_bytes(path: &std::path::Path) -> u64 {
    let out = std::process::Command::new("df")
        .args(["-P", "-k"])
        .arg(path)
        .output();
    if let Ok(out) = out {
        let text = String::from_utf8_lossy(&out.stdout);
        if let Some(line) = text.lines().nth(1) {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() >= 4 {
                if let Ok(kb) = cols[3].parse::<u64>() {
                    return kb.saturating_mul(1024);
                }
            }
        }
    }
    0
}

#[derive(Debug)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    fn new(code: i64, message: impl Into<String>) -> Self {
        RpcError {
            code,
            message: message.into(),
        }
    }
    fn invalid(msg: impl Into<String>) -> Self {
        Self::new(-32602, msg)
    }
    fn method(msg: impl Into<String>) -> Self {
        Self::new(-32601, msg)
    }
    fn server(msg: impl Into<String>) -> Self {
        Self::new(-32000, msg)
    }
}

type RpcResult<T> = std::result::Result<T, RpcError>;

pub const METHOD_NOT_FOUND: i64 = -32601;

fn log_rpc_error(shared: &Shared, method: &str, code: i64) {
    let key = format!("{method}:{code}");
    let Ok(mut seen) = shared.error_log.lock() else {
        return;
    };
    if let Some(at) = seen.get(&key) {
        if at.elapsed() < Duration::from_secs(60) {
            return;
        }
    }
    seen.insert(key, Instant::now());
    eprintln!("[e-lite] rpc error: method={method} code={code}");
}

pub fn dispatch(shared: &Shared, method: &str, params: &Value) -> RpcResult<Value> {
    dispatch_for(shared, None, method, params)
}

pub fn dispatch_for(
    shared: &Shared,
    client: Option<&Arc<ClientState>>,
    method: &str,
    params: &Value,
) -> RpcResult<Value> {
    match method {
        "server.version" => server_version(params),
        "server.ping" => Ok(Value::Null),
        "server.banner" => Ok(json!(format!(
            "e-lite {} — no wallet registration",
            VERSION
        ))),
        "server.donation_address" => Ok(json!("")),
        "server.peers.subscribe" => Ok(json!([])),
        "server.features" => Ok(server_features(shared)),
        _ => {
            // Everything else reads the index: enforce a consistent tip and a
            // non-misleading state.
            let _guard = shared
                .lock
                .read()
                .map_err(|_| RpcError::server("lock poisoned"))?;
            check_ready(shared)?;
            match method {
                "blockchain.headers.subscribe" => headers_subscribe(shared, client),
                "blockchain.block.header" => block_header(shared, params),
                "blockchain.block.headers" => block_headers(shared, params),
                "blockchain.scripthash.get_history" => scripthash_history(shared, params),
                "blockchain.scripthash.get_balance" => scripthash_balance(shared, params),
                "blockchain.scripthash.listunspent" => scripthash_listunspent(shared, params),
                "blockchain.scripthash.subscribe" => scripthash_subscribe(shared, client, params),
                "blockchain.scripthash.unsubscribe" => {
                    if let Some(c) = client {
                        if let Ok(script) = param_scripthash(params) {
                            if let Ok(mut subs) = c.subs.lock() {
                                subs.remove(&script);
                            }
                        }
                    }
                    Ok(json!(true))
                }
                "blockchain.scripthash.get_mempool" => scripthash_mempool(shared, params),
                "mempool.get_fee_histogram" => fee_histogram(shared),
                "blockchain.relayfee" => relay_fee(shared),
                "blockchain.estimatefee" => estimate_fee(shared, params),
                "blockchain.block.stats" => block_stats(shared, params),
                "blockchain.transaction.broadcast" => broadcast_transaction(shared, params),
                "blockchain.silentpayments.subscribe" | "blockchain.silentpayments.unsubscribe" => {
                    silent_payments(shared, method)
                }
                "blockchain.transaction.get" => transaction_get(shared, params),
                "blockchain.transaction.get_merkle" => transaction_merkle(shared, params),
                "blockchain.transaction.id_from_pos" => transaction_id_from_pos(shared, params),
                _ => Err(RpcError::method(format!("unknown method: {method}"))),
            }
        }
    }
}

fn check_ready(shared: &Shared) -> RpcResult<()> {
    match shared.get_state() {
        state::READY
        | state::DEGRADED
        | state::CORE_UNAVAILABLE
        | state::PAUSE_DOWNLOAD
        | state::PRUNING => Ok(()),
        state::BUILDING => {
            let tip = shared
                .store
                .committed_tip()
                .ok()
                .flatten()
                .map(|t| t.height)
                .unwrap_or(0);
            Err(RpcError::server(format!(
                "index is building (indexed height {tip}); retry later"
            )))
        }
        state::REORG => Err(RpcError::server("reorg recovery in progress")),
        state::FATAL => Err(RpcError::server(
            "fatal integrity error; operator action required",
        )),
        _ => Err(RpcError::server("server not ready")),
    }
}

fn server_version(params: &Value) -> RpcResult<Value> {
    let arr = params
        .as_array()
        .ok_or_else(|| RpcError::invalid("server.version expects [client, protocol]"))?;
    let requested = arr.get(1);
    let compatible = match requested {
        None => true,
        Some(Value::String(v)) => v == PROTOCOL_MIN || v == PROTOCOL_MAX,
        Some(Value::Array(range)) => {
            let min = range
                .first()
                .and_then(|v| v.as_str())
                .unwrap_or(PROTOCOL_MIN);
            let max = range.get(1).and_then(|v| v.as_str()).unwrap_or(min);
            !(max < PROTOCOL_MIN || min > PROTOCOL_MAX)
        }
        _ => false,
    };
    if !compatible {
        return Err(RpcError::server(format!(
            "unsupported protocol; server supports {PROTOCOL_MIN}-{PROTOCOL_MAX}"
        )));
    }
    Ok(json!([format!("e-lite {VERSION}"), PROTOCOL_MAX]))
}

fn server_features(shared: &Shared) -> Value {
    json!({
        "genesis_hash": shared.identity.genesis.to_hex(),
        "hash_function": "sha256",
        "server_version": format!("e-lite {VERSION}"),
        "protocol_min": PROTOCOL_MIN,
        "protocol_max": PROTOCOL_MAX,
        "pruning": Value::Null,
        "hosts": {},
    })
}

fn tip_block(shared: &Shared) -> RpcResult<el_model::BlockRecord> {
    let tip = shared
        .store
        .committed_tip()
        .map_err(|e| RpcError::server(e.to_string()))?
        .ok_or_else(|| RpcError::server("index is empty"))?;
    shared
        .store
        .get_block(tip.height)
        .map_err(|e| RpcError::server(e.to_string()))?
        .ok_or_else(|| RpcError::server("tip block missing"))
}

fn headers_subscribe(shared: &Shared, client: Option<&Arc<ClientState>>) -> RpcResult<Value> {
    let rec = tip_block(shared)?;
    if let Some(c) = client {
        c.headers.store(true, Ordering::SeqCst);
        c.last_height.store(rec.height as u64, Ordering::SeqCst);
    }
    Ok(json!({
        "height": rec.height,
        "hex": hex::encode(rec.header),
    }))
}

fn param_u32(params: &Value, idx: usize) -> RpcResult<u32> {
    params
        .as_array()
        .and_then(|a| a.get(idx))
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
        .ok_or_else(|| RpcError::invalid("expected an integer parameter"))
}

fn header_chain(shared: &Shared, cp_height: u32) -> RpcResult<Vec<Hash32>> {
    let mut hashes = Vec::with_capacity(cp_height as usize + 1);
    for h in 0..=cp_height {
        let rec = shared
            .store
            .get_block(h)
            .map_err(|e| RpcError::server(e.to_string()))?
            .ok_or_else(|| RpcError::server(format!("height {h} not indexed")))?;
        hashes.push(rec.hash);
    }
    Ok(hashes)
}

fn checkpoint_proof(
    shared: &Shared,
    leaf_height: u32,
    cp_height: u32,
) -> RpcResult<(Hash32, Vec<Hash32>)> {
    if leaf_height > cp_height {
        return Err(RpcError::invalid(
            "height must be <= cp_height when cp_height is set",
        ));
    }
    let hashes = header_chain(shared, cp_height)?;
    merkle_branch(&hashes, leaf_height as usize)
        .ok_or_else(|| RpcError::server("cannot compute checkpoint branch"))
}

fn block_header(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let height = param_u32(params, 0)?;
    let cp_height = params
        .as_array()
        .and_then(|a| a.get(1))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let rec = shared
        .store
        .get_block(height)
        .map_err(|e| RpcError::server(e.to_string()))?
        .ok_or_else(|| RpcError::server(format!("height {height} not indexed")))?;
    let header = hex::encode(rec.header);
    if cp_height == 0 {
        return Ok(json!(header));
    }
    let (root, branch) = checkpoint_proof(shared, height, cp_height as u32)?;
    Ok(json!({
        "header": header,
        "branch": branch.iter().map(|h| h.to_hex()).collect::<Vec<_>>(),
        "root": root.to_hex(),
    }))
}

fn block_headers(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let start = param_u32(params, 0)?;
    let count = param_u32(params, 1)?.min(MAX_HEADERS_CHUNK as u32);
    let cp_height = params
        .as_array()
        .and_then(|a| a.get(2))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let mut hexs = String::new();
    let mut got = 0u32;
    for h in start..start.saturating_add(count) {
        match shared
            .store
            .get_block(h)
            .map_err(|e| RpcError::server(e.to_string()))?
        {
            Some(rec) => {
                hexs.push_str(&hex::encode(rec.header));
                got += 1;
            }
            None => break,
        }
    }
    let mut result = json!({
        "count": got,
        "hex": hexs,
        "max": MAX_HEADERS_CHUNK,
    });
    if cp_height != 0 && got > 0 {
        let last = start + got - 1;
        let (root, branch) = checkpoint_proof(shared, last, cp_height as u32)?;
        result["root"] = json!(root.to_hex());
        result["branch"] = json!(branch.iter().map(|h| h.to_hex()).collect::<Vec<_>>());
    }
    Ok(result)
}

fn param_scripthash(params: &Value) -> RpcResult<ScriptHash> {
    let s = params
        .as_array()
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .ok_or_else(|| RpcError::invalid("expected scripthash hex"))?;
    ScriptHash::from_electrum_hex(s).map_err(|e| RpcError::invalid(e.to_string()))
}

type HistoryRow = (u32, u32, String, u64, u64);

fn history_entries(shared: &Shared, script: ScriptHash) -> RpcResult<Vec<HistoryRow>> {
    let entries = shared
        .store
        .history_for_script(script)
        .map_err(|e| RpcError::server(e.to_string()))?;
    let mut blocks: HashMap<u32, el_model::BlockRecord> = HashMap::new();
    let mut out = Vec::with_capacity(entries.len());
    for (height, tx_pos, effect) in entries {
        let rec = match blocks.get(&height) {
            Some(r) => *r,
            None => {
                let r = shared
                    .store
                    .get_block(height)
                    .map_err(|e| RpcError::server(e.to_string()))?
                    .ok_or_else(|| RpcError::server(format!("block {height} missing")))?;
                blocks.insert(height, r);
                r
            }
        };
        let txnum = rec.txnum_base + tx_pos as u64;
        let txid = shared
            .store
            .txid_at(txnum)
            .map_err(|e| RpcError::server(e.to_string()))?;
        out.push((
            height,
            tx_pos,
            txid.to_hex(),
            effect.received_sat,
            effect.spent_sat,
        ));
    }
    Ok(out)
}

fn scripthash_history(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let script = param_scripthash(params)?;
    let entries = history_entries(shared, script)?;
    let mem = shared
        .mempool
        .read()
        .map_err(|_| RpcError::server("mempool lock poisoned"))?;
    let mem_entries = mem.history(script);
    let mut arr = Vec::with_capacity(entries.len() + mem_entries.len());
    for (height, _, txid, _, _) in entries {
        arr.push(json!({"tx_hash": txid, "height": height}));
    }
    // Electrum history appends mempool entries after the confirmed ones.
    for (txid, fee, height) in mem_entries {
        arr.push(json!({
            "tx_hash": txid.to_hex(),
            "height": height,
            "fee": fee,
        }));
    }
    Ok(Value::Array(arr))
}

fn scripthash_balance(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let script = param_scripthash(params)?;
    let utxos = shared
        .store
        .utxos_for_script(script)
        .map_err(|e| RpcError::server(e.to_string()))?;
    let mem = shared
        .mempool
        .read()
        .map_err(|_| RpcError::server("mempool lock poisoned"))?;
    // Confirmed balance ignores mempool events; the unconfirmed field carries
    // the mempool delta (which already subtracts spent confirmed UTXOs).
    let confirmed: u64 = utxos.iter().map(|(_, v, _, _)| *v).sum();
    Ok(json!({
        "confirmed": confirmed,
        "unconfirmed": mem.balance_delta(script),
    }))
}

fn scripthash_listunspent(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let script = param_scripthash(params)?;
    let utxos = shared
        .store
        .utxos_for_script(script)
        .map_err(|e| RpcError::server(e.to_string()))?;
    let mem = shared
        .mempool
        .read()
        .map_err(|_| RpcError::server("mempool lock poisoned"))?;
    let mut arr = Vec::new();
    for (op, value, height, _coinbase) in utxos {
        if mem.is_spent(&op) {
            continue;
        }
        arr.push(json!({
            "tx_hash": op.txid.to_hex(),
            "tx_pos": op.vout,
            "height": height,
            "value": value,
        }));
    }
    for (op, value) in mem.utxos(script) {
        arr.push(json!({
            "tx_hash": op.txid.to_hex(),
            "tx_pos": op.vout,
            "height": 0,
            "value": value,
        }));
    }
    Ok(Value::Array(arr))
}

fn script_status(shared: &Shared, script: ScriptHash) -> RpcResult<Option<String>> {
    let entries = history_entries(shared, script)?;
    let mem = shared
        .mempool
        .read()
        .map_err(|_| RpcError::server("mempool lock poisoned"))?;
    let mem_entries = mem.history(script);
    if entries.is_empty() && mem_entries.is_empty() {
        return Ok(None);
    }
    let mut hasher = Sha256::new();
    for (height, _, txid, _, _) in entries {
        hasher.update(format!("{txid}:{height}:").as_bytes());
    }
    for (txid, _, height) in mem_entries {
        hasher.update(format!("{txid}:{height}:").as_bytes());
    }
    Ok(Some(hex::encode(hasher.finalize())))
}

fn scripthash_subscribe(
    shared: &Shared,
    client: Option<&Arc<ClientState>>,
    params: &Value,
) -> RpcResult<Value> {
    let script = param_scripthash(params)?;
    let status = script_status(shared, script)?;
    if let Some(c) = client {
        if let Ok(mut subs) = c.subs.lock() {
            subs.insert(script, status.clone());
        }
    }
    match status {
        Some(status) => Ok(json!(status)),
        None => Ok(Value::Null),
    }
}

fn transaction_get(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let txid = params
        .as_array()
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .ok_or_else(|| RpcError::invalid("expected txid"))?;
    let verbose = params
        .as_array()
        .and_then(|a| a.get(1))
        .map(|v| v.as_bool().unwrap_or(false))
        .unwrap_or(false);
    let txid_hash = Hash32::from_hex(txid).map_err(RpcError::invalid)?;
    let occ = shared
        .store
        .occurrences(txid_hash)
        .map_err(|e| RpcError::server(e.to_string()))?;

    if verbose {
        let blockhash = match occ.first() {
            Some((height, _)) => shared
                .store
                .get_block(*height)
                .ok()
                .flatten()
                .map(|r| r.hash.to_hex()),
            None => None,
        };
        return shared
            .core
            .raw_transaction_json(txid, blockhash.as_deref())
            .map_err(|e| RpcError::server(format!("rawtx unavailable: {e}")));
    }

    match occ.first().copied() {
        Some((height, _)) => {
            // Keep the prune gate away from this height while we resolve.
            let _guard = shared.readers.enter(height);
            shared
                .resolver
                .raw_tx(txid_hash, height)
                .map(|raw| json!(raw))
                .map_err(|e| RpcError::server(e.to_string()))
        }
        None => shared
            .core
            .raw_transaction_hex_opt(txid, None)
            .map(|raw| json!(raw))
            .map_err(|e| RpcError::server(format!("transaction not found: {e}"))),
    }
}

fn fee_histogram(shared: &Shared) -> RpcResult<Value> {
    let mem = shared
        .mempool
        .read()
        .map_err(|_| RpcError::server("mempool lock poisoned"))?;
    Ok(Value::Array(
        mem.fee_histogram()
            .into_iter()
            .map(|(rate, size)| json!([rate, size]))
            .collect(),
    ))
}

fn relay_fee(shared: &Shared) -> RpcResult<Value> {
    let mut fee = *shared
        .relayfee
        .lock()
        .map_err(|_| RpcError::server("fee lock poisoned"))?;
    if fee <= 0.0 {
        if let Ok(info) = shared.core.network_info() {
            if info.relayfee > 0.0 {
                fee = info.relayfee;
                *shared
                    .relayfee
                    .lock()
                    .map_err(|_| RpcError::server("fee lock poisoned"))? = fee;
            }
        }
    }
    if fee <= 0.0 {
        fee = 0.00001;
    }
    Ok(json!(fee))
}

fn estimate_fee(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let target = param_u32(params, 0)?.clamp(1, 1008);
    {
        let cache = shared
            .fee_estimates
            .lock()
            .map_err(|_| RpcError::server("fee lock poisoned"))?;
        if let Some((value, at)) = cache.get(&target) {
            if at.elapsed() < Duration::from_secs(30) {
                return Ok(json!(value));
            }
        }
    }

    let mut value = None;
    if let Ok(estimate) = shared.core.estimate_smart_fee(target) {
        if let Some(rate) = estimate.feerate {
            if rate > 0.0 {
                value = Some(rate);
            }
        }
    }
    let value = value.unwrap_or_else(|| {
        // Never return -1: clients (Sparrow) treat it as a failure when sending.
        let mut fallback = *shared.relayfee.lock().unwrap_or_else(|e| e.into_inner());
        if fallback <= 0.0 {
            fallback = 0.00001;
        }
        if let Ok(info) = shared.core.mempool_info() {
            fallback = fallback.max(info.mempoolminfee);
        }
        fallback
    });
    shared
        .fee_estimates
        .lock()
        .map_err(|_| RpcError::server("fee lock poisoned"))?
        .insert(target, (value, Instant::now()));
    Ok(json!(value))
}

fn block_stats(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let height = param_u32(params, 0)? as u64;
    shared
        .core
        .block_stats(height)
        .map_err(|e| RpcError::server(e.to_string()))
}

fn broadcast_transaction(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let hex = params
        .as_array()
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .ok_or_else(|| RpcError::invalid("expected raw transaction hex"))?;
    let txid = shared
        .core
        .send_raw_transaction(hex)
        .map_err(|e| RpcError::server(e.to_string()))?;
    Ok(json!(txid))
}

fn silent_payments(shared: &Shared, method: &str) -> RpcResult<Value> {
    if !shared.silent_payments_enabled {
        return Err(RpcError::server(
            "silent payments are disabled by configuration ([silent_payments] enabled = false)",
        ));
    }
    // The opt-in gate exists so a future BIP352 indexer can be wired without
    // changing the protocol surface. Until then, fail explicitly.
    Err(RpcError::server(format!(
        "{method}: silent payments are enabled but this build has no BIP352 indexer (see docs/ADR-018)"
    )))
}

fn scripthash_mempool(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let script = param_scripthash(params)?;
    let mem = shared
        .mempool
        .read()
        .map_err(|_| RpcError::server("mempool lock poisoned"))?;
    let arr: Vec<Value> = mem
        .history(script)
        .into_iter()
        .map(|(txid, fee, height)| {
            json!({
                "tx_hash": txid.to_hex(),
                "height": height,
                "fee": fee,
            })
        })
        .collect();
    Ok(Value::Array(arr))
}

fn transaction_merkle(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let txid = params
        .as_array()
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .ok_or_else(|| RpcError::invalid("expected txid"))?;
    let height = param_u32(params, 1)?;
    let txid_hash = Hash32::from_hex(txid).map_err(RpcError::invalid)?;
    let rec = shared
        .store
        .get_block(height)
        .map_err(|e| RpcError::server(e.to_string()))?
        .ok_or_else(|| RpcError::server(format!("height {height} not indexed")))?;
    let txids = shared
        .store
        .block_txids(&rec)
        .map_err(|e| RpcError::server(e.to_string()))?;
    let pos = txids
        .iter()
        .position(|t| *t == txid_hash)
        .ok_or_else(|| RpcError::server("transaction not in that block"))?;
    let (_, branch) = merkle_branch(&txids, pos)
        .ok_or_else(|| RpcError::server("cannot compute merkle branch"))?;
    Ok(json!({
        "block_height": height,
        "merkle": branch.iter().map(|h| h.to_hex()).collect::<Vec<_>>(),
        "pos": pos,
    }))
}

fn transaction_id_from_pos(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let height = param_u32(params, 0)?;
    let pos = param_u32(params, 1)?;
    let want_merkle = params
        .as_array()
        .and_then(|a| a.get(2))
        .map(|v| v.as_bool().unwrap_or(false))
        .unwrap_or(false);
    let rec = shared
        .store
        .get_block(height)
        .map_err(|e| RpcError::server(e.to_string()))?
        .ok_or_else(|| RpcError::server(format!("height {height} not indexed")))?;
    let txids = shared
        .store
        .block_txids(&rec)
        .map_err(|e| RpcError::server(e.to_string()))?;
    let txid = txids
        .get(pos as usize)
        .ok_or_else(|| RpcError::server("position out of range"))?;
    if !want_merkle {
        return Ok(json!(txid.to_hex()));
    }
    let (_, branch) = merkle_branch(&txids, pos as usize)
        .ok_or_else(|| RpcError::server("cannot compute merkle branch"))?;
    Ok(json!({
        "tx_hash": txid.to_hex(),
        "merkle": branch.iter().map(|h| h.to_hex()).collect::<Vec<_>>(),
    }))
}

/// Poll Core and keep the mempool overlay fresh. Core remains the source of
/// truth; nothing is persisted.
pub fn start_mempool(shared: Arc<Shared>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut last_err = String::new();
        let mut last_fee = Instant::now()
            .checked_sub(Duration::from_secs(600))
            .unwrap_or_else(Instant::now);
        let mut last_tip = u64::MAX;
        let mut last_refresh = Instant::now()
            .checked_sub(Duration::from_secs(60))
            .unwrap_or_else(Instant::now);
        loop {
            // Reconcile immediately after a new block; otherwise poll every 3 s.
            let tip = shared
                .store
                .committed_tip()
                .ok()
                .flatten()
                .map(|t| t.height as u64)
                .unwrap_or(0);
            let tip_changed = tip != last_tip;
            if !tip_changed && last_refresh.elapsed() < Duration::from_secs(3) {
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
            let result = {
                let mut mem = match shared.mempool.write() {
                    Ok(m) => m,
                    Err(_) => break,
                };
                mem.refresh(&shared.core, &shared.store)
            };
            last_tip = tip;
            last_refresh = Instant::now();
            match result {
                Ok(_) => {
                    if !last_err.is_empty() {
                        eprintln!("[e-lite] mempool refresh recovered");
                        last_err.clear();
                    }
                }
                Err(e) => {
                    let msg = e.to_string();
                    if msg != last_err {
                        eprintln!("[e-lite] mempool refresh: {msg}");
                        last_err = msg;
                    }
                }
            }
            if last_fee.elapsed() >= Duration::from_secs(60) {
                if let Ok(info) = shared.core.network_info() {
                    if info.relayfee > 0.0 {
                        if let Ok(mut fee) = shared.relayfee.lock() {
                            *fee = info.relayfee;
                        }
                    }
                }
                last_fee = Instant::now();
            }
            std::thread::sleep(Duration::from_secs(3));
        }
    })
}

/// Push notifications for subscribed clients (headers and scripthash status).
/// Recomputed only when the tip or the mempool overlay changes.
pub fn start_notifications(shared: Arc<Shared>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut last_height = u64::MAX;
        let mut last_mempool = u64::MAX;
        loop {
            shared.stats.notifier_runs.fetch_add(1, Ordering::Relaxed);
            let tip = shared.store.committed_tip().ok().flatten();
            let (height, header) = match tip {
                Some(t) => (
                    t.height as u64,
                    shared
                        .store
                        .get_block(t.height)
                        .ok()
                        .flatten()
                        .map(|r| hex::encode(r.header)),
                ),
                None => (0, None),
            };
            let mem_version = shared.mempool.read().map(|m| m.version()).unwrap_or(0);
            let new_block = height != last_height;
            let mem_changed = mem_version != last_mempool;
            if new_block || mem_changed {
                for client in shared.clients.snapshot() {
                    if !client.alive.load(Ordering::SeqCst) {
                        continue;
                    }
                    if new_block && client.headers.load(Ordering::SeqCst) {
                        if let Some(h) = &header {
                            let notification = json!({
                                "jsonrpc": "2.0",
                                "method": "blockchain.headers.subscribe",
                                "params": [{"height": height, "hex": h}],
                            });
                            if client.send(&notification) {
                                shared
                                    .stats
                                    .notifications_sent
                                    .fetch_add(1, Ordering::Relaxed);
                                client.last_height.store(height, Ordering::SeqCst);
                            } else {
                                shared.stats.notifier_errors.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                    let subs: Vec<(ScriptHash, Option<String>)> = client
                        .subs
                        .lock()
                        .map(|s| s.iter().map(|(k, v)| (*k, v.clone())).collect())
                        .unwrap_or_default();
                    for (script, old) in subs {
                        let Ok(new) = script_status(&shared, script) else {
                            continue;
                        };
                        if new != old {
                            let notification = json!({
                                "jsonrpc": "2.0",
                                "method": "blockchain.scripthash.subscribe",
                                "params": [script.to_electrum_hex(), new],
                            });
                            if client.send(&notification) {
                                shared
                                    .stats
                                    .notifications_sent
                                    .fetch_add(1, Ordering::Relaxed);
                                if let Ok(mut subs) = client.subs.lock() {
                                    subs.insert(script, new);
                                }
                            } else {
                                shared.stats.notifier_errors.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                }
                last_height = height;
                last_mempool = mem_version;
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    })
}

// ------------------------------------------------------------------- transports

pub fn start_server(shared: Arc<Shared>) -> std::io::Result<std::thread::JoinHandle<()>> {
    let listener = TcpListener::bind(&shared.listen)?;
    let handle = std::thread::spawn(move || {
        for incoming in listener.incoming() {
            let stream = match incoming {
                Ok(s) => s,
                Err(_) => continue,
            };
            if shared.stats.clients.load(Ordering::SeqCst) >= shared.max_clients {
                let _ = stream.shutdown(std::net::Shutdown::Both);
                continue;
            }
            let shared = Arc::clone(&shared);
            let Ok(writer) = stream.try_clone() else {
                continue;
            };
            let client = shared.clients.register(writer);
            std::thread::spawn(move || {
                shared.stats.clients.fetch_add(1, Ordering::SeqCst);
                let _ = handle_client(&shared, stream, client);
                shared.stats.clients.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });
    Ok(handle)
}

fn handle_client(
    shared: &Shared,
    stream: TcpStream,
    client: Arc<ClientState>,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(600)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    let mut reader = BufReader::new(stream);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        if buf.len() > 1024 * 1024 {
            let _ = client.send(&json!({
                "jsonrpc": "2.0",
                "id": Value::Null,
                "error": {"code": -32600, "message": "request too large"},
            }));
            break;
        }
        let line = String::from_utf8_lossy(&buf);
        if line.trim().is_empty() {
            continue;
        }
        let response = process_line_for(shared, Some(&client), &line);
        if !client.send(&response) {
            break;
        }
    }
    shared.clients.unregister(client.id);
    Ok(())
}

pub fn process_line(shared: &Shared, line: &str) -> Value {
    process_line_for(shared, None, line)
}

pub fn process_line_for(shared: &Shared, client: Option<&Arc<ClientState>>, line: &str) -> Value {
    shared.stats.requests_total.fetch_add(1, Ordering::Relaxed);
    let request: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            shared.stats.errors_total.fetch_add(1, Ordering::Relaxed);
            return json!({
                "jsonrpc": "2.0",
                "id": Value::Null,
                "error": {"code": -32700, "message": format!("parse error: {e}")},
            });
        }
    };
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = match request.get("method").and_then(|m| m.as_str()) {
        Some(m) => m,
        None => {
            shared.stats.errors_total.fetch_add(1, Ordering::Relaxed);
            return json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32600, "message": "invalid request"},
            });
        }
    };
    let params = request.get("params").cloned().unwrap_or(json!([]));
    match dispatch_for(shared, client, method, &params) {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(e) => {
            shared.stats.errors_total.fetch_add(1, Ordering::Relaxed);
            log_rpc_error(shared, method, e.code);
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": e.code, "message": e.message},
            })
        }
    }
}

/// Local /status and /metrics endpoint. Loopback only by default.
pub fn start_metrics(
    shared: Arc<Shared>,
    addr: String,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let listener = TcpListener::bind(&addr)?;
    let handle = std::thread::spawn(move || {
        for incoming in listener.incoming() {
            let mut stream = match incoming {
                Ok(s) => s,
                Err(_) => continue,
            };
            let mut buf = [0u8; 2048];
            let n = stream.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let path = req
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("/");
            let (ctype, body) = match path {
                "/status" => ("application/json", shared.status_json().to_string()),
                "/healthz" => ("text/plain", "ok\n".to_string()),
                "/metrics" => ("text/plain; version=0.0.4", metrics_text(&shared)),
                _ => (
                    "text/plain",
                    "e-lite: try /status, /metrics or /healthz\n".to_string(),
                ),
            };
            let resp = format!(
                "HTTP/1.0 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes());
        }
    });
    Ok(handle)
}

fn metrics_text(shared: &Shared) -> String {
    let st = shared.status_json();
    let g = |k: &str| st[k].as_u64().unwrap_or(0);
    let mut out = String::new();
    let mut line = |name: &str, help: &str, kind: &str, value: u64| {
        out.push_str(&format!(
            "# HELP {name} {help}\n# TYPE {name} {kind}\n{name} {value}\n"
        ));
    };
    line(
        "e_lite_indexed_height",
        "Indexed tip height",
        "gauge",
        g("indexed_height"),
    );
    line(
        "e_lite_core_height",
        "Bitcoin Core tip height",
        "gauge",
        g("core_height"),
    );
    line(
        "e_lite_lag_blocks",
        "Blocks behind Core",
        "gauge",
        g("lag_blocks"),
    );
    line(
        "e_lite_txnum",
        "Indexed transaction occurrences",
        "counter",
        g("txnum"),
    );
    line(
        "e_lite_db_bytes",
        "Index size on disk",
        "gauge",
        g("db_bytes"),
    );
    line(
        "e_lite_disk_free_bytes",
        "Free disk on the data directory",
        "gauge",
        g("disk_free_bytes"),
    );
    line(
        "e_lite_server_clients",
        "Connected Electrum clients",
        "gauge",
        g("clients"),
    );
    line(
        "e_lite_server_requests_total",
        "Electrum requests served",
        "counter",
        g("requests_total"),
    );
    line(
        "e_lite_errors_total",
        "Protocol and internal errors",
        "counter",
        g("errors_total"),
    );
    line(
        "e_lite_last_block_ms",
        "Duration of the last applied block",
        "gauge",
        g("last_block_ms"),
    );
    line(
        "e_lite_uptime_seconds",
        "Process uptime",
        "gauge",
        g("uptime_seconds"),
    );
    let state = shared.get_state();
    out.push_str("# HELP e_lite_state Server state (0 blocking values)\n");
    out.push_str("# TYPE e_lite_state gauge\n");
    out.push_str(&format!(
        "e_lite_state{{state=\"{}\"}} {}\n",
        state::name(state),
        { state }
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_negotiation() {
        assert_eq!(
            server_version(&json!(["client", "1.4"])).unwrap(),
            json!([format!("e-lite {VERSION}"), "1.4"])
        );
        assert!(server_version(&json!(["client", ["1.4", "1.4"]])).is_ok());
        assert!(server_version(&json!(["client", "1.2"])).is_err());
        assert!(server_version(&json!(["client", ["1.0", "1.2"]])).is_err());
    }

    #[test]
    fn scripthash_param_is_reversed() {
        let sh = ScriptHash::from_script(b"\x51");
        let v = json!([sh.to_electrum_hex()]);
        assert_eq!(param_scripthash(&v).unwrap(), sh);
        assert!(param_scripthash(&json!(["zz"])).is_err());
    }
}
