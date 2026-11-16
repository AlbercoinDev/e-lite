//! rpc module (split from lib.rs in alpha.29.17.0).

use super::*;

#[derive(Debug)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    /// Set when the connection must be closed after sending the error.
    pub close: bool,
    /// The method name is not one this server knows. Its name is client
    /// text and never reaches the log.
    pub unknown_method: bool,
}

impl RpcError {
    pub(crate) fn new(code: i64, message: impl Into<String>) -> Self {
        RpcError {
            code,
            message: message.into(),
            close: false,
            unknown_method: false,
        }
    }
    pub(crate) fn invalid(msg: impl Into<String>) -> Self {
        Self::new(-32602, msg)
    }
    pub(crate) fn method(msg: impl Into<String>) -> Self {
        Self::new(-32601, msg)
    }
    /// A method name this server does not implement at all.
    pub(crate) fn unknown_method(name: &str) -> Self {
        let mut e = Self::method(format!("unknown method: {name}"));
        e.unknown_method = true;
        e
    }
    pub(crate) fn server(msg: impl Into<String>) -> Self {
        Self::new(-32000, msg)
    }
    /// Mark this error as fatal for the connection.
    pub(crate) fn closing(mut self) -> Self {
        self.close = true;
        self
    }
}

pub(crate) type RpcResult<T> = std::result::Result<T, RpcError>;

pub const METHOD_NOT_FOUND: i64 = -32601;

/// Most distinct `method:code` pairs remembered for the once-a-minute limit.
/// Known methods are a fixed set, so this is never reached in practice; it is
/// the bound that keeps the map small whatever a client sends.
pub(crate) const ERROR_LOG_MAX: usize = 256;

/// One INFO line per method and error code, at most once a minute. An unknown
/// method is logged without its name: the name is client text (unbounded
/// distinct values, and it may carry control characters into the log). A
/// known method keeps its name whatever the code (a 1.6 method called on a
/// 1.4 connection also answers -32601).
pub(crate) fn log_rpc_error(shared: &Shared, method: &str, error: &RpcError) {
    let code = error.code;
    let method = if error.unknown_method {
        "<unknown>"
    } else {
        method
    };
    let key = format!("{method}:{code}");
    {
        let Ok(mut seen) = shared.error_log.lock() else {
            return;
        };
        if let Some(at) = seen.get(&key) {
            if at.elapsed() < Duration::from_secs(60) {
                return;
            }
        }
        if seen.len() >= ERROR_LOG_MAX {
            seen.retain(|_, at| at.elapsed() < Duration::from_secs(60));
            if seen.len() >= ERROR_LOG_MAX {
                return;
            }
        }
        seen.insert(key, Instant::now());
    }
    eprintln!("[e-lite] rpc error: method={method} code={code}");
}

/// Count a failed `blockchain.transaction.get` by cause and say so in a
/// `debug:` line, at most one per cause every 5 s (the INFO `rpc error` line
/// is one per minute and has no cause). The line carries the cause, the
/// height and the running total: never a txid or a block hash (AUD-132).
pub(crate) fn note_tx_get_failure(shared: &Shared, cause: TxGetFailure, height: Option<u32>) {
    let total = shared.stats.tx_get_failures.add(cause);
    if !debug_logs() {
        return;
    }
    let key = format!("tx.get:{}", cause.label());
    let Ok(mut seen) = shared.error_log.lock() else {
        return;
    };
    if seen
        .get(&key)
        .is_some_and(|at| at.elapsed() < Duration::from_secs(5))
    {
        return;
    }
    seen.insert(key, Instant::now());
    drop(seen);
    let at = height.map_or_else(|| "unconfirmed".to_string(), |h| format!("height {h}"));
    eprintln!(
        "[e-lite] debug: tx.get failed: {} ({at}, {total} so far)",
        cause.label()
    );
}

pub(crate) fn rawtx_failure(shared: &Shared, height: u32, e: el_rawtx::RawtxError) -> RpcError {
    note_tx_get_failure(shared, TxGetFailure::of(&e), Some(height));
    RpcError::server(e.to_string())
}

pub fn dispatch(shared: &Shared, method: &str, params: &Value) -> RpcResult<Value> {
    dispatch_for(shared, None, SERVER_MIN, method, params)
}

