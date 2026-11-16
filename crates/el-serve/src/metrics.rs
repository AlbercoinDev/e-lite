//! metrics module (split from lib.rs in alpha.29.17.0).

use super::*;

/// Local /status and /metrics endpoint. Loopback only by default.
pub fn start_metrics(
    shared: Arc<Shared>,
    addr: String,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let listener = bind_listener(&addr, "metrics", None)?;
    Ok(start_metrics_on(shared, listener))
}

/// Serve `/status`, `/metrics` and `/healthz` on an already-bound listener.
pub fn start_metrics_on(shared: Arc<Shared>, listener: TcpListener) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
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
    })
}

/// Every numeric metric, one per line: name, type, the dotted path of its
/// value in `/status`, and the help text. A boolean is exported as 0/1.
const METRICS: &str = "\
e_lite_indexed_height gauge indexed_height Indexed tip height
e_lite_core_height gauge core_height Bitcoin Core tip height
e_lite_lag_blocks gauge lag_blocks Blocks behind Core
e_lite_core_pruneheight gauge core.pruneheight Lowest block still available in Core
e_lite_core_retention_blocks gauge retention_blocks Blocks Core can still serve before pruning
e_lite_blocks_applied counter blocks_applied Blocks applied to the index
e_lite_tuning_readers gauge tuning.readers Effective parallel fetch readers
e_lite_tuning_commit_interval_ms gauge tuning.commit_interval_ms Effective commit interval
e_lite_tuning_decode_ms gauge tuning.decode_ms_ewma Block decode/hash latency (EWMA)
e_lite_tuning_wait_ms gauge tuning.wait_ms_ewma Pipeline wait latency (EWMA)
e_lite_tuning_cache_mib gauge tuning.cache_mib Effective RocksDB block cache
e_lite_rawtx_cache_hits counter rawtx.cache_hits Raw transactions served from the validated block cache
e_lite_rawtx_fetches counter rawtx.fetches Whole blocks fetched from external sources
e_lite_rawtx_unavailable counter rawtx.unavailable Raw transactions that could not be resolved
e_lite_rawtx_decoys_pending gauge rawtx.decoys_pending Decoy block downloads owed for real external downloads
e_lite_rawtx_p2p_failures counter rawtx.p2p_failures P2P peer fetch failures
e_lite_rawtx_p2p_skipped counter rawtx.p2p_skipped P2P fetches skipped because every peer was cooling down
e_lite_prefetch_queued counter prefetch.queued History blocks queued for background prefetch
e_lite_prefetch_done counter prefetch.done History blocks processed by the prefetcher
e_lite_memory_available_mib gauge memory.available_mib Memory available to e-lite (cgroup aware)
e_lite_memory_total_mib gauge memory.total_mib Memory visible to e-lite (cgroup aware)
e_lite_tx_per_sec gauge tuning.tx_per_sec Indexed transactions per second (EWMA)
e_lite_blocks_per_sec gauge tuning.blocks_per_sec Indexed blocks per second (EWMA)
e_lite_ready gauge ready 1 when the index has caught up to Core's validated blocks
e_lite_fully_synced gauge fully_synced 1 when Core is out of IBD and the index is at the network tip
e_lite_commit_ms gauge tuning.commit_ms_ewma Commit latency (EWMA)
e_lite_disk_low gauge disk_low 1 when the disk guard has paused indexing
e_lite_txnum counter txnum Indexed transaction occurrences
e_lite_db_bytes gauge db_bytes Index size on disk
e_lite_disk_free_bytes gauge disk_free_bytes Free disk on the data directory
e_lite_server_clients gauge clients Connected Electrum clients
e_lite_server_requests_total counter requests_total Electrum requests served
e_lite_tls_handshakes_total counter tls_handshakes Completed Electrum TLS handshakes
e_lite_tls_handshake_failures_total counter tls_handshake_failures Failed Electrum TLS handshakes
e_lite_errors_total counter errors_total Protocol and internal errors
e_lite_rate_limited_total counter rate_limited Requests delayed by the per-client rate limiter
e_lite_providers_healthy gauge providers_healthy Public providers that answered the startup probe
e_lite_last_block_ms gauge last_block_ms Duration of the last applied block
e_lite_uptime_seconds gauge uptime_seconds Process uptime
";

/// The table above as `(name, kind, path, help)`.
fn metrics_table() -> impl Iterator<Item = (&'static str, &'static str, &'static str, &'static str)>
{
    METRICS.lines().filter_map(|row| {
        let mut fields = row.splitn(4, ' ');
        Some((
            fields.next()?,
            fields.next()?,
            fields.next()?,
            fields.next()?,
        ))
    })
}

/// The number at a dotted path of the status document (0 when absent).
fn status_number(status: &Value, path: &str) -> u64 {
    let value = path.split('.').fold(status, |v, key| &v[key]);
    value
        .as_u64()
        .unwrap_or_else(|| u64::from(value.as_bool().unwrap_or(false)))
}

pub(crate) fn metrics_text(shared: &Shared) -> String {
    let st = shared.status_json();
    let mut out = String::new();
    for (name, kind, path, help) in metrics_table() {
        let value = status_number(&st, path);
        out.push_str(&format!(
            "# HELP {name} {help}\n# TYPE {name} {kind}\n{name} {value}\n"
        ));
    }
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
