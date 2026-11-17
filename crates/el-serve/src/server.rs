//! server module (split from lib.rs in alpha.29.17.0).

use super::*;

/// How long `bind_listener` waits for a port held by an outgoing connection.
/// A closed connection keeps its source port for 60 s (TIME_WAIT).
pub const BIND_RETRY: Duration = Duration::from_secs(75);

/// Bind a listener, waiting when the port is only held by an outgoing
/// connection (AUD-181).
///
/// The default ports are inside the kernel's ephemeral range, so any local
/// connection, including e-lite's own RPC calls to Core, may be using one of
/// them as its *source* port, live or for 60 s after it closed. The kernel
/// then refuses the bind with "address in use" although nothing listens
/// there. That clears by itself, so it is retried; a port another program is
/// really listening on fails at once, and the error says which case it was.
/// `stop` ends the wait early.
pub fn bind_listener(
    addr: &str,
    what: &str,
    stop: Option<&AtomicBool>,
) -> std::io::Result<TcpListener> {
    let deadline = Instant::now() + BIND_RETRY;
    let mut announced = false;
    loop {
        let error = match TcpListener::bind(addr) {
            Ok(listener) => return Ok(listener),
            Err(e) => e,
        };
        if error.kind() != std::io::ErrorKind::AddrInUse {
            return Err(error);
        }
        let explain = |why: &str| std::io::Error::new(error.kind(), format!("{error} ({why})"));
        if someone_listens(addr) {
            return Err(explain("another program is listening on this port"));
        }
        if stop.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
            return Err(explain("stopped while waiting for the port"));
        }
        if Instant::now() >= deadline {
            return Err(explain(
                "nothing listens on it, but outgoing connections kept the port busy",
            ));
        }
        if !announced {
            eprintln!(
                "[e-lite] {what} port {addr} is held for a moment by an outgoing connection \
                 of this host (nothing is listening on it); retrying for up to {} s",
                BIND_RETRY.as_secs()
            );
            announced = true;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Whether a program is listening on the port of `addr` right now. The
/// kernel's socket table answers exactly; a connection attempt is the
/// fallback where `/proc/net` cannot be read.
fn someone_listens(addr: &str) -> bool {
    use std::net::ToSocketAddrs;
    let Ok(resolved) = addr.to_socket_addrs() else {
        return false;
    };
    let targets: Vec<std::net::SocketAddr> = resolved.collect();
    let Some(port) = targets.first().map(|t| t.port()) else {
        return false;
    };
    if let Some(listening) = listening_in_proc(port) {
        return listening;
    }
    targets.into_iter().any(|mut target| {
        if target.ip().is_unspecified() {
            target.set_ip(match target {
                std::net::SocketAddr::V4(_) => std::net::Ipv4Addr::LOCALHOST.into(),
                std::net::SocketAddr::V6(_) => std::net::Ipv6Addr::LOCALHOST.into(),
            });
        }
        TcpStream::connect_timeout(&target, Duration::from_millis(300)).is_ok()
    })
}

/// `Some(true)` when `/proc/net/tcp` or `tcp6` has a socket in LISTEN state
/// on `port`; `None` when neither file can be read.
fn listening_in_proc(port: u16) -> Option<bool> {
    let mut readable = false;
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(table) else {
            continue;
        };
        readable = true;
        if proc_table_listens(&text, port) {
            return Some(true);
        }
    }
    readable.then_some(false)
}

/// One `/proc/net/tcp*` table: `sl local_address rem_address st ...`, the
/// local address as `HEXIP:HEXPORT` and `0A` for the LISTEN state.
pub(crate) fn proc_table_listens(table: &str, port: u16) -> bool {
    table.lines().skip(1).any(|line| {
        let mut fields = line.split_whitespace();
        let local = fields.nth(1);
        let state = fields.nth(1);
        let local_port = local
            .and_then(|l| l.rsplit(':').next())
            .and_then(|hex| u16::from_str_radix(hex, 16).ok());
        local_port == Some(port) && state == Some("0A")
    })
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
            // A loopback peer is the managed Tor handing us an onion client
            // (or a local one): both get the more generous timeouts.
            let tor_client = stream
                .peer_addr()
                .map(|addr| addr.ip().is_loopback())
                .unwrap_or(false);
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
            let client = shared.clients.register(wire, tor_client);
            let slot = ClientSlot {
                shared: Arc::clone(&shared),
                client,
            };
            // Named: the panic hook lets this thread fail alone (AUD-173).
            // If the thread cannot start, the closure is dropped and the
            // slot with it.
            let _ = std::thread::Builder::new()
                .name(el_model::sync::THREAD_CLIENT.to_string())
                .spawn(move || {
                    let _ = handle_client(&slot.shared, Arc::clone(&slot.client));
                });
        }
    })
}

