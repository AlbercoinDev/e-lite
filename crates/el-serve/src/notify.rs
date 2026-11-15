//! notify module (split from lib.rs in alpha.29.17.0).

use super::*;

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
        let mut ever_ok = false;
        loop {
            if shared.shutdown.load(Ordering::Relaxed) {
                break;
            }
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
            // Build the candidate outside the write lock: Core RPC and the
            // per-input UTXO lookups can take seconds on a large mempool, and
            // holding `mempool.write()` would block every history/balance read.
            // Cheap check first: the txid list. The overlay is cloned (and
            // only new transactions fetched) when it differs (AUD-138).
            let result = el_mempool::mempool_ids(&shared.core).map(|ids| {
                let unchanged = shared.mempool.read().map(|m| !m.differs_from(&ids)).ok();
                (ids, unchanged)
            });
            let result = match result {
                Ok((_, None)) => break, // lock poisoned
                Ok((_, Some(true))) => Ok(None),
                Ok((ids, Some(false))) => {
                    let mut candidate = match shared.mempool.read() {
                        Ok(m) => m.clone(),
                        Err(_) => break,
                    };
                    candidate
                        .refresh_with_ids(&ids, &shared.core, &shared.store)
                        .map(|changed| changed.then_some(candidate))
                }
                Err(e) => Err(e),
            };
            last_tip = tip;
            last_refresh = Instant::now();
            match result {
                Ok(updated) => {
                    ever_ok = true;
                    if let Some(candidate) = updated {
                        let touched: Vec<ScriptHash> =
                            candidate.last_touched().iter().copied().collect();
                        if let Ok(mut mem) = shared.mempool.write() {
                            *mem = candidate;
                        }
                        shared.dirty_scripts.mark_all(touched);
                    }
                    if !last_err.is_empty() {
                        eprintln!("[e-lite] mempool refresh recovered");
                        last_err.clear();
                    }
                }
                Err(e) => {
                    let msg = e.to_string();
                    let starting = matches!(
                        shared.get_state(),
                        state::WAITING_FOR_CORE | state::INITIALIZING
                    );
                    let connection_noise = msg.contains("Connection refused")
                        || msg.contains("io:")
                        || msg.contains("Loading block index")
                        || msg.contains("Verifying blocks");
                    // Before the first successful refresh (serve offline, Core
                    // still loading), connection errors are expected and must
                    // not look like failures. After a successful refresh they
                    // are logged once, so a real outage is visible.
                    let expected = !ever_ok && connection_noise;
                    if msg != last_err && !starting && !expected {
                        eprintln!("[e-lite] mempool refresh: {msg}");
                    }
                    last_err = msg;
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

#[derive(Clone)]
pub(crate) struct OutpointJob {
    client: Arc<ClientState>,
    txid: Hash32,
    vout: u32,
    force: bool,
}

pub(crate) type OutpointKey = (u64, Hash32, u32);

/// Bounded worker pool for outpoint status computation. Outpoint status may
/// need to resolve a raw funding/spending transaction, which can touch Core or
/// external providers; doing that in the notifier thread would delay every
/// other wallet's notifications.
pub(crate) struct OutpointNotifier {
    tx: mpsc::SyncSender<OutpointJob>,
    pending: Arc<Mutex<HashMap<OutpointKey, (OutpointJob, bool)>>>,
}

impl OutpointNotifier {
    fn new(shared: Arc<Shared>, workers: usize, queue: usize) -> Self {
        let (tx, rx) = mpsc::sync_channel::<OutpointJob>(queue.max(1));
        let rx = Arc::new(Mutex::new(rx));
        let pending = Arc::new(Mutex::new(HashMap::new()));
        for _ in 0..workers.max(1) {
            let shared = Arc::clone(&shared);
            let rx = Arc::clone(&rx);
            let pending = Arc::clone(&pending);
            std::thread::spawn(move || loop {
                let job = {
                    let guard = match rx.lock() {
                        Ok(g) => g,
                        Err(_) => return,
                    };
                    guard.recv()
                };
                let Ok(job) = job else {
                    return;
                };
                let key = (job.client.id, job.txid, job.vout);
                if let Ok(mut pending) = pending.lock() {
                    pending.remove(&key);
                }
                if !job.client.alive.load(Ordering::SeqCst) {
                    continue;
                }
                let sub = job
                    .client
                    .outpoints
                    .lock()
                    .ok()
                    .and_then(|subs| subs.get(&(job.txid, job.vout)).cloned());
                let Some(sub) = sub else {
                    continue;
                };
                let Ok(new) = outpoint_status(&shared, job.txid, job.vout, sub.script) else {
                    continue;
                };
                if new != sub.status || job.force {
                    let notification = json!({
                        "jsonrpc": "2.0",
                        "method": "blockchain.outpoint.subscribe",
                        "params": [job.txid.to_hex(), job.vout, new],
                    });
                    if job.client.notify(&notification) {
                        shared
                            .stats
                            .notifications_sent
                            .fetch_add(1, Ordering::Relaxed);
                        if let Ok(mut subs) = job.client.outpoints.lock() {
                            if let Some(entry) = subs.get_mut(&(job.txid, job.vout)) {
                                entry.status = new;
                            }
                        }
                    } else {
                        shared.stats.notifier_errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        }
        Self { tx, pending }
    }

    fn enqueue(&self, job: OutpointJob) {
        let key = (job.client.id, job.txid, job.vout);
        if let Ok(mut pending) = self.pending.lock() {
            if pending.contains_key(&key) {
                return;
            }
            pending.insert(key, (job, false));
        } else {
            return;
        }
        self.flush();
    }

    /// Retry jobs that did not fit in the bounded queue.
    fn flush(&self) {
        let mut pending = match self.pending.lock() {
            Ok(p) => p,
            Err(_) => return,
        };
        for (key, (job, queued)) in pending.iter_mut() {
            if *queued {
                continue;
            }
            if self.tx.try_send(job.clone()).is_ok() {
                *queued = true;
            } else {
                // Keep the job for the next loop.
                let _ = key;
                break;
            }
        }
    }
}

/// Push notifications for subscribed clients (headers and scripthash status).
/// Recomputed only when the tip or the mempool overlay changes.
pub fn start_notifications(shared: Arc<Shared>) -> std::thread::JoinHandle<()> {
    let outpoints = OutpointNotifier::new(Arc::clone(&shared), 2, 256);
    std::thread::spawn(move || {
        let mut last_height = u64::MAX;
        let mut last_hash: Option<String> = None;
        loop {
            if shared.shutdown.load(Ordering::Relaxed) {
                break;
            }
            shared.stats.notifier_runs.fetch_add(1, Ordering::Relaxed);
            let (height, hash, header) = {
                let Ok(_g) = shared.lock.read() else {
                    std::thread::sleep(Duration::from_secs(2));
                    continue;
                };
                let tip = shared.store.committed_tip().ok().flatten();
                match tip {
                    Some(t) => (
                        t.height as u64,
                        Some(t.block_hash.to_hex()),
                        shared
                            .store
                            .get_block(t.height)
                            .ok()
                            .flatten()
                            .map(|r| hex::encode(r.header)),
                    ),
                    None => (0, None, None),
                }
            };
            let new_block = height != last_height || hash != last_hash;
            let (dirty, force_all) = shared.dirty_scripts.take();
            // A rewind or a same-height fork can change scripts that are not in
            // the new block's dirty set, so recompute every subscription.
            let reorg =
                (new_block && height < last_height) || (height == last_height && hash != last_hash);
            let scan_scripts = force_all || reorg || !dirty.is_empty();
            // Retry outpoint jobs that did not fit in the queue, even when no
            // new block arrived.
            outpoints.flush();
            if !new_block && !scan_scripts {
                notifier_pause(&shared);
                continue;
            }
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
                        if client.notify(&notification) {
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
                if !scan_scripts {
                    continue;
                }
                // Scripthash and scriptpubkey subscriptions are independent:
                // a client may use both for the same script and must receive
                // one notification per method.
                let (sh_subs, spk_subs) = {
                    let sh = client
                        .subs
                        .lock()
                        .map(|s| s.iter().map(|(k, v)| (*k, v.clone())).collect::<Vec<_>>())
                        .unwrap_or_default();
                    let spk = client
                        .spk_subs
                        .lock()
                        .map(|s| s.iter().map(|(k, v)| (*k, v.clone())).collect::<Vec<_>>())
                        .unwrap_or_default();
                    (sh, spk)
                };
                for (script, old, method, force_reorg) in sh_subs
                    .into_iter()
                    .map(|(s, o)| (s, o, "blockchain.scripthash.subscribe", false))
                    .chain(
                        spk_subs
                            .into_iter()
                            .map(|(s, o)| (s, o, "blockchain.scriptpubkey.subscribe", reorg)),
                    )
                {
                    if !(force_all || reorg || dirty.contains(&script)) {
                        continue;
                    }
                    let new = {
                        let Ok(_g) = shared.lock.read() else {
                            continue;
                        };
                        match script_status(&shared, script) {
                            Ok(v) => v,
                            Err(_) => continue,
                        }
                    };
                    if new != old || force_reorg {
                        let notification = json!({
                            "jsonrpc": "2.0",
                            "method": method,
                            "params": [script.to_electrum_hex(), new],
                        });
                        if client.notify(&notification) {
                            shared
                                .stats
                                .notifications_sent
                                .fetch_add(1, Ordering::Relaxed);
                            let store = if method == "blockchain.scripthash.subscribe" {
                                &client.subs
                            } else {
                                &client.spk_subs
                            };
                            if let Ok(mut subs) = store.lock() {
                                subs.insert(script, new);
                            }
                        } else {
                            shared.stats.notifier_errors.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }

                let outpoint_subs: Vec<((Hash32, u32), OutpointSub)> = client
                    .outpoints
                    .lock()
                    .map(|s| s.iter().map(|(k, v)| (*k, v.clone())).collect())
                    .unwrap_or_default();
                for ((txid, vout), sub) in outpoint_subs {
                    if !(force_all || reorg || dirty.contains(&sub.script)) {
                        continue;
                    }
                    // Status resolution may need Core or an external provider;
                    // keep it off the notifier thread.
                    outpoints.enqueue(OutpointJob {
                        client: Arc::clone(&client),
                        txid,
                        vout,
                        force: reorg,
                    });
                }
            }
            last_height = height;
            last_hash = hash;
            notifier_pause(&shared);
        }
    })
}

/// Sleep until the indexer or the mempool marks a script (a commit always
/// does), at most 2 s; then coalesce briefly so a burst of marks is handled
/// in one pass. Notifications used to wait up to a fixed 2 s.
pub(crate) fn notifier_pause(shared: &Shared) {
    shared.dirty_scripts.wait(Duration::from_secs(2));
    std::thread::sleep(Duration::from_millis(100));
}

// ------------------------------------------------------------------- transports
