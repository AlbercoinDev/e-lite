//! protocol module (split from lib.rs in alpha.29.17.0).

use super::*;

pub(crate) fn server_version(
    params: &Value,
    client: Option<&Arc<ClientState>>,
) -> RpcResult<Value> {
    let arr = params
        .as_array()
        .ok_or_else(|| RpcError::invalid("server.version expects [client, protocol]"))?;
    // 1.6 mandates that this is the first message. If the connection already
    // sent something else, negotiate only up to 1.4.2 instead of disconnecting.
    let allow_1_6 = client
        .map(|c| !c.saw_message.load(Ordering::SeqCst))
        .unwrap_or(true);
    let negotiated = negotiate_protocol(arr.get(1), allow_1_6)?;
    if let Some(c) = client {
        let mut slot = c
            .protocol
            .lock()
            .map_err(|_| RpcError::server("client lock poisoned"))?;
        if slot.is_some() {
            return Err(RpcError::new(
                -32600,
                "server.version already negotiated on this connection",
            ));
        }
        *slot = Some(negotiated);
    }
    if debug_logs() {
        // Which wallet and protocol this connection speaks. The name is the
        // client's own string (`Sparrow 2.3.0`); it carries no wallet data and
        // is sanitised so it cannot forge or break a log line.
        let name = arr.first().and_then(Value::as_str).unwrap_or("-");
        let shown: String = name
            .chars()
            .take(40)
            .map(|c| {
                if c.is_ascii_graphic() || c == ' ' {
                    c
                } else {
                    '?'
                }
            })
            .collect();
        eprintln!(
            "[e-lite] debug: wallet: client {shown:?} protocol {negotiated} (banner {})",
            server_banner()
        );
    }
    Ok(json!([server_banner(), negotiated.to_string()]))
}

/// Resolve the requested client range against the versions this build serves.
/// A missing request means the protocol default, 1.4.
pub(crate) fn negotiate_protocol(
    requested: Option<&Value>,
    allow_1_6: bool,
) -> RpcResult<ProtocolVersion> {
    let (client_min, client_max) = match requested {
        None => (SERVER_MIN, SERVER_MIN),
        Some(Value::String(value)) => {
            let version = ProtocolVersion::parse(value).map_err(RpcError::invalid)?;
            (version, version)
        }
        Some(Value::Array(range)) => {
            let min = range
                .first()
                .and_then(|v| v.as_str())
                .ok_or_else(|| RpcError::invalid("protocol range expects [min, max]"))?;
            let min = ProtocolVersion::parse(min).map_err(RpcError::invalid)?;
            let max = match range.get(1) {
                Some(Value::String(value)) => {
                    ProtocolVersion::parse(value).map_err(RpcError::invalid)?
                }
                None => min,
                Some(_) => return Err(RpcError::invalid("protocol range expects string versions")),
            };
            (min, max)
        }
        Some(_) => {
            return Err(RpcError::invalid(
                "protocol must be a version string or [min, max]",
            ))
        }
    };
    if client_max < client_min {
        return Err(RpcError::invalid("protocol range is inverted"));
    }
    // Deliberately lenient: a client asking for a future or skipped version
    // (1.5, 1.7) is served the newest version we actually implement below it.
    // We only refuse clients that cannot speak 1.4 at all.  The negotiated
    // version is returned, so a compliant client knows which shape to expect.
    let available = if allow_1_6 {
        SERVER_VERSIONS
    } else {
        PRE_1_6_VERSIONS
    };
    ProtocolVersion::highest_available(client_max, available).ok_or_else(|| {
        RpcError::server(format!(
            "no supported protocol at or below {client_max}; server supports \
             {PROTOCOL_MIN}-{PROTOCOL_MAX}"
        ))
        .closing()
    })
}

pub const DEFAULT_BANNER_TEMPLATE: &str = r#"# e-lite wallet banner template
# Placeholders: {version} {network} {state} {height} {headers} {percent} {bar}
# {core_height} {core_percent} {lag} {blocks_per_sec} {tx_per_sec} {eta}
# {prune} {pruneheight} {listen} {tls} {log_file} {uptime} {ready} {ready_note}
# Sections: [indexing] while the index catches up, [ready] when usable.

[indexing]
e-lite {version} · {network} · {state}
{bar} {percent}
height {height} / {headers} · Core {core_height} ({core_percent})
lag {lag} · {blocks_per_sec} blk/s · ETA {eta}
Electrum {listen}{tls}
Some wallet queries may be unavailable until caught up.

[ready]
==============================================
   e-lite  ·  {state}  ·  {network}
   height {height} / {headers}  ({percent})
   Electrum {listen}{tls}
   {ready_note}
==============================================
"#;

pub(crate) const BANNER_MAX_BYTES: u64 = 32 * 1024;

/// Create the configured banner file with the default template if it does not
/// exist yet. A relative path is resolved by the caller.
pub fn ensure_banner_file(path: &Path) -> std::io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(path, DEFAULT_BANNER_TEMPLATE)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644));
    }
    Ok(())
}

pub(crate) fn banner_text(shared: &Shared) -> String {
    let status = shared.sync_status();
    let (default_indexing, default_ready) = split_banner(DEFAULT_BANNER_TEMPLATE);
    let (custom_indexing, custom_ready) = shared
        .banner_file
        .as_ref()
        .and_then(|path| read_banner(path))
        .unwrap_or((None, None));
    let template = if status.ready {
        custom_ready.or(default_ready).unwrap_or_default()
    } else {
        custom_indexing.or(default_indexing).unwrap_or_default()
    };
    render_banner(&template, shared, &status)
}

pub(crate) fn read_banner(path: &Path) -> Option<(Option<String>, Option<String>)> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > BANNER_MAX_BYTES {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    Some(split_banner(&text))
}

