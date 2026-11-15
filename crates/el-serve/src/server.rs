//! server module (split from lib.rs in alpha.29.17.0).

use super::*;

pub fn start_server(shared: Arc<Shared>) -> std::io::Result<std::thread::JoinHandle<()>> {
    let listener = TcpListener::bind(&shared.listen)?;
    Ok(start_listener_on(listener, shared, None))
}

/// Serve an already-bound listener. When `tls` is set, every accepted
/// connection is wrapped in TLS before it reaches the JSON-RPC loop.
pub fn start_listener_on(
    listener: TcpListener,
    shared: Arc<Shared>,
    tls: Option<Arc<CertWatcher>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        for incoming in listener.incoming() {
            let stream = match incoming {
                Ok(s) => s,
                Err(_) => continue,
            };
            // Reserve the slot before spawning: checking and incrementing
            // inside the child thread lets a burst exceed max_clients.
            let before = shared.stats.clients.fetch_add(1, Ordering::SeqCst);
            if before >= shared.max_clients {
                shared.stats.clients.fetch_sub(1, Ordering::SeqCst);
                let _ = stream.shutdown(std::net::Shutdown::Both);
                continue;
            }
            let wire = match &tls {
                Some(watcher) => Wire::tls(watcher.config(), stream),
                None => Wire::plain(stream),
            };
            let wire = match wire {
                Ok(w) => w,
                Err(_) => {
                    if tls.is_some() {
                        shared
                            .stats
                            .tls_handshake_failures
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    shared.stats.clients.fetch_sub(1, Ordering::SeqCst);
                    continue;
                }
            };
            let client = shared.clients.register(wire);
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || {
                let _ = handle_client(&shared, client);
                shared.stats.clients.fetch_sub(1, Ordering::SeqCst);
            });
        }
    })
}

/// Reads from the client while releasing the connection lock between attempts,
/// so the notifier thread can write to the same TLS connection.
pub(crate) struct WireReader {
    client: Arc<ClientState>,
    idle_deadline: Instant,
    poll: Duration,
}

impl WireReader {
    fn new(client: Arc<ClientState>) -> Self {
        Self {
            client,
            idle_deadline: Instant::now() + CLIENT_IDLE_TIMEOUT,
            poll: CLIENT_READ_POLL,
        }
    }
}

impl Read for WireReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            {
                let mut wire = self
                    .client
                    .writer
                    .lock()
                    .map_err(|_| std::io::Error::other("connection lock poisoned"))?;
                // Short polls while background answers are pending, so their
                // writer gets the lock within ~10 ms (AUD-164).
                let poll = if self.client.background.load(Ordering::Relaxed) > 0 {
                    CLIENT_READ_POLL_BUSY
                } else {
                    CLIENT_READ_POLL
                };
                if poll != self.poll && wire.socket().set_read_timeout(Some(poll)).is_ok() {
                    self.poll = poll;
                }
                let read = wire.read(buf);
                drop(wire);
                // Notifications queued while this poll held the lock.
                self.client.flush_pending();
                match read {
                    Ok(n) => {
                        self.idle_deadline = Instant::now() + CLIENT_IDLE_TIMEOUT;
                        return Ok(n);
                    }
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut => {}
                    Err(e) => return Err(e),
                }
            }
            if Instant::now() >= self.idle_deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "client idle timeout",
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// Read one newline-terminated line without allowing the buffer to exceed
/// `max` bytes. `Ok(false)` means clean EOF.
pub(crate) fn read_line_limited<R: BufRead>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<bool> {
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(!buf.is_empty());
        }
        if let Some(pos) = available.iter().position(|b| *b == b'\n') {
            let take = pos + 1;
            if buf.len().saturating_add(take) > max {
                reader.consume(take);
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "request line too large",
                ));
            }
            buf.extend_from_slice(&available[..take]);
            reader.consume(take);
            return Ok(true);
        }
        let take = available.len();
        if buf.len().saturating_add(take) > max {
            reader.consume(take);
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "request line too large",
            ));
        }
        buf.extend_from_slice(available);
        reader.consume(take);
    }
}