/// One accepted connection: its `max_clients` slot and its registry entry.
/// Released on drop, so a connection thread that unwinds cannot leak the slot
/// (the listener would otherwise refuse clients forever once enough had
/// leaked). A panic in a connection thread is contained (AUD-173), so this
/// guard is what gives the slot back.
struct ClientSlot {
    shared: Arc<Shared>,
    client: Arc<ClientState>,
}

impl Drop for ClientSlot {
    fn drop(&mut self) {
        self.client.alive.store(false, Ordering::SeqCst);
        self.shared.clients.unregister(self.client.id);
        self.shared.resolver.forget_client(self.client.id);
        let counter = if self.client.detached.load(Ordering::SeqCst) {
            &self.shared.stats.detached_clients
        } else {
            &self.shared.stats.clients
        };
        counter.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The client closed its side while lines of this connection still wait on a
/// download: give the `max_clients` slot and the registry entry back now
/// instead of when the downloads end (up to the download budget later). The
/// lines go on and still answer a client that only half-closed. At most
/// `max_clients` connections may be in this state, so cycling connections
/// cannot pile up threads without limit; past that the slot stays held, as
/// before.
fn release_slot_early(shared: &Shared, client: &ClientState) {
    if client.background.load(Ordering::Relaxed) == 0 || client.detached.load(Ordering::SeqCst) {
        return;
    }
    let detached = &shared.stats.detached_clients;
    if detached.fetch_add(1, Ordering::SeqCst) >= shared.max_clients {
        detached.fetch_sub(1, Ordering::SeqCst);
        return;
    }
    client.detached.store(true, Ordering::SeqCst);
    shared.clients.detach(client.id);
    shared.stats.clients.fetch_sub(1, Ordering::SeqCst);
}

/// Whether the client closed its side: a zero-length peek on the socket. Does
/// not consume request bytes that are still unread.
fn peer_closed(client: &ClientState) -> bool {
    let Ok(wire) = client.writer.lock() else {
        return true;
    };
    matches!(wire.socket().peek(&mut [0u8; 1]), Ok(0))
}

/// Reads from the client while releasing the connection lock between attempts,
/// so the notifier thread can write to the same TLS connection.
pub(crate) struct WireReader {
    client: Arc<ClientState>,
    idle_deadline: Instant,
    poll: Duration,
}

impl WireReader {
    pub(crate) fn new(client: Arc<ClientState>) -> Self {
        Self {
            idle_deadline: Instant::now() + client.idle_timeout,
            client,
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
            // Marked dead by a failed write or by the notifier: stop here
            // instead of idling until the 600 s timeout.
            if !self.client.alive.load(Ordering::Relaxed) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionAborted,
                    "connection closed",
                ));
            }
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
                        self.idle_deadline = Instant::now() + self.client.idle_timeout;
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
    // The slot, the registry entry and the resolver state are released by
    // `ClientSlot` when the connection thread ends.
    client.alive.store(false, Ordering::SeqCst);
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
    let in_flight = (std::sync::Mutex::new(0usize), Condvar::new());
    let closing = AtomicBool::new(false);
    // While the reader waits for a free place it is not reading, so it
    // would not see the client leave: look at the socket between waits.
    let wait_below = |limit: usize| {
        let (count, cv) = &in_flight;
        let mut n = count.lock().unwrap_or_else(|p| p.into_inner());
        while *n > limit.saturating_sub(1) {
            let (guard, timeout) = cv
                .wait_timeout(n, PLACE_WAIT_POLL)
                .unwrap_or_else(|p| p.into_inner());
            n = guard;
            if timeout.timed_out() && !client.detached.load(Ordering::Relaxed) {
                drop(n);
                if peer_closed(client) {
                    release_slot_early(shared, client);
                }
                n = count.lock().unwrap_or_else(|p| p.into_inner());
            }
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
                Ok(false) => {
                    release_slot_early(shared, client);
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                    let _ = client.send(&json!({
                        "jsonrpc": "2.0",
                        "id": Value::Null,
                        "error": {"code": -32600, "message": "request too large"},
                    }));
                    break;
                }
                Err(_) => {
                    release_slot_early(shared, client);
                    break;
                }
            }
            let line = String::from_utf8_lossy(&buf);
            if line.trim().is_empty() {
                continue;
            }
            let request = match parse_line(shared, &line) {
                Ok(request) => request,
                Err(outcome) => {
                    if let Some(response) = outcome.response {
                        if !client.send(&response) {
                            break;
                        }
                    }
                    continue;
                }
            };
            if may_wait_on_network(&request) {
                let mut n = wait_below(PIPELINE_DEPTH);
                *n += 1;
                drop(n);
                client.background.fetch_add(1, Ordering::Relaxed);
                // Gives the pipeline place back when the line ends, however
                // it ends: a line that panics, or a thread that cannot
                // start, must not leave the reader waiting for a free place.
                let place = LinePlace {
                    client,
                    in_flight: &in_flight,
                    closing: &closing,
                };
                let spawned = std::thread::Builder::new()
                    .name(el_model::sync::THREAD_LINE.to_string())
                    .spawn_scoped(scope, move || {
                        let outcome = process_parsed(shared, Some(place.client), request);
                        let sent = outcome
                            .response
                            .as_ref()
                            .is_none_or(|response| place.client.send(response));
                        if sent && !outcome.close {
                            place.finished();
                        }
                    });
                if spawned.is_err() {
                    break;
                }
                continue;
            }
            // `server.version` fixes the protocol of everything after it:
            // let the background lines finish first.
            if names_method(&request, "server.version") {
                drop(wait_below(1));
            }
            let outcome = process_parsed(shared, Some(client), request);
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

/// One background line of a connection. Dropping it frees its place; unless
/// `finished` ran first, it also closes the connection (the line could not
/// answer: its write failed, it asked to close, or it panicked).
pub(crate) struct LinePlace<'a> {
    pub(crate) client: &'a Arc<ClientState>,
    pub(crate) in_flight: &'a (std::sync::Mutex<usize>, Condvar),
    pub(crate) closing: &'a AtomicBool,
}

impl LinePlace<'_> {
    /// The line answered and the connection goes on.
    pub(crate) fn finished(self) {
        let mut done = std::mem::ManuallyDrop::new(self);
        done.release();
    }

    fn release(&mut self) {
        self.client.background.fetch_sub(1, Ordering::Relaxed);
        let (count, cv) = self.in_flight;
        let mut n = count.lock().unwrap_or_else(|p| p.into_inner());
        *n = n.saturating_sub(1);
        drop(n);
        cv.notify_all();
    }
}

impl Drop for LinePlace<'_> {
    fn drop(&mut self) {
        self.closing.store(true, Ordering::Relaxed);
        self.release();
    }
}