/// Split a template into `[indexing]` and `[ready]` sections. A file without
/// section headers is used for both states.
pub(crate) fn split_banner(text: &str) -> (Option<String>, Option<String>) {
    let mut indexing = String::new();
    let mut ready = String::new();
    let mut current: Option<u8> = None;
    let mut saw_section = false;
    for line in text.lines() {
        match line.trim() {
            "[indexing]" => {
                current = Some(0);
                saw_section = true;
            }
            "[ready]" => {
                current = Some(1);
                saw_section = true;
            }
            _ => match current {
                Some(0) => {
                    indexing.push_str(line);
                    indexing.push('\n');
                }
                Some(1) => {
                    ready.push_str(line);
                    ready.push('\n');
                }
                _ => {}
            },
        }
    }
    if !saw_section {
        return (Some(text.to_string()), Some(text.to_string()));
    }
    let out = |s: String| if s.trim().is_empty() { None } else { Some(s) };
    (out(indexing), out(ready))
}

pub(crate) fn render_banner(template: &str, shared: &Shared, status: &SyncStatus) -> String {
    let state = if status.fully_synced {
        "FULLY SYNCED"
    } else if status.ready && status.offline {
        "READY (offline)"
    } else if status.ready && status.core_ibd {
        "READY (Core verifying)"
    } else if status.ready {
        "READY"
    } else {
        "INDEXING"
    };
    let ready_note = if status.fully_synced {
        "Fully synced. Wallet ready to use."
    } else if status.offline {
        "Offline cached index; Core is unavailable."
    } else if status.core_ibd {
        "Ready for the indexed height; Core is still verifying."
    } else {
        "Wallet ready to use."
    };
    let tls = shared
        .tls_listen
        .as_ref()
        .map(|addr| format!(" · SSL {addr}"))
        .unwrap_or_default();
    let bar = progress_bar(status.percent);
    let eta = eta_text(status.lag, status.blocks_per_sec);
    let network = shared.identity().chain.name().to_string();
    let tx_per_sec = shared.stats.tuning_tx_per_sec.load(Ordering::Relaxed);
    template
        .replace("{version}", VERSION)
        .replace("{network}", &network)
        .replace("{state}", state)
        .replace("{height}", &format_num(status.height))
        .replace("{headers}", &format_num(status.headers))
        .replace("{percent}", &format!("{:.1}%", status.percent))
        .replace("{bar}", &bar)
        .replace("{core_height}", &format_num(status.core_height))
        .replace("{core_percent}", &format!("{:.1}%", status.core_percent))
        .replace("{lag}", &format_num(status.lag))
        .replace("{blocks_per_sec}", &status.blocks_per_sec.to_string())
        .replace("{tx_per_sec}", &tx_per_sec.to_string())
        .replace("{eta}", &eta)
        .replace("{pruneheight}", &format_num(status.pruneheight))
        .replace("{prune}", &format_num(status.pruneheight))
        .replace("{listen}", &shared.listen)
        .replace("{tls}", &tls)
        .replace("{log_file}", &shared.log_file.display().to_string())
        .replace("{uptime}", &uptime_text(shared.stats.started.elapsed()))
        .replace("{ready}", if status.ready { "yes" } else { "no" })
        .replace("{ready_note}", ready_note)
}

pub(crate) fn progress_bar(percent: f64) -> String {
    const WIDTH: usize = 30;
    let filled = ((percent.clamp(0.0, 100.0) / 100.0) * WIDTH as f64).round() as usize;
    format!("[{}{}]", "#".repeat(filled), "-".repeat(WIDTH - filled))
}