pub(crate) fn handle_client(shared: &Shared, client: Arc<ClientState>) -> std::io::Result<()> {
    client.set_baseline(shared);
    if debug_logs() {
        eprintln!(
            "[e-lite] debug: wallet: connection {} (tls {})",
            client.id, client.tls
        );
    }
    let result = client_loop(shared, &client);
    client.alive.store(false, Ordering::SeqCst);
    shared.clients.unregister(client.id);
    shared.resolver.forget_client(client.id);
    client.log_close(shared);
    result
}

pub(crate) fn client_loop(shared: &Shared, client: &Arc<ClientState>) -> std::io::Result<()> {
    if client.tls {
        let handshake = {
            let mut wire = client
                .writer
                .lock()
                .map_err(|_| std::io::Error::other("connection lock poisoned"))?;
            wire.handshake()
        };
        if let Err(e) = handshake {
            shared
                .stats
                .tls_handshake_failures
                .fetch_add(1, Ordering::Relaxed);
            return Err(e);
        }
        shared.stats.tls_handshakes.fetch_add(1, Ordering::Relaxed);
    }

    let mut reader = BufReader::new(WireReader::new(Arc::clone(client)));
    let mut buf = Vec::new();
    // Lines that may wait on an external block download run beside the
    // connection's reader (AUD-160): a slow `transaction.get` must not hold
    // every later request of the connection, including the wallet's retry
    // of the same batch after its own read timeout. Responses carry their
    // ids, so their order on the wire does not matter.
    let in_flight = (Mutex::new(0usize), Condvar::new());
    let closing = AtomicBool::new(false);
    let wait_below = |limit: usize| {
        let (count, cv) = &in_flight;
        let mut n = count.lock().unwrap_or_else(|p| p.into_inner());
        while *n > limit.saturating_sub(1) {
            n = cv.wait(n).unwrap_or_else(|p| p.into_inner());
        }
        n
    };
    std::thread::scope(|scope| {
        loop {
            if closing.load(Ordering::Relaxed) {
                break;
            }
            buf.clear();
            match read_line_limited(&mut reader, &mut buf, MAX_REQUEST_BYTES) {
                Ok(true) => {}
                Ok(false) => break,
                Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                    let _ = client.send(&json!({
                        "jsonrpc": "2.0",
                        "id": Value::Null,
                        "error": {"code": -32600, "message": "request too large"},
                    }));
                    break;
                }
                Err(_) => break,
            }
            let line = String::from_utf8_lossy(&buf).into_owned();
            if line.trim().is_empty() {
                continue;
            }
            if may_wait_on_network(&line) {
                let mut n = wait_below(PIPELINE_DEPTH);
                *n += 1;
                drop(n);
                client.background.fetch_add(1, Ordering::Relaxed);
                let in_flight = &in_flight;
                let closing = &closing;
                scope.spawn(move || {
                    let outcome = process_line_for(shared, Some(client), &line);
                    let sent = outcome
                        .response
                        .as_ref()
                        .is_none_or(|response| client.send(response));
                    if !sent || outcome.close {
                        closing.store(true, Ordering::Relaxed);
                    }
                    client.background.fetch_sub(1, Ordering::Relaxed);
                    let (count, cv) = in_flight;
                    *count.lock().unwrap_or_else(|p| p.into_inner()) -= 1;
                    cv.notify_all();
                });
                continue;
            }
            // `server.version` fixes the protocol of everything after it:
            // let the background lines finish first.
            if line.contains("server.version") {
                drop(wait_below(1));
            }
            let outcome = process_line_for(shared, Some(client), &line);
            if let Some(response) = outcome.response {
                if !client.send(&response) {
                    break;
                }
            }
            if outcome.close {
                break;
            }
        }
    });
    Ok(())
}

/// Lines of one connection that may run at the same time while they wait on
/// an external block download.
pub const PIPELINE_DEPTH: usize = 4;

/// Whether a request line may wait on an external block download.
pub(crate) fn may_wait_on_network(line: &str) -> bool {
    line.contains("blockchain.transaction.get\"")
}

