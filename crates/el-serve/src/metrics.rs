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

pub(crate) fn metrics_text(shared: &Shared) -> String {
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
        "e_lite_core_pruneheight",
        "Lowest block still available in Core",
        "gauge",
        st["core"]["pruneheight"].as_u64().unwrap_or(0),
    );
    line(
        "e_lite_core_retention_blocks",
        "Blocks Core can still serve before pruning",
        "gauge",
        g("retention_blocks"),
    );
    line(
        "e_lite_blocks_applied",
        "Blocks applied to the index",
        "counter",
        g("blocks_applied"),
    );
    line(
        "e_lite_tuning_readers",
        "Effective parallel fetch readers",
        "gauge",
        st["tuning"]["readers"].as_u64().unwrap_or(0),
    );
    line(
        "e_lite_tuning_commit_interval_ms",
        "Effective commit interval",
        "gauge",
        st["tuning"]["commit_interval_ms"].as_u64().unwrap_or(0),
    );
    line(
        "e_lite_tuning_decode_ms",
        "Block decode/hash latency (EWMA)",
        "gauge",
        st["tuning"]["decode_ms_ewma"].as_u64().unwrap_or(0),
    );
    line(
        "e_lite_tuning_wait_ms",
        "Pipeline wait latency (EWMA)",
        "gauge",
        st["tuning"]["wait_ms_ewma"].as_u64().unwrap_or(0),
    );
    line(
        "e_lite_tuning_cache_mib",
        "Effective RocksDB block cache",
        "gauge",
        st["tuning"]["cache_mib"].as_u64().unwrap_or(0),
    );
    line(
        "e_lite_rawtx_cache_hits",
        "Raw transactions served from the validated block cache",
        "counter",
        st["rawtx"]["cache_hits"].as_u64().unwrap_or(0),
    );
    line(
        "e_lite_rawtx_fetches",
        "Whole blocks fetched from external sources",
        "counter",
        st["rawtx"]["fetches"].as_u64().unwrap_or(0),
    );
    line(
        "e_lite_rawtx_unavailable",
        "Raw transactions that could not be resolved",
        "counter",
        st["rawtx"]["unavailable"].as_u64().unwrap_or(0),
    );
    line(
        "e_lite_rawtx_decoys_pending",
        "Decoy block downloads owed for real external downloads",
        "gauge",
        st["rawtx"]["decoys_pending"].as_u64().unwrap_or(0),
    );
    line(
        "e_lite_rawtx_p2p_failures",
        "P2P peer fetch failures",
        "counter",
        st["rawtx"]["p2p_failures"].as_u64().unwrap_or(0),
    );
    line(
        "e_lite_rawtx_p2p_skipped",
        "P2P fetches skipped because every peer was cooling down",
        "counter",
        st["rawtx"]["p2p_skipped"].as_u64().unwrap_or(0),
    );
    line(
        "e_lite_prefetch_queued",
        "History blocks queued for background prefetch",
        "counter",
        st["prefetch"]["queued"].as_u64().unwrap_or(0),
    );
    line(
        "e_lite_prefetch_done",
        "History blocks processed by the prefetcher",
        "counter",
        st["prefetch"]["done"].as_u64().unwrap_or(0),
    );
    line(
        "e_lite_memory_available_mib",
        "Memory available to e-lite (cgroup aware)",
        "gauge",
        st["memory"]["available_mib"].as_u64().unwrap_or(0),
    );
    line(
        "e_lite_memory_total_mib",
        "Memory visible to e-lite (cgroup aware)",
        "gauge",
        st["memory"]["total_mib"].as_u64().unwrap_or(0),
    );
    line(
        "e_lite_tx_per_sec",
        "Indexed transactions per second (EWMA)",
        "gauge",
        st["tuning"]["tx_per_sec"].as_u64().unwrap_or(0),
    );
    line(
        "e_lite_blocks_per_sec",
        "Indexed blocks per second (EWMA)",
        "gauge",
        st["tuning"]["blocks_per_sec"].as_u64().unwrap_or(0),
    );
    line(
        "e_lite_ready",
        "1 when the index has caught up to Core's validated blocks",
        "gauge",
        if st["ready"].as_bool().unwrap_or(false) {
            1
        } else {
            0
        },
    );
    line(
        "e_lite_fully_synced",
        "1 when Core is out of IBD and the index is at the network tip",
        "gauge",
        if st["fully_synced"].as_bool().unwrap_or(false) {
            1
        } else {
            0
        },
    );
    line(
        "e_lite_commit_ms",
        "Commit latency (EWMA)",
        "gauge",
        st["tuning"]["commit_ms_ewma"].as_u64().unwrap_or(0),
    );
    line(
        "e_lite_disk_low",
        "1 when the disk guard has paused indexing",
        "gauge",
        if st["disk_low"].as_bool().unwrap_or(false) {
            1
        } else {
            0
        },
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
        "e_lite_tls_handshakes_total",
        "Completed Electrum TLS handshakes",
        "counter",
        g("tls_handshakes"),
    );
    line(
        "e_lite_tls_handshake_failures_total",
        "Failed Electrum TLS handshakes",
        "counter",
        g("tls_handshake_failures"),
    );
    line(
        "e_lite_errors_total",
        "Protocol and internal errors",
        "counter",
        g("errors_total"),
    );
    line(
        "e_lite_rate_limited_total",
        "Requests delayed by the per-client rate limiter",
        "counter",
        g("rate_limited"),
    );
    line(
        "e_lite_providers_healthy",
        "Public providers that answered the startup probe",
        "gauge",
        g("providers_healthy"),
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