pub(crate) fn format_num(value: u64) -> String {
    let s = value.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub(crate) fn eta_text(lag: u64, blocks_per_sec: u64) -> String {
    if lag == 0 {
        return "0m".to_string();
    }
    if blocks_per_sec == 0 {
        return "unknown".to_string();
    }
    let secs = lag / blocks_per_sec.max(1);
    if secs >= 3600 {
        format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

pub(crate) fn uptime_text(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    format!(
        "{}h {:02}m {:02}s",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

pub(crate) fn server_ping(
    params: &Value,
    version: ProtocolVersion,
    client: Option<&Arc<ClientState>>,
) -> RpcResult<Value> {
    if version < V1_7 {
        return Ok(Value::Null);
    }
    // The 1.7 signature is (pong_len, data). `data` is ignored by the caller
    // and we return the requested number of hex characters.
    let pong_len = params
        .as_array()
        .and_then(|a| a.first())
        .and_then(Value::as_u64)
        .unwrap_or(0);
    const MAX_PONG_HEX: u64 = 1024 * 1024;
    if pong_len > MAX_PONG_HEX {
        return Err(RpcError::invalid(format!(
            "pong_len must be <= {MAX_PONG_HEX} hex characters"
        )));
    }
    if let Some(c) = client {
        // Per-connection budget: neither a batch nor a loop can make the
        // server hold more than MAX_PONG_BYTES for one client (AUD-167).
        let left = c.pong_budget.fetch_sub(pong_len, Ordering::Relaxed);
        if left < pong_len {
            c.pong_budget.fetch_add(pong_len, Ordering::Relaxed);
            return Err(RpcError::invalid(
                "pong_len exceeds this connection's remaining pong budget",
            ));
        }
    }
    Ok(json!({ "data": "0".repeat(pong_len as usize) }))
}

pub(crate) fn server_features(shared: &Shared, version: ProtocolVersion) -> RpcResult<Value> {
    if matches!(
        shared.get_state(),
        state::WAITING_FOR_CORE | state::INITIALIZING
    ) {
        return Err(RpcError::server(
            "waiting for Bitcoin Core; the network identity is not initialized yet",
        ));
    }
    let id = shared.identity();
    let mut features = json!({
        "genesis_hash": id.genesis.to_hex(),
        "hash_function": "sha256",
        "server_version": server_banner(),
        "protocol_min": PROTOCOL_MIN,
        "protocol_max": PROTOCOL_MAX,
        "pruning": Value::Null,
        "hosts": {},
    });
    if version >= V1_7 {
        if let Some(obj) = features.as_object_mut() {
            obj.remove("hash_function");
        }
    }
    Ok(features)
}

pub(crate) fn tip_block(shared: &Shared) -> RpcResult<el_model::BlockRecord> {
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

pub(crate) fn headers_subscribe(
    shared: &Shared,
    client: Option<&Arc<ClientState>>,
) -> RpcResult<Value> {
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

pub(crate) fn param_u32(params: &Value, idx: usize) -> RpcResult<u32> {
    let value = params
        .as_array()
        .and_then(|a| a.get(idx))
        .and_then(|v| v.as_u64())
        .ok_or_else(|| RpcError::invalid("expected an integer parameter"))?;
    u32::try_from(value).map_err(|_| RpcError::invalid("integer parameter out of range"))
}

pub(crate) fn header_chain(shared: &Shared, cp_height: u32) -> RpcResult<Vec<Hash32>> {
    shared
        .headers
        .chain(&shared.store, cp_height)
        .map_err(RpcError::server)
}

pub(crate) fn block_header(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let height = param_u32(params, 0)?;
    let cp_height = params
        .as_array()
        .and_then(|a| a.get(1))
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
        .try_into()
        .map_err(|_| RpcError::invalid("cp_height out of range"))?;
    // Everything that needs a consistent tip happens under the read guard;
    // the merkle branch (millions of hashes) is computed after releasing it.
    let (header, hashes) = {
        let _g = read_guard(shared)?;
        let rec = shared
            .store
            .get_block(height)
            .map_err(|e| RpcError::server(e.to_string()))?
            .ok_or_else(|| RpcError::server(format!("height {height} not indexed")))?;
        let hashes = if cp_height == 0 {
            None
        } else {
            Some(header_chain(shared, cp_height)?)
        };
        (hex::encode(rec.header), hashes)
    };
    let Some(hashes) = hashes else {
        return Ok(json!(header));
    };
    let (root, branch) = merkle_branch(&hashes, height as usize)
        .ok_or_else(|| RpcError::server("cannot compute checkpoint branch"))?;
    Ok(json!({
        "header": header,
        "branch": branch.iter().map(|h| h.to_hex()).collect::<Vec<_>>(),
        "root": root.to_hex(),
    }))
}

pub(crate) fn block_headers(
    shared: &Shared,
    params: &Value,
    version: ProtocolVersion,
) -> RpcResult<Value> {
    let start = param_u32(params, 0)?;
    let count = param_u32(params, 1)?.min(MAX_HEADERS_CHUNK as u32);
    let cp_height = params
        .as_array()
        .and_then(|a| a.get(2))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cp_height =
        u32::try_from(cp_height).map_err(|_| RpcError::invalid("cp_height out of range"))?;
    let (headers, got, hashes) = {
        let _g = read_guard(shared)?;
        let mut headers = Vec::new();
        let mut got = 0u32;
        for h in start..start.saturating_add(count) {
            match shared
                .store
                .get_block(h)
                .map_err(|e| RpcError::server(e.to_string()))?
            {
                Some(rec) => {
                    headers.push(hex::encode(rec.header));
                    got += 1;
                }
                None => break,
            }
        }
        let hashes = if cp_height != 0 && got > 0 {
            Some(header_chain(shared, cp_height)?)
        } else {
            None
        };
        (headers, got, hashes)
    };
    // 1.6 changed the response from one concatenated `hex` string to a list.
    let mut result = if version >= V1_6 {
        json!({
            "count": got,
            "headers": headers,
            "max": MAX_HEADERS_CHUNK,
        })
    } else {
        json!({
            "count": got,
            "hex": headers.concat(),
            "max": MAX_HEADERS_CHUNK,
        })
    };
    if let Some(hashes) = hashes {
        let last = start + got - 1;
        let (root, branch) = merkle_branch(&hashes, last as usize)
            .ok_or_else(|| RpcError::server("cannot compute checkpoint branch"))?;
        result["root"] = json!(root.to_hex());
        result["branch"] = json!(branch.iter().map(|h| h.to_hex()).collect::<Vec<_>>());
    }
    Ok(result)
}

pub(crate) fn param_scripthash(params: &Value) -> RpcResult<ScriptHash> {
    let s = params
        .as_array()
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .ok_or_else(|| RpcError::invalid("expected scripthash hex"))?;
    ScriptHash::from_electrum_hex(s).map_err(|e| RpcError::invalid(e.to_string()))
}

pub(crate) type HistoryRow = (u32, u32, String, u64, u64);

pub(crate) fn history_entries_with_code(
    shared: &Shared,
    script: ScriptHash,
    too_large_code: i64,
    prefetch: bool,
) -> RpcResult<Vec<HistoryRow>> {
    let entries = shared
        .store
        .history_for_script(script)
        .map_err(|e| RpcError::server(e.to_string()))?;
    if entries.len() > MAX_HISTORY_ENTRIES {
        return Err(RpcError::new(
            too_large_code,
            format!(
                "history for this script has {} entries (limit {}); use a script with fewer transactions",
                entries.len(),
                MAX_HISTORY_ENTRIES
            ),
        ));
    }
    let mut blocks: HashMap<u32, el_model::BlockRecord> = HashMap::new();
    let mut out = Vec::with_capacity(entries.len());
    // Blocks in history order with their transactions, for the prefetcher.
    let mut prefetch_blocks: Vec<(Hash32, u32, Vec<Hash32>)> = Vec::new();
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
        if prefetch {
            match prefetch_blocks.last_mut() {
                Some((_, h, txids)) if *h == height => txids.push(txid),
                _ => prefetch_blocks.push((rec.hash, height, vec![txid])),
            }
        }
        out.push((
            height,
            tx_pos,
            txid.to_hex(),
            effect.received_sat,
            effect.spent_sat,
        ));
    }
    if prefetch && shared.resolver.tx_mode_servers().is_some() {
        // Per-txid mode (AUD-163): every pruned transaction of the history,
        // fetched now with the connection's decoy wallets. Heights Core
        // still has are served locally and never leave the node.
        let pruned_below = shared.stats.core_pruneheight.load(Ordering::Relaxed);
        let refs: Vec<el_rawtx::TxRef> = out
            .iter()
            .filter(|(height, ..)| (*height as u64) < pruned_below)
            .filter_map(|(height, tx_pos, txid, ..)| {
                Hash32::from_hex(txid).ok().map(|txid| el_rawtx::TxRef {
                    txid,
                    height: *height,
                    pos: *tx_pos,
                })
            })
            .collect();
        if !refs.is_empty() {
            shared
                .resolver
                .want_history(current_client(), script, &refs);
        }
        return Ok(out);
    }
    let mut prefetched = 0usize;
    for (hash, height, txids) in prefetch_blocks {
        if prefetched >= PREFETCH_PER_HISTORY {
            break;
        }
        if shared.prefetch.enqueue(hash, height, txids) {
            prefetched += 1;
        }
    }
    Ok(out)
}

pub(crate) fn history_value(
    shared: &Shared,
    script: ScriptHash,
    too_large_code: i64,
) -> RpcResult<Value> {
    let entries = history_entries_with_code(shared, script, too_large_code, true)?;
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

pub(crate) fn balance_value(shared: &Shared, script: ScriptHash) -> RpcResult<Value> {
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

pub(crate) fn listunspent_value(shared: &Shared, script: ScriptHash) -> RpcResult<Value> {
    let utxos = shared
        .store
        .utxos_for_script(script)
        .map_err(|e| RpcError::server(e.to_string()))?;
    let mem = shared
        .mempool
        .read()
        .map_err(|_| RpcError::server("mempool lock poisoned"))?;
    // Electrum expects confirmed UTXOs in blockchain order: height, then the
    // transaction's position in its block (AUD-146; it used to be txid
    // order), then vout. The store is keyed by (script, outpoint), so the
    // position is looked up only where it matters: heights holding several
    // UTXOs of this script.
    let mut per_height: HashMap<u32, usize> = HashMap::new();
    for (op, _, height, _) in &utxos {
        if !mem.is_spent(op) {
            *per_height.entry(*height).or_default() += 1;
        }
    }
    let mut rows: Vec<(u32, u32, String, u32, u64)> = Vec::with_capacity(utxos.len());
    for (op, value, height, _coinbase) in utxos {
        if mem.is_spent(&op) {
            continue;
        }
        let block_pos = if per_height.get(&height).copied().unwrap_or(0) > 1 {
            shared
                .store
                .occurrences(op.txid)
                .map_err(|e| RpcError::server(e.to_string()))?
                .into_iter()
                .find(|(h, _)| *h == height)
                .map(|(_, pos)| pos)
                .unwrap_or(u32::MAX)
        } else {
            0
        };
        rows.push((height, block_pos, op.txid.to_hex(), op.vout, value));
    }
    rows.sort_by(|a, b| (a.0, a.1, &a.2, a.3).cmp(&(b.0, b.1, &b.2, b.3)));
    let mut arr: Vec<Value> = rows
        .into_iter()
        .map(|(height, _, tx_hash, tx_pos, value)| {
            json!({
                "tx_hash": tx_hash,
                "tx_pos": tx_pos,
                "height": height,
                "value": value,
            })
        })
        .collect();
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

pub(crate) fn status_value(
    shared: &Shared,
    script: ScriptHash,
    too_large_code: i64,
    prefetch: bool,
) -> RpcResult<Option<String>> {
    let entries = history_entries_with_code(shared, script, too_large_code, prefetch)?;
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

pub(crate) fn script_status(shared: &Shared, script: ScriptHash) -> RpcResult<Option<String>> {
    status_value(shared, script, -32000, false)
}

pub(crate) fn mempool_value(shared: &Shared, script: ScriptHash) -> RpcResult<Value> {
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

pub(crate) fn scripthash_history(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let script = param_scripthash(params)?;
    // Never log the script hash: it identifies a wallet address (AUD-132).
    history_value(shared, script, -32000).inspect_err(|e| {
        eprintln!(
            "[e-lite] get_history failed: {}",
            el_rawtx::redact_hashes(&e.message)
        );
    })
}

pub(crate) fn scripthash_balance(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let script = param_scripthash(params)?;
    balance_value(shared, script)
}

pub(crate) fn scripthash_listunspent(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let script = param_scripthash(params)?;
    listunspent_value(shared, script)
}

pub(crate) fn scripthash_subscribe(
    shared: &Shared,
    client: Option<&Arc<ClientState>>,
    params: &Value,
) -> RpcResult<Value> {
    let script = param_scripthash(params)?;
    // A subscribe is the wallet's first look at an address: start fetching
    // its transactions now (AUD-163), one round trip before get_history.
    let status = status_value(shared, script, -32000, true)?;
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

pub(crate) fn param_scriptpubkey(params: &Value) -> RpcResult<ScriptHash> {
    let s = params
        .as_array()
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .ok_or_else(|| RpcError::invalid("expected scriptPubKey hex"))?;
    let bytes =
        hex::decode(s).map_err(|e| RpcError::invalid(format!("invalid scriptPubKey: {e}")))?;
    Ok(ScriptHash::from_script(&bytes))
}

pub(crate) fn scriptpubkey_history(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let script = param_scriptpubkey(params)?;
    Ok(json!({ "history": history_value(shared, script, 10001)? }))
}

pub(crate) fn scriptpubkey_balance(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let script = param_scriptpubkey(params)?;
    balance_value(shared, script)
}

pub(crate) fn scriptpubkey_listunspent(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let script = param_scriptpubkey(params)?;
    Ok(json!({ "utxos": listunspent_value(shared, script)? }))
}

pub(crate) fn scriptpubkey_mempool(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let script = param_scriptpubkey(params)?;
    Ok(json!({ "history": mempool_value(shared, script)? }))
}

pub(crate) fn scriptpubkey_subscribe(
    shared: &Shared,
    client: Option<&Arc<ClientState>>,
    params: &Value,
) -> RpcResult<Value> {
    let script = param_scriptpubkey(params)?;
    let status = status_value(shared, script, 10001, true)?;
    if let Some(c) = client {
        if let Ok(mut spk) = c.spk_subs.lock() {
            spk.insert(script, status.clone());
        }
    }
    match status {
        Some(status) => Ok(json!(status)),
        None => Ok(Value::Null),
    }
}

pub(crate) fn scriptpubkey_unsubscribe(
    client: Option<&Arc<ClientState>>,
    params: &Value,
) -> RpcResult<Value> {
    let script = param_scriptpubkey(params)?;
    let removed = client
        .and_then(|c| {
            c.spk_subs
                .lock()
                .ok()
                .map(|mut subs| subs.remove(&script).is_some())
        })
        .unwrap_or(false);
    Ok(json!(removed))
}
pub(crate) fn param_outpoint(params: &Value) -> RpcResult<(Hash32, u32, ScriptHash)> {
    let arr = params
        .as_array()
        .ok_or_else(|| RpcError::invalid("expected [tx_hash, txout_idx, spk_hint]"))?;
    let txid = arr
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid("expected tx_hash hex"))?;
    let txid = Hash32::from_hex(txid).map_err(RpcError::invalid)?;
    let vout = u32::try_from(
        arr.get(1)
            .and_then(Value::as_u64)
            .ok_or_else(|| RpcError::invalid("expected txout_idx"))?,
    )
    .map_err(|_| RpcError::invalid("txout_idx out of range"))?;
    let script = param_scriptpubkey(&json!([arr.get(2).cloned().unwrap_or(Value::Null)]))?;
    Ok((txid, vout, script))
}

pub(crate) fn decode_raw_tx(raw: &str) -> RpcResult<bitcoin::Transaction> {
    let bytes = hex::decode(raw).map_err(|e| RpcError::server(format!("rawtx hex: {e}")))?;
    bitcoin::consensus::encode::deserialize::<bitcoin::Transaction>(&bytes)
        .map_err(|e| RpcError::server(format!("rawtx decode: {e}")))
}

/// Only blocks Core can still serve need a reader guard. A block below
/// Core's prune height is resolved from cache/P2P/Esplora; holding a guard for
/// it would block pruning without any benefit.
pub(crate) fn rawtx_reader_guard(shared: &Shared, height: u32) -> Option<el_chain::ReaderGuard> {
    let pruneheight = shared.stats.core_pruneheight.load(Ordering::Relaxed);
    if (height as u64) < pruneheight {
        None
    } else {
        Some(shared.readers.enter(height))
    }
}

pub(crate) fn resolve_raw(shared: &Shared, txid: Hash32, height: u32) -> RpcResult<String> {
    let _guard = rawtx_reader_guard(shared, height);
    shared
        .resolver
        .raw_tx(txid, height)
        .map_err(|e| RpcError::server(e.to_string()))
}

/// Resolve the Electrum 1.7 outpoint status without a persistent spend index.
/// The fast path is a live UTXO (no raw-tx fetch); spent outputs are resolved
/// from the script history, and only the candidate raw transactions are read.
pub(crate) fn outpoint_status(
    shared: &Shared,
    txid: Hash32,
    vout: u32,
    script: ScriptHash,
) -> RpcResult<Value> {
    let op = Outpoint { txid, vout };

    // Funder in the mempool: outputs are already in memory and verify the hint.
    {
        let mem = shared
            .mempool
            .read()
            .map_err(|_| RpcError::server("mempool lock poisoned"))?;
        if let Some(tx) = mem.tx(&txid) {
            let Some((_, out_script, _)) = tx.outputs.iter().find(|(op, _, _)| op.vout == vout)
            else {
                return Ok(json!({}));
            };
            if *out_script != script {
                return Ok(json!({}));
            }
            let mut status = json!({ "funder_height": tx.height });
            if let Some((spender, height)) = mem.spender(&op) {
                status["spender_txhash"] = json!(spender.to_hex());
                status["spender_height"] = json!(height);
            }
            return Ok(status);
        }
    }

    // Confirmed funder height.
    let funder_height = {
        let _g = read_guard(shared)?;
        let occurrences = shared
            .store
            .occurrences(txid)
            .map_err(|e| RpcError::server(e.to_string()))?;
        let Some((height, _)) = occurrences.first().copied() else {
            return Ok(json!({}));
        };
        height
    };

    // Mempool spender of a confirmed output.
    if let Some((spender, height)) = shared
        .mempool
        .read()
        .map_err(|_| RpcError::server("mempool lock poisoned"))?
        .spender(&op)
    {
        return Ok(json!({
            "funder_height": funder_height,
            "spender_txhash": spender.to_hex(),
            "spender_height": height,
        }));
    }

    // Unspent fast path: the UTXO also carries the authoritative script.
    let utxo = {
        let _g = read_guard(shared)?;
        shared
            .store
            .get_utxo(&op)
            .map_err(|e| RpcError::server(e.to_string()))?
    };
    if let Some(utxo) = utxo {
        if utxo.script_hash != script {
            return Ok(json!({}));
        }
        return Ok(json!({ "funder_height": funder_height }));
    }

    // Spent: verify the funding output against the client's hint, then scan the
    // spending candidates. The funding block is normally already cached.
    let funding_raw = resolve_raw(shared, txid, funder_height)?;
    let funding = decode_raw_tx(&funding_raw)?;
    let Some(output) = funding.output.get(vout as usize) else {
        return Ok(json!({}));
    };
    if ScriptHash::from_script(output.script_pubkey.as_bytes()) != script {
        return Ok(json!({}));
    }

    let candidates = {
        let _g = read_guard(shared)?;
        let entries = shared
            .store
            .history_for_script(script)
            .map_err(|e| RpcError::server(e.to_string()))?;
        if entries.len() > MAX_HISTORY_ENTRIES {
            return Err(RpcError::server(format!(
                "history for this script has {} entries (limit {}); cannot resolve the spender",
                entries.len(),
                MAX_HISTORY_ENTRIES
            )));
        }
        let mut candidates = Vec::new();
        for (height, tx_pos, effect) in entries {
            if height < funder_height || effect.spent_sat == 0 {
                continue;
            }
            let rec = shared
                .store
                .get_block(height)
                .map_err(|e| RpcError::server(e.to_string()))?
                .ok_or_else(|| RpcError::server(format!("block {height} missing")))?;
            let candidate = shared
                .store
                .txid_at(rec.txnum_base + tx_pos as u64)
                .map_err(|e| RpcError::server(e.to_string()))?;
            candidates.push((candidate, height));
        }
        // A busy script (an exchange) can list thousands of spends; bounding
        // the scan keeps a remote outpoint request from pinning external
        // downloads (AUD-167). The most recent spends are the likely ones.
        if candidates.len() > MAX_OUTPOINT_CANDIDATES {
            let skip = candidates.len() - MAX_OUTPOINT_CANDIDATES;
            candidates.drain(..skip);
        }
        candidates
    };

    for (spender, height) in candidates {
        if spender == txid {
            continue;
        }
        let raw = resolve_raw(shared, spender, height)?;
        let tx = decode_raw_tx(&raw)?;
        if tx.input.iter().any(|input| {
            input.previous_output.txid.to_string() == txid.to_hex()
                && input.previous_output.vout == vout
        }) {
            return Ok(json!({
                "funder_height": funder_height,
                "spender_txhash": spender.to_hex(),
                "spender_height": height,
            }));
        }
    }

    Ok(json!({ "funder_height": funder_height }))
}

pub(crate) fn outpoint_get_status(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let (txid, vout, script) = param_outpoint(params)?;
    outpoint_status(shared, txid, vout, script)
}

pub(crate) fn outpoint_subscribe(
    shared: &Shared,
    client: Option<&Arc<ClientState>>,
    params: &Value,
) -> RpcResult<Value> {
    let (txid, vout, script) = param_outpoint(params)?;
    let status = outpoint_status(shared, txid, vout, script)?;
    if let Some(c) = client {
        let mut subs = c
            .outpoints
            .lock()
            .map_err(|_| RpcError::server("outpoint lock poisoned"))?;
        if subs.len() >= 1000 && !subs.contains_key(&(txid, vout)) {
            return Err(RpcError::server(
                "too many outpoint subscriptions on this connection (limit 1000)",
            ));
        }
        subs.insert(
            (txid, vout),
            OutpointSub {
                script,
                status: status.clone(),
            },
        );
    }
    Ok(status)
}

pub(crate) fn outpoint_unsubscribe(
    client: Option<&Arc<ClientState>>,
    params: &Value,
) -> RpcResult<Value> {
    let arr = params
        .as_array()
        .ok_or_else(|| RpcError::invalid("expected [tx_hash, txout_idx]"))?;
    let txid = arr
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid("expected tx_hash hex"))?;
    let txid = Hash32::from_hex(txid).map_err(RpcError::invalid)?;
    let vout = u32::try_from(
        arr.get(1)
            .and_then(Value::as_u64)
            .ok_or_else(|| RpcError::invalid("expected txout_idx"))?,
    )
    .map_err(|_| RpcError::invalid("txout_idx out of range"))?;
    let removed = client
        .and_then(|c| {
            c.outpoints
                .lock()
                .ok()
                .map(|mut subs| subs.remove(&(txid, vout)).is_some())
        })
        .unwrap_or(false);
    Ok(json!(removed))
}

/// Turn Core's `decoderawtransaction` result into the verbose object wallets
/// expect from `blockchain.transaction.get(txid, true)`: Core's decoder lacks
/// the chain context that `getrawtransaction` adds.
pub(crate) fn decorate_verbose_tx(
    decoded: &mut Value,
    raw: &str,
    block: &BlockRecord,
    tip: u32,
) -> RpcResult<()> {
    let header: bitcoin::block::Header = bitcoin::consensus::encode::deserialize(&block.header)
        .map_err(|e| {
            RpcError::server(format!(
                "invalid indexed header at height {}: {e}",
                block.height
            ))
        })?;
    let obj = decoded
        .as_object_mut()
        .ok_or_else(|| RpcError::server("decoderawtransaction returned a non-object"))?;
    obj.insert("hex".to_string(), json!(raw));
    obj.insert("blockhash".to_string(), json!(block.hash.to_hex()));
    obj.insert(
        "confirmations".to_string(),
        json!(u64::from(tip.saturating_sub(block.height)) + 1),
    );
    obj.insert("time".to_string(), json!(header.time));
    obj.insert("blocktime".to_string(), json!(header.time));
    obj.insert("in_active_chain".to_string(), json!(true));
    Ok(())
}

/// Where the index places a confirmed transaction right now: its block and
/// the tip, read under one guard so they describe the same chain.
fn locate_confirmed(shared: &Shared, txid: Hash32) -> RpcResult<Option<(BlockRecord, u32)>> {
    let _g = read_guard(shared)?;
    let occ = shared
        .store
        .occurrences(txid)
        .map_err(|e| RpcError::server(e.to_string()))?;
    let Some((height, _)) = occ.first().copied() else {
        return Ok(None);
    };
    let block = shared
        .store
        .get_block(height)
        .map_err(|e| RpcError::server(e.to_string()))?
        .ok_or_else(|| RpcError::server(format!("block {height} missing")))?;
    let tip = shared
        .store
        .committed_tip()
        .map_err(|e| RpcError::server(e.to_string()))?
        .map(|t| t.height)
        .unwrap_or(height);
    Ok(Some((block, tip)))
}

/// The raw transaction together with the block and tip it is announced
/// with (AUD-186). The download runs without the index lock, so a reorg may
/// move the transaction meanwhile: the location is read before and after, and
/// the bytes are only returned with the block they were fetched for. A reorg
/// in between repeats the resolution instead of mixing two chains.
#[doc(hidden)]
pub fn confirmed_tx_with_block(
    shared: &Shared,
    txid: Hash32,
    fetch: impl Fn(u32) -> RpcResult<String>,
) -> RpcResult<(String, BlockRecord, u32)> {
    const ATTEMPTS: usize = 3;
    for _ in 0..ATTEMPTS {
        let Some((before, _)) = locate_confirmed(shared, txid)? else {
            break;
        };
        let raw = fetch(before.height)?;
        match locate_confirmed(shared, txid)? {
            Some((after, tip)) if after.hash == before.hash => return Ok((raw, after, tip)),
            _ => continue,
        }
    }
    Err(RpcError::server(
        "the transaction moved during a reorganisation; ask again",
    ))
}

pub(crate) fn transaction_get(shared: &Shared, params: &Value) -> RpcResult<Value> {
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
    let (occ, blockhash, warm) = {
        let _g = read_guard(shared)?;
        let occ = shared
            .store
            .occurrences(txid_hash)
            .map_err(|e| RpcError::server(e.to_string()))?;
        let mut blockhash = None;
        let mut warm = None;
        if let Some((height, _)) = occ.first() {
            if let Ok(Some(rec)) = shared.store.get_block(*height) {
                blockhash = Some(rec.hash.to_hex());
                // A pruned block is needed: start its download now, in
                // parallel with the per-txid path. On demand only (no
                // speculative history prefetch in tx mode).
                if u64::from(*height) < shared.stats.core_pruneheight.load(Ordering::Relaxed)
                    && !shared.prefetch.wants(&rec.hash)
                {
                    if let Ok(txids) = shared.store.block_txids(&rec) {
                        warm = Some((rec.hash, *height, txids));
                    }
                }
            }
        }
        (occ, blockhash, warm)
    };
    if let Some((hash, height, txids)) = warm {
        shared.prefetch.enqueue(hash, height, txids);
    }

    if verbose {
        // For a confirmed transaction use the verified resolver and decode the
        // raw bytes locally through Core. This works on a pruned node, where
        // getrawtransaction(txid, true, blockhash) would fail.
        if !occ.is_empty() {
            let (raw, block, tip) = confirmed_tx_with_block(shared, txid_hash, |height| {
                let _guard = rawtx_reader_guard(shared, height);
                shared
                    .resolver
                    .raw_tx_for(current_client(), txid_hash, height)
                    .map_err(|e| rawtx_failure(shared, height, e))
            })?;
            let mut decoded = shared.core.decode_raw_transaction(&raw).map_err(|e| {
                note_tx_get_failure(shared, TxGetFailure::Internal, Some(block.height));
                RpcError::server(format!("rawtx decode: {e}"))
            })?;
            decorate_verbose_tx(&mut decoded, &raw, &block, tip)?;
            return Ok(decoded);
        }
        let mut value = shared
            .core
            .raw_transaction_json(txid, blockhash.as_deref())
            .map_err(|e| {
                note_tx_get_failure(shared, TxGetFailure::NotFound, None);
                RpcError::server(format!("rawtx unavailable: {e}"))
            })?;
        if let Some(obj) = value.as_object_mut() {
            if !obj.contains_key("confirmations") {
                obj.insert("confirmations".to_string(), json!(0));
            }
        }
        return Ok(value);
    }

    match occ.first().copied() {
        Some((height, _)) => {
            // Keep the prune gate away from this height only when Core can
            // still serve it; external-only heights must not block pruning.
            let _guard = rawtx_reader_guard(shared, height);
            shared
                .resolver
                .raw_tx_for(current_client(), txid_hash, height)
                .map(|raw| json!(raw))
                .map_err(|e| rawtx_failure(shared, height, e))
        }
        None => shared
            .core
            .raw_transaction_hex_opt(txid, None)
            .map(|raw| json!(raw))
            .map_err(|e| {
                note_tx_get_failure(shared, TxGetFailure::NotFound, None);
                RpcError::server(format!("transaction not found: {e}"))
            }),
    }
}

pub(crate) fn mempool_info(shared: &Shared) -> RpcResult<Value> {
    let info = shared
        .core
        .mempool_info()
        .map_err(|e| RpcError::server(format!("mempool info: {e}")))?;
    Ok(json!({
        "mempoolminfee": info.mempoolminfee,
        "minrelaytxfee": info.minrelaytxfee,
        "incrementalrelayfee": info.incrementalrelayfee,
    }))
}

pub(crate) fn mempool_recent(shared: &Shared) -> RpcResult<Value> {
    let mem = shared
        .mempool
        .read()
        .map_err(|_| RpcError::server("mempool lock poisoned"))?;
    Ok(Value::Array(
        mem.recent(10)
            .into_iter()
            .map(|(txid, fee, vsize)| {
                json!({
                    "txid": txid.to_hex(),
                    "fee": fee,
                    "vsize": vsize,
                })
            })
            .collect(),
    ))
}

pub(crate) fn fee_histogram(shared: &Shared) -> RpcResult<Value> {
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

pub(crate) fn relay_fee(shared: &Shared) -> RpcResult<Value> {
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

pub(crate) fn estimate_fee(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let target = param_u32(params, 0)?.clamp(1, 1008);
    let mode = params
        .as_array()
        .and_then(|a| a.get(1))
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(mode) = &mode {
        if !matches!(mode.as_str(), "unset" | "economical" | "conservative") {
            return Err(RpcError::invalid(
                "estimatefee mode must be unset, economical or conservative",
            ));
        }
    }
    let key = (target, mode.clone());
    {
        let cache = shared
            .fee_estimates
            .lock()
            .map_err(|_| RpcError::server("fee lock poisoned"))?;
        if let Some((value, at)) = cache.get(&key) {
            if at.elapsed() < Duration::from_secs(30) {
                return Ok(json!(value));
            }
        }
    }

    let mut value = None;
    if let Ok(estimate) = shared.core.estimate_smart_fee(target, mode.as_deref()) {
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
        .insert(key, (value, Instant::now()));
    Ok(json!(value))
}

pub(crate) fn block_stats(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let height = param_u32(params, 0)? as u64;
    shared
        .core
        .block_stats(height)
        .map_err(|e| RpcError::server(e.to_string()))
}

pub(crate) fn broadcast_transaction(shared: &Shared, params: &Value) -> RpcResult<Value> {
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

pub(crate) fn broadcast_package(
    shared: &Shared,
    params: &Value,
    version: ProtocolVersion,
) -> RpcResult<Value> {
    if version < V1_6 {
        return Err(RpcError::method(
            "blockchain.transaction.broadcast_package requires protocol 1.6",
        ));
    }
    let args = params
        .as_array()
        .ok_or_else(|| RpcError::invalid("expected [raw_txs, verbose]"))?;
    let raw = args
        .first()
        .and_then(Value::as_array)
        .ok_or_else(|| RpcError::invalid("expected an array of raw transactions"))?;
    let mut raw_txs = Vec::with_capacity(raw.len());
    for tx in raw {
        raw_txs.push(
            tx.as_str()
                .ok_or_else(|| RpcError::invalid("raw transactions must be hex strings"))?
                .to_string(),
        );
    }
    if raw_txs.is_empty() {
        return Err(RpcError::invalid(
            "broadcast_package expects a non-empty array of raw transactions",
        ));
    }
    let verbose = args.get(1).and_then(Value::as_bool).unwrap_or(false);
    let result = shared
        .core
        .submit_package(&raw_txs)
        .map_err(|e| RpcError::server(format!("submitpackage: {e}")))?;
    Ok(if verbose {
        result
    } else {
        submit_package_summary(&result)
    })
}

/// Convert Core's `submitpackage` response to the stable 1.6 shape:
/// `{"success": bool, "errors": [{"txid", "error"}, ...]}`.
pub(crate) fn submit_package_summary(core: &Value) -> Value {
    let success = core.get("package_msg").and_then(Value::as_str) == Some("success");
    let mut errors = Vec::new();
    if let Some(results) = core.get("tx-results").and_then(Value::as_object) {
        for result in results.values() {
            if let Some(error) = result.get("error").and_then(Value::as_str) {
                errors.push(json!({
                    "txid": result.get("txid").cloned().unwrap_or(Value::Null),
                    "error": error,
                }));
            }
        }
    }
    if errors.is_empty() {
        json!({ "success": success })
    } else {
        json!({ "success": success, "errors": errors })
    }
}

pub(crate) fn test_mempool_accept(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let raw = params
        .as_array()
        .and_then(|a| a.first())
        .and_then(Value::as_array)
        .ok_or_else(|| RpcError::invalid("expected an array of raw transactions"))?;
    let mut raw_txs = Vec::with_capacity(raw.len());
    for tx in raw {
        raw_txs.push(
            tx.as_str()
                .ok_or_else(|| RpcError::invalid("raw transactions must be hex strings"))?
                .to_string(),
        );
    }
    if raw_txs.is_empty() {
        return Err(RpcError::invalid(
            "testmempoolaccept expects a non-empty array of raw transactions",
        ));
    }
    let value = shared
        .core
        .test_mempool_accept(&raw_txs)
        .map_err(|e| RpcError::server(format!("testmempoolaccept: {e}")))?;
    // Electrum documents `reason`; Core names it `reject-reason`.
    let Some(items) = value.as_array() else {
        return Ok(value);
    };
    let mapped = items
        .iter()
        .map(|item| {
            let mut obj = item.as_object().cloned().unwrap_or_default();
            if let Some(reason) = obj.remove("reject-reason") {
                obj.insert("reason".to_string(), reason);
            }
            Value::Object(obj)
        })
        .collect();
    Ok(Value::Array(mapped))
}

pub(crate) fn silent_payments(shared: &Shared, method: &str) -> RpcResult<Value> {
    if !shared.silent_payments_enabled {
        return Err(RpcError::server(
            "silent payments are disabled by configuration ([silent_payments] enabled = false)",
        ));
    }
    // The opt-in gate exists so a future BIP352 indexer can be wired without
    // changing the protocol surface. Until then, fail explicitly.
    Err(RpcError::server(format!(
        "{method}: silent payments are enabled but this build has no BIP352 indexer (see docs/adr/ADR-018-SILENT-PAYMENTS.md)"
    )))
}

pub(crate) fn scripthash_mempool(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let script = param_scripthash(params)?;
    mempool_value(shared, script)
}

/// Txids of recently proven blocks, by block hash. A wallet asks for the
/// merkle proof of every history transaction (Sparrow: pages of 250); this
/// reads each block's txids from `txids.dat` once instead of once per proof
/// (AUD-166). Txids of a block hash never change, so sharing is safe.
pub(crate) const MERKLE_CACHE_BLOCKS: usize = 64;

/// The txids of one block and, once a proof was asked for, its merkle tree:
/// the tree is built once per block, not once per proof (AUD-185).
pub(crate) struct BlockTxids {
    pub(crate) txids: Vec<Hash32>,
    tree: std::sync::OnceLock<Option<el_model::MerkleTree>>,
}

impl BlockTxids {
    pub(crate) fn new(txids: Vec<Hash32>) -> Self {
        Self {
            txids,
            tree: std::sync::OnceLock::new(),
        }
    }

    pub(crate) fn branch(&self, pos: usize) -> Option<Vec<Hash32>> {
        self.tree
            .get_or_init(|| el_model::MerkleTree::new(&self.txids))
            .as_ref()?
            .branch(pos)
    }

    #[cfg(test)]
    pub(crate) fn tree_built(&self) -> bool {
        self.tree.get().is_some()
    }
}

pub(crate) type TxidsByBlock = VecDeque<(Hash32, Arc<BlockTxids>)>;

pub(crate) static BLOCK_TXIDS: std::sync::LazyLock<Mutex<TxidsByBlock>> =
    std::sync::LazyLock::new(|| Mutex::new(VecDeque::new()));

pub(crate) fn block_txids_cached(
    shared: &Shared,
    rec: &el_model::BlockRecord,
) -> RpcResult<Arc<BlockTxids>> {
    if let Ok(cache) = BLOCK_TXIDS.lock() {
        if let Some((_, txids)) = cache.iter().find(|(h, _)| *h == rec.hash) {
            return Ok(Arc::clone(txids));
        }
    }
    let txids = Arc::new(BlockTxids::new(
        shared
            .store
            .block_txids(rec)
            .map_err(|e| RpcError::server(e.to_string()))?,
    ));
    if let Ok(mut cache) = BLOCK_TXIDS.lock() {
        cache.retain(|(h, _)| *h != rec.hash);
        cache.push_back((rec.hash, Arc::clone(&txids)));
        while cache.len() > MERKLE_CACHE_BLOCKS {
            cache.pop_front();
        }
    }
    Ok(txids)
}

pub(crate) fn transaction_merkle(shared: &Shared, params: &Value) -> RpcResult<Value> {
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
    let block = block_txids_cached(shared, &rec)?;
    let pos = block
        .txids
        .iter()
        .position(|t| *t == txid_hash)
        .ok_or_else(|| RpcError::server("transaction not in that block"))?;
    let branch = block
        .branch(pos)
        .ok_or_else(|| RpcError::server("cannot compute merkle branch"))?;
    Ok(json!({
        "block_height": height,
        "merkle": branch.iter().map(|h| h.to_hex()).collect::<Vec<_>>(),
        "pos": pos,
    }))
}

pub(crate) fn transaction_id_from_pos(shared: &Shared, params: &Value) -> RpcResult<Value> {
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
    let block = block_txids_cached(shared, &rec)?;
    let txid = block
        .txids
        .get(pos as usize)
        .ok_or_else(|| RpcError::server("position out of range"))?;
    if !want_merkle {
        return Ok(json!(txid.to_hex()));
    }
    let branch = block
        .branch(pos as usize)
        .ok_or_else(|| RpcError::server("cannot compute merkle branch"))?;
    Ok(json!({
        "tx_hash": txid.to_hex(),
        "merkle": branch.iter().map(|h| h.to_hex()).collect::<Vec<_>>(),
    }))
}