/// Result of one line from the wire. Notifications have no response; a fatal
/// protocol error asks the caller to close the connection after replying.
pub struct LineOutcome {
    pub response: Option<Value>,
    pub close: bool,
}

pub fn process_line(shared: &Shared, line: &str) -> Value {
    process_line_for(shared, None, line)
        .response
        .unwrap_or(Value::Null)
}

pub(crate) fn negotiated_client_version(client: Option<&Arc<ClientState>>) -> ProtocolVersion {
    if let Some(c) = client {
        if let Ok(protocol) = c.protocol.lock() {
            if let Some(version) = *protocol {
                return version;
            }
        }
    }
    SERVER_MIN
}

/// Most requests one JSON-RPC batch may carry. A batch line is also bounded
/// by `MAX_REQUEST_BYTES`.
pub const MAX_BATCH_REQUESTS: usize = 1000;

/// Requests of one batch resolved at the same time. External block
/// downloads stay bounded by `rawtx.max_parallel_fetches` underneath.
pub const BATCH_WORKERS: usize = 8;
/// Workers of a batch that carries `transaction.get`: those mostly wait on
/// the network, and every distinct block of the batch should be in the
/// download queue at once, not eight at a time (AUD-160).
pub const BATCH_IO_WORKERS: usize = 32;

pub fn process_line_for(
    shared: &Shared,
    client: Option<&Arc<ClientState>>,
    line: &str,
) -> LineOutcome {
    shared.stats.lines_total.fetch_add(1, Ordering::Relaxed);
    let request: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            shared.stats.requests_total.fetch_add(1, Ordering::Relaxed);
            shared.stats.errors_total.fetch_add(1, Ordering::Relaxed);
            return LineOutcome {
                response: Some(json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": {"code": -32700, "message": format!("parse error: {e}")},
                })),
                close: false,
            };
        }
    };
    match request {
        Value::Array(items) => process_batch(shared, client, items),
        request => process_request(shared, client, &request),
    }
}

/// A JSON-RPC 2.0 batch: one array of requests on one line, answered with one
/// array of responses (notifications get none; an all-notification batch gets
/// no reply at all). Wallets that batch (Sparrow, Electrum) send a whole page
/// of `get_history`/`transaction.get` at once, which turns one network round
/// trip per address into one per page: the difference between minutes and
/// seconds over an onion service.
///
/// Items run in parallel unless the batch carries `server.version` or an
/// unsubscribe, whose order relative to the other items matters.
pub(crate) fn process_batch(
    shared: &Shared,
    client: Option<&Arc<ClientState>>,
    items: Vec<Value>,
) -> LineOutcome {
    let invalid = |message: &str| {
        shared.stats.requests_total.fetch_add(1, Ordering::Relaxed);
        shared.stats.errors_total.fetch_add(1, Ordering::Relaxed);
        LineOutcome {
            response: Some(json!({
                "jsonrpc": "2.0",
                "id": Value::Null,
                "error": {"code": -32600, "message": message},
            })),
            close: false,
        }
    };
    if items.is_empty() {
        return invalid("invalid request: empty batch");
    }
    if items.len() > MAX_BATCH_REQUESTS {
        return invalid(&format!(
            "invalid request: batch larger than {MAX_BATCH_REQUESTS} requests"
        ));
    }
    // Measurement, not behaviour: two relaxed counters per line (never per
    // item) so a debug log says whether a wallet is really batching.
    let items_len = items.len() as u64;
    shared.stats.batch_lines.fetch_add(1, Ordering::Relaxed);
    shared
        .stats
        .batch_items
        .fetch_add(items_len, Ordering::Relaxed);
    shared
        .stats
        .batch_max_items
        .fetch_max(items_len, Ordering::Relaxed);
    let mut first_batch = false;
    if let Some(c) = client {
        if let Ok(mut s) = c.stats.lock() {
            first_batch = s.batch_lines == 0;
            s.batch_lines += 1;
            s.batch_items += items_len;
            s.batch_max_items = s.batch_max_items.max(items_len);
        }
    }
    if first_batch && debug_logs() {
        eprintln!("[e-lite] debug: wallet: first batch, {items_len} items (parallel JSON-RPC)");
    }
    let ordered = items.iter().any(|item| {
        item.get("method")
            .and_then(Value::as_str)
            .is_some_and(|m| m == "server.version" || m.ends_with(".unsubscribe"))
    });
    let io_bound = items.iter().any(|item| {
        item.get("method")
            .and_then(Value::as_str)
            .is_some_and(|m| m == "blockchain.transaction.get")
    });
    let workers = if ordered {
        1
    } else if io_bound {
        items.len().min(BATCH_IO_WORKERS)
    } else {
        items.len().min(BATCH_WORKERS)
    };
    let outcomes: Vec<LineOutcome> = if workers <= 1 {
        items
            .iter()
            .map(|item| process_request(shared, client, item))
            .collect()
    } else {
        let next = AtomicUsize::new(0);
        let slots: Vec<Mutex<Option<LineOutcome>>> =
            items.iter().map(|_| Mutex::new(None)).collect();
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| loop {
                    let idx = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(idx) else {
                        return;
                    };
                    let outcome = process_request(shared, client, item);
                    if let Ok(mut slot) = slots[idx].lock() {
                        *slot = Some(outcome);
                    }
                });
            }
        });
        slots
            .into_iter()
            .map(|slot| {
                slot.into_inner().ok().flatten().unwrap_or(LineOutcome {
                    response: Some(json!({
                        "jsonrpc": "2.0",
                        "id": Value::Null,
                        "error": {"code": -32603, "message": "internal error"},
                    })),
                    close: false,
                })
            })
            .collect()
    };
    let close = outcomes.iter().any(|o| o.close);
    let responses: Vec<Value> = outcomes.into_iter().filter_map(|o| o.response).collect();
    LineOutcome {
        response: (!responses.is_empty()).then_some(Value::Array(responses)),
        close,
    }
}