/// Lines of one connection that may run at the same time while they wait on
/// an external block download.
pub const PIPELINE_DEPTH: usize = 4;

/// How often a reader waiting for a pipeline place checks that the client is
/// still there.
const PLACE_WAIT_POLL: Duration = Duration::from_millis(250);

/// Whether a request (or any item of a batch) calls `method`. Decided from
/// the parsed request, never from the raw text: a parameter that happens to
/// contain a method name does not count.
pub(crate) fn names_method(request: &Value, method: &str) -> bool {
    let calls = |item: &Value| item.get("method").and_then(Value::as_str) == Some(method);
    match request {
        Value::Array(items) => items.iter().any(calls),
        item => calls(item),
    }
}

/// Whether a request may wait on an external block download.
pub(crate) fn may_wait_on_network(request: &Value) -> bool {
    names_method(request, "blockchain.transaction.get")
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
    match parse_line(shared, line) {
        Ok(request) => process_parsed(shared, client, request),
        Err(outcome) => outcome,
    }
}

/// Parse one line from the wire. The error is the JSON-RPC parse-error
/// response, ready to send.
pub(crate) fn parse_line(shared: &Shared, line: &str) -> Result<Value, LineOutcome> {
    shared.stats.lines_total.fetch_add(1, Ordering::Relaxed);
    serde_json::from_str(line).map_err(|e| {
        shared.stats.requests_total.fetch_add(1, Ordering::Relaxed);
        shared.stats.errors_total.fetch_add(1, Ordering::Relaxed);
        LineOutcome {
            response: Some(json!({
                "jsonrpc": "2.0",
                "id": Value::Null,
                "error": {"code": -32700, "message": format!("parse error: {e}")},
            })),
            close: false,
        }
    })
}

/// Serve a parsed line: a single request or a batch.
pub(crate) fn process_parsed(
    shared: &Shared,
    client: Option<&Arc<ClientState>>,
    request: Value,
) -> LineOutcome {
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
                // A worker that cannot start is not an error: the others
                // take its items, and an item nobody took answers -32603.
                let _ = std::thread::Builder::new()
                    .name(el_model::sync::THREAD_BATCH.to_string())
                    .spawn_scoped(scope, || loop {
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
            log_rpc_error(shared, method, &e);
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