pub fn dispatch_for(
    shared: &Shared,
    client: Option<&Arc<ClientState>>,
    version: ProtocolVersion,
    method: &str,
    params: &Value,
) -> RpcResult<Value> {
    match method {
        "server.version" => server_version(params, client),
        "server.ping" => server_ping(params, version, client),
        "server.banner" => Ok(json!(banner_text(shared))),
        "server.donation_address" => Ok(json!("")),
        "server.peers.subscribe" => Ok(json!([])),
        "server.add_peer" => Ok(json!(false)),
        "server.features" => server_features(shared, version),
        "mempool.get_info" => mempool_info(shared),
        "mempool.recent" => mempool_recent(shared),
        _ => {
            // Everything else reads the index: enforce a consistent tip and a
            // non-misleading state. The read lock is taken per method and is
            // *never* held across Core or external network I/O.
            check_ready(shared)?;
            // The per-txid path may only send txids of heights Core has
            // pruned (AUD-167). One relaxed store per request.
            shared
                .resolver
                .set_prune_height(shared.stats.core_pruneheight.load(Ordering::Relaxed) as u32);
            match method {
                "blockchain.headers.subscribe" => {
                    let _g = read_guard(shared)?;
                    headers_subscribe(shared, client)
                }
                // The checkpoint proof hashes every header up to cp_height.
                // The guard is taken inside, only around the store reads, so
                // a remote proof request cannot stall commits (AUD-167).
                "blockchain.block.header" => block_header(shared, params),
                "blockchain.block.headers" => block_headers(shared, params, version),
                "blockchain.scripthash.get_history" => {
                    let _g = read_guard(shared)?;
                    scripthash_history(shared, params)
                }
                "blockchain.scripthash.get_balance" => {
                    let _g = read_guard(shared)?;
                    scripthash_balance(shared, params)
                }
                "blockchain.scripthash.listunspent" => {
                    let _g = read_guard(shared)?;
                    scripthash_listunspent(shared, params)
                }
                "blockchain.scripthash.subscribe" => {
                    let _g = read_guard(shared)?;
                    scripthash_subscribe(shared, client, params)
                }
                "blockchain.scripthash.unsubscribe" => {
                    let script = param_scripthash(params)?;
                    let removed = client
                        .and_then(|c| {
                            c.subs
                                .lock()
                                .ok()
                                .map(|mut subs| subs.remove(&script).is_some())
                        })
                        .unwrap_or(false);
                    Ok(json!(removed))
                }
                "blockchain.scripthash.get_mempool" => scripthash_mempool(shared, params),
                "blockchain.scriptpubkey.get_history" => {
                    let _g = read_guard(shared)?;
                    scriptpubkey_history(shared, params)
                }
                "blockchain.scriptpubkey.get_balance" => {
                    let _g = read_guard(shared)?;
                    scriptpubkey_balance(shared, params)
                }
                "blockchain.scriptpubkey.listunspent" => {
                    let _g = read_guard(shared)?;
                    scriptpubkey_listunspent(shared, params)
                }
                "blockchain.scriptpubkey.get_mempool" => scriptpubkey_mempool(shared, params),
                "blockchain.scriptpubkey.subscribe" => {
                    let _g = read_guard(shared)?;
                    scriptpubkey_subscribe(shared, client, params)
                }
                "blockchain.scriptpubkey.unsubscribe" => scriptpubkey_unsubscribe(client, params),
                "blockchain.outpoint.get_status" => outpoint_get_status(shared, params),
                "blockchain.outpoint.subscribe" => outpoint_subscribe(shared, client, params),
                "blockchain.outpoint.unsubscribe" => outpoint_unsubscribe(client, params),
                "mempool.get_fee_histogram" => fee_histogram(shared),
                "blockchain.relayfee" => relay_fee(shared),
                "blockchain.estimatefee" => estimate_fee(shared, params),
                "blockchain.block.stats" => block_stats(shared, params),
                "blockchain.transaction.broadcast" => broadcast_transaction(shared, params),
                "blockchain.transaction.broadcast_package" => {
                    broadcast_package(shared, params, version)
                }
                "blockchain.transaction.testmempoolaccept" => test_mempool_accept(shared, params),
                "blockchain.silentpayments.subscribe" | "blockchain.silentpayments.unsubscribe" => {
                    silent_payments(shared, method)
                }
                "blockchain.transaction.get" => transaction_get(shared, params),
                "blockchain.transaction.get_merkle" => {
                    let _g = read_guard(shared)?;
                    transaction_merkle(shared, params)
                }
                "blockchain.transaction.id_from_pos" => {
                    let _g = read_guard(shared)?;
                    transaction_id_from_pos(shared, params)
                }
                _ => Err(RpcError::unknown_method(method)),
            }
        }
    }
}

pub(crate) fn read_guard(shared: &Shared) -> RpcResult<RwLockReadGuard<'_, ()>> {
    shared
        .lock
        .read()
        .map_err(|_| RpcError::server("lock poisoned"))
}

pub(crate) fn check_ready(shared: &Shared) -> RpcResult<()> {
    let has_tip = shared.store.committed_tip().ok().flatten().is_some();
    match shared.get_state() {
        state::READY => Ok(()),
        state::DEGRADED | state::CORE_UNAVAILABLE | state::PAUSE_DOWNLOAD | state::PRUNING
            if has_tip =>
        {
            Ok(())
        }
        state::DEGRADED | state::CORE_UNAVAILABLE | state::PAUSE_DOWNLOAD | state::PRUNING => Err(
            RpcError::server("the index has no data yet; waiting for Bitcoin Core"),
        ),
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
        state::WAITING_FOR_CORE => Err(RpcError::server(
            "waiting for Bitcoin Core; the index is not initialized yet",
        )),
        state::INITIALIZING => Err(RpcError::server(
            "initializing the index against Bitcoin Core; retry later",
        )),
        state::REORG => Err(RpcError::server("reorg recovery in progress")),
        state::FATAL => Err(RpcError::server(
            "fatal integrity error; operator action required",
        )),
        _ => Err(RpcError::server("server not ready")),
    }
}