/// One JSON-RPC request object, alone on its line or inside a batch.
pub(crate) fn process_request(
    shared: &Shared,
    client: Option<&Arc<ClientState>>,
    request: &Value,
) -> LineOutcome {
    let _scope = ClientScope::enter(client.map(|c| c.id).unwrap_or(0));
    shared.stats.requests_total.fetch_add(1, Ordering::Relaxed);
    let is_notification = request
        .as_object()
        .map(|obj| !obj.contains_key("id"))
        .unwrap_or(false);
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = match request.get("method").and_then(|m| m.as_str()) {
        Some(m) => m,
        None => {
            shared.stats.errors_total.fetch_add(1, Ordering::Relaxed);
            return LineOutcome {
                response: Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32600, "message": "invalid request"},
                })),
                close: false,
            };
        }
    };
    let params = request.get("params").cloned().unwrap_or(json!([]));
    let throttled = client
        .map(|c| {
            c.budget
                .lock()
                .map(|mut budget| budget.acquire(Instant::now()))
                .unwrap_or(false)
        })
        .unwrap_or(false);
    if throttled {
        // Soft throttle: the request already waited for its token. Keep the
        // counter for observability, but never turn a wallet burst into an
        // error.
        shared.stats.rate_limited.fetch_add(1, Ordering::Relaxed);
    }
    if method != "server.version" {
        if let Some(c) = client {
            c.saw_message.store(true, Ordering::SeqCst);
        }
    }
    let version = negotiated_client_version(client);
    let started = Instant::now();
    let outcome = dispatch_for(shared, client, version, method, &params);
    if let Some(c) = client {
        c.record_request(shared, method, started.elapsed());
    }
    match outcome {
        Ok(result) => LineOutcome {
            response: if is_notification {
                None
            } else {
                Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
            },
            close: false,
        },
        Err(e) => {
            shared.stats.errors_total.fetch_add(1, Ordering::Relaxed);
            log_rpc_error(shared, method, e.code);
            LineOutcome {
                response: if is_notification {
                    None
                } else {
                    Some(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {"code": e.code, "message": e.message},
                    }))
                },
                close: e.close,
            }
        }
    }
}
