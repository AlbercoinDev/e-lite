//! Downloading a whole block from the external sources: one shared
//! download per block (leader and followers), the race over shuffled
//! endpoints with hedged attempts, and the memory of recent failures.

use super::*;

/// When the leader of a block download gives up if it never gets a slot:
/// the slot wait (`EXTERNAL_QUEUE_MAX`) plus the network budget, or the
/// request's own deadline when the request already spent time on the per-txid
/// path. The network budget is never shortened: the slot wait must not eat it.
fn block_path_deadline(now: Instant, request_deadline: Option<Instant>) -> Instant {
    let full = now + EXTERNAL_QUEUE_MAX + EXTERNAL_BUDGET;
    match request_deadline {
        // A wallet request has a total budget; keep at least the network
        // part for the download itself and give the rest to the queue.
        Some(limit) => limit.min(full).max(now + WALLET_BLOCK_NET),
        None => full,
    }
}

/// A block download shared by concurrent callers asking for the same height.
pub(super) struct InflightBlock {
    done: Mutex<bool>,
    /// The leader's block, already verified against the indexed header:
    /// followers reuse it instead of verifying their own copy.
    result: Mutex<Option<Result<Arc<bitcoin::Block>>>>,
    cv: Condvar,
    /// The leader's deadline: queue limit plus budget when it registers,
    /// then exactly `EXTERNAL_BUDGET` from the moment it gets a download
    /// slot. Followers wait until the current `deadline + FOLLOWER_GRACE`.
    deadline: Mutex<Instant>,
    /// A wallet request is waiting on this download: a prefetch leader
    /// switches to wallet priority (AUD-165).
    wallet_waiting: AtomicBool,
}

impl InflightBlock {
    fn new(deadline: Instant, wallet: bool) -> Self {
        Self {
            done: Mutex::new(false),
            result: Mutex::new(None),
            cv: Condvar::new(),
            deadline: Mutex::new(deadline),
            wallet_waiting: AtomicBool::new(wallet),
        }
    }

    fn deadline(&self) -> Instant {
        *self
            .deadline
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn set_deadline(&self, deadline: Instant) {
        *self
            .deadline
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = deadline;
    }

    /// Wait up to `timeout` for this download to finish. `None` when it is
    /// still running; the caller then races the per-txid path against it.
    pub(super) fn wait_outcome(&self, timeout: Duration) -> Option<Result<Arc<bitcoin::Block>>> {
        let deadline = Instant::now() + timeout;
        let mut done = self.done.lock().ok()?;
        while !*done {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            let (guard, _) = self
                .cv
                .wait_timeout(done, left.min(Duration::from_millis(50)))
                .ok()?;
            done = guard;
        }
        drop(done);
        let result = self.result.lock().ok()?.clone();
        Some(result.unwrap_or_else(|| Err(RawtxError::Unavailable("inflight result lost".into()))))
    }
}

impl Resolver {
    /// Fetch one block from the external sources, deduplicating concurrent
    /// requests for the same hash. The first caller downloads; the rest wait
    /// for its result instead of hammering the same provider.
    /// Returns the verified block and whether this caller led the download
    /// (only the leader owes decoys for it).
    pub(super) fn fetch_external_block(
        &self,
        rec: &el_model::BlockRecord,
        priority: Priority,
        request_deadline: Option<Instant>,
    ) -> Result<(Arc<bitcoin::Block>, bool)> {
        let hash = &rec.hash;
        if let Some(cache) = &self.cache {
            if let Some(bytes) = cache.get(hash) {
                if let Some(block) = self.recent_block(hash) {
                    return Ok((block, false));
                }
                return self
                    .verify_external(&bytes, rec)
                    .map(|block| (block, false));
            }
        }
        let (entry, leader) = {
            let mut map = self
                .inflight
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match map.get(hash) {
                Some(existing) => {
                    if priority == Priority::Wallet {
                        existing.wallet_waiting.store(true, Ordering::Relaxed);
                    }
                    (Arc::clone(existing), false)
                }
                None => {
                    let entry = Arc::new(InflightBlock::new(
                        block_path_deadline(Instant::now(), request_deadline),
                        priority == Priority::Wallet,
                    ));
                    map.insert(*hash, Arc::clone(&entry));
                    (entry, true)
                }
            }
        };

        if leader {
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                self.fetch_from_external_sources(hash, &entry, priority)
            }))
            .unwrap_or_else(|_| {
                Err(RawtxError::Unavailable(
                    "external block fetch panicked".into(),
                ))
            });
            // Only a wallet's failure is remembered (AUD-165): a prefetch
            // that waited for a slot, or failed in the background, must not
            // answer a later wallet request with its error (Sparrow stores
            // it as an unfetchable transaction, AUD-045).
            if let Err(e) = &result {
                if entry.wallet_waiting.load(Ordering::Relaxed) && !is_queue_timeout(e) {
                    self.remember_failure(hash, e);
                }
            }
            // Verified before the followers wake up, so the block is
            // checked once per download and not once per request.
            let result = result.and_then(|bytes| self.verify_external(&bytes, rec));
            *entry
                .result
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(result.clone());
            *entry
                .done
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
            entry.cv.notify_all();
            self.inflight
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(hash);
            return result.map(|block| (block, true));
        }

        // The leader gives up at its own deadline; a follower waits a little
        // longer so it always receives the leader's result (AUD-156). The
        // deadline moves once the leader gets a slot, so it is re-read.
        let mut done = entry
            .done
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while !*done {
            let now = Instant::now();
            let deadline = entry.deadline() + FOLLOWER_GRACE;
            if now >= deadline {
                return Err(RawtxError::Unavailable(
                    "inflight block download timed out".into(),
                ));
            }
            let wait = deadline
                .saturating_duration_since(now)
                .min(Duration::from_secs(1));
            let (guard, _) = entry
                .cv
                .wait_timeout(done, wait)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            done = guard;
        }
        drop(done);
        let result = entry
            .result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        result
            .unwrap_or_else(|| Err(RawtxError::Unavailable("inflight result lost".into())))
            .map(|block| (block, false))
    }

    /// Race the endpoints of every external source for one block (AUD-160).
    ///
    /// Endpoints are shuffled (no provider or peer is preferred, AUD-135)
    /// and tried with at most `RACE_WIDTH` attempts at once. A failure
    /// starts the next endpoint at once; a silent or slow attempt is hedged
    /// without being cut, so a slow but live circuit can still win. The
    /// first verified block wins and every other attempt is aborted. The
    /// clearnet fallback tier starts only when no primary attempt is left.
    fn fetch_from_external_sources(
        &self,
        hash: &Hash32,
        entry: &InflightBlock,
        priority: Priority,
    ) -> Result<Vec<u8>> {
        // A wallet request may take the last free slot; the prefetch leaves
        // it free and steps aside while any wallet request waits for a slot.
        // A prefetch that a wallet joined becomes a wallet download (AUD-165).
        let wallet = priority == Priority::Wallet || entry.wallet_waiting.load(Ordering::Relaxed);
        // The queue gets whatever the request budget leaves after the network
        // part (~25 s of queue and 20 s of network for a wallet request, 45 s
        // total); the full 120 s for a background prefetch. The governor caps
        // every Tor stream (block downloads, per-txid sessions, decoys) with
        // wallet > prefetch > decoy priority, so a cold burst waits for a
        // slot instead of saturating Tor.
        let budget = if wallet {
            WALLET_BLOCK_NET
        } else {
            EXTERNAL_BUDGET
        };
        let lane = if wallet { Lane::Wallet } else { Lane::Prefetch };
        let queue_deadline = entry
            .deadline()
            .checked_sub(budget)
            .unwrap_or_else(Instant::now);
        let Some(_permit) = self
            .governor
            .acquire(lane, queue_deadline.max(Instant::now() + MIN_ATTEMPT))
        else {
            self.stats.unavailable.fetch_add(1, Ordering::Relaxed);
            return Err(RawtxError::Unavailable(
                "every download slot stayed busy until the deadline".into(),
            ));
        };
        // The network budget starts now, not when the request queued.
        let deadline = Instant::now() + budget;
        entry.set_deadline(deadline);
        let mut plan: Vec<(Arc<dyn BlockSource>, Endpoint)> = Vec::new();
        for source in self.sources() {
            for endpoint in source.endpoints(FetchMode::Normal) {
                plan.push((Arc::clone(&source), endpoint));
            }
        }
        plan.shuffle(&mut thread_rng());
        // Stable: random order inside each tier, primaries first.
        plan.sort_by_key(|(_, endpoint)| endpoint.tier);
        if plan.is_empty() {
            self.stats.unavailable.fetch_add(1, Ordering::Relaxed);
            return Err(RawtxError::Unavailable(
                "every external endpoint is cooling down".into(),
            ));
        }
        let wallet = priority == Priority::Wallet || entry.wallet_waiting.load(Ordering::Relaxed);
        let _wallet = wallet.then(|| Busy::new(&self.wallet_downloads));
        let result = self.race(hash, deadline, plan, wallet);
        if result.is_ok() {
            // A success means Tor is healthy: lift any pause at once.
            self.governor.healthy();
        } else {
            self.stats.unavailable.fetch_add(1, Ordering::Relaxed);
        }
        result
    }

    fn race(
        &self,
        hash: &Hash32,
        deadline: Instant,
        plan: Vec<(Arc<dyn BlockSource>, Endpoint)>,
        wallet: bool,
    ) -> Result<Vec<u8>> {
        struct Running {
            ctl: AttemptCtl,
            started: Instant,
            via_tor: bool,
            _permit: Permit,
        }
        let (tx, rx) = mpsc::channel::<(usize, Result<Vec<u8>>)>();
        let race_started = Instant::now();
        let mut exhausted_since: Option<Instant> = None;
        // Attempts started per endpoint in this race.
        let mut tried: HashMap<String, u32> = HashMap::new();
        let mut running: HashMap<usize, Running> = HashMap::new();
        let mut errors: Vec<String> = Vec::new();
        let mut next = 0usize;
        let mut plan = plan;
        let mut retried: HashMap<String, u32> = HashMap::new();

        let finish = |running: &mut HashMap<usize, Running>| {
            for r in running.values() {
                r.ctl.abort();
            }
            running.clear();
        };

        loop {
            let now = Instant::now();
            if now + MIN_ATTEMPT.min(Duration::from_millis(500)) >= deadline {
                errors.push("external fetch deadline reached".to_string());
                finish(&mut running);
                break;
            }
            // Start attempts: the first one (waiting for an attempt permit if
            // needed), the next after a failure, or a hedge.
            let want_more = if running.is_empty() {
                true
            } else if running.len() >= RACE_WIDTH {
                false
            } else {
                running
                    .values()
                    .max_by_key(|r| r.started)
                    .is_some_and(|r| should_hedge(r.ctl.bytes(), now - r.started, r.via_tor))
            };
            let attempts: u32 = tried.values().sum();
            if want_more && next < plan.len() && (attempts as usize) < MAX_RACE_ATTEMPTS {
                let fallback = plan[next].1.tier > 0;
                // The fallback tier waits for every primary to be done
                // (none running, none queued after it).
                let allowed = !fallback
                    || (running.is_empty() && plan[next..].iter().all(|(_, e)| e.tier > 0));
                if allowed {
                    // Waiting for an attempt permit must not outlive the
                    // network budget: `queue_deadline` covers the download
                    // slot only (AUD-167). A prefetch or decoy leaves one
                    // permit free so a wallet never queues behind them.
                    let keep_free = usize::from(!wallet);
                    let wait_until = if running.is_empty() { deadline } else { now };
                    if let Some(permit) =
                        self.external_attempts.acquire_until(wait_until, keep_free)
                    {
                        self.stats.race_attempts.fetch_add(1, Ordering::Relaxed);
                        if !running.is_empty() {
                            self.stats.race_hedges.fetch_add(1, Ordering::Relaxed);
                        }
                        let (source, endpoint) = plan[next].clone();
                        *tried.entry(endpoint.key.clone()).or_insert(0) += 1;
                        let ctl = AttemptCtl::new();
                        running.insert(
                            next,
                            Running {
                                ctl: ctl.clone(),
                                started: Instant::now(),
                                via_tor: endpoint.via_tor,
                                _permit: permit,
                            },
                        );
                        self.spawn_attempt(
                            next,
                            source,
                            endpoint,
                            *hash,
                            deadline,
                            ctl,
                            tx.clone(),
                        );
                        next += 1;
                        continue;
                    } else if running.is_empty() {
                        errors.push("every attempt slot stayed busy".to_string());
                        break;
                    }
                }
            }
            if running.is_empty() && next >= plan.len() {
                // Every endpoint failed or is cooling down. While budget
                // remains, wait a little for one to come back instead of
                // failing the wallet (whose client may store the error,
                // AUD-045). Quiet: a rate-limited provider is not forced
                // early; an endpoint already tried here does not return
                // (a Tor one keeps its single retry on a new circuit).
                let since = *exhausted_since.get_or_insert_with(Instant::now);
                if attempts as usize >= MAX_RACE_ATTEMPTS
                    || since.elapsed() >= RACE_REFILL_WAIT
                    || deadline.saturating_duration_since(Instant::now()) < MIN_ATTEMPT * 3
                {
                    break;
                }
                std::thread::sleep(Duration::from_secs(1));
                let mut fresh: Vec<(Arc<dyn BlockSource>, Endpoint)> = Vec::new();
                for source in self.sources() {
                    for endpoint in source.endpoints(FetchMode::Quiet) {
                        let tries = tried.get(&endpoint.key).copied().unwrap_or(0);
                        let allowed =
                            tries == 0 || (endpoint.via_tor && tries <= TOR_RETRIES_PER_ENDPOINT);
                        if allowed && !fresh.iter().any(|(_, e)| e.key == endpoint.key) {
                            fresh.push((Arc::clone(&source), endpoint));
                        }
                    }
                }
                if !fresh.is_empty() {
                    exhausted_since = None;
                }
                fresh.shuffle(&mut thread_rng());
                fresh.sort_by_key(|(_, endpoint)| endpoint.tier);
                plan.extend(fresh);
                continue;
            }
            if running.is_empty() {
                // Nothing running and nothing startable now.
                break;
            }
            let wait = RACE_TICK.min(deadline.saturating_duration_since(Instant::now()));
            match rx.recv_timeout(wait) {
                Ok((idx, Ok(bytes))) => {
                    running.remove(&idx);
                    finish(&mut running);
                    let wins = match plan[idx].0.name() {
                        "esplora" => &self.stats.race_wins_esplora,
                        "p2p" => &self.stats.race_wins_p2p,
                        _ => &self.stats.race_wins_other,
                    };
                    wins.fetch_add(1, Ordering::Relaxed);
                    self.stats
                        .race_win_ms
                        .fetch_add(race_started.elapsed().as_millis() as u64, Ordering::Relaxed);
                    self.stats.fetches.fetch_add(1, Ordering::Relaxed);
                    self.stats
                        .bytes
                        .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                    return Ok(bytes);
                }
                Ok((idx, Err(e))) => {
                    if let Some(ended) = running.remove(&idx) {
                        let (source, endpoint) = plan[idx].clone();
                        errors.push(format!("{}: {e}", source.name()));
                        // Over Tor the next attempt is a new circuit (new exit):
                        // after a failure a new circuit may fix, the endpoint
                        // gets one more try in this race, at the end of the
                        // queue. The attempt says so itself: by now the
                        // endpoint is cooling down and no longer listed, so
                        // asking the source would always say no.
                        let tries = retried.entry(endpoint.key.clone()).or_insert(0);
                        if endpoint.via_tor
                            && *tries < TOR_RETRIES_PER_ENDPOINT
                            && ended.ctl.wants_retry()
                        {
                            *tries += 1;
                            // Before the fallback tier, which waits for it.
                            let at = (next..plan.len())
                                .find(|&i| plan[i].1.tier > endpoint.tier)
                                .unwrap_or(plan.len());
                            plan.insert(at, (source, endpoint));
                        }
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        Err(RawtxError::Unavailable(if errors.is_empty() {
            "no external attempt could be made".to_string()
        } else {
            errors.join("; ")
        }))
    }

    /// One attempt in its own thread: fetch from one endpoint, verify, report.
    #[allow(clippy::too_many_arguments)]
    fn spawn_attempt(
        &self,
        idx: usize,
        source: Arc<dyn BlockSource>,
        endpoint: Endpoint,
        hash: Hash32,
        deadline: Instant,
        ctl: AttemptCtl,
        tx: mpsc::Sender<(usize, Result<Vec<u8>>)>,
    ) {
        let stats = Arc::clone(&self.stats);
        let spawned = std::thread::Builder::new()
            .name(el_model::sync::THREAD_FETCH.to_string())
            .spawn(move || {
                let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    source.fetch_endpoint(&endpoint, &hash, deadline, FetchMode::Normal, &ctl)
                }))
                .unwrap_or_else(|_| Err(RawtxError::Unavailable("attempt panicked".into())));
                let result = result.and_then(|bytes| {
                    match verify_block_by_hash(&bytes, &hash).and_then(|block| {
                        verify_witness_commitment(&block)?;
                        Ok(block)
                    }) {
                        // Not written to the block cache: the caller keeps the
                        // transactions it needs (AUD-139).
                        Ok(_) => Ok(bytes),
                        Err(e) => {
                            stats.verify_failures.fetch_add(1, Ordering::Relaxed);
                            eprintln!(
                                "[e-lite] {} returned an invalid block ({})",
                                source.name(),
                                e.kind()
                            );
                            Err(e)
                        }
                    }
                });
                let _ = tx.send((idx, result));
            });
        if let Err(e) = spawned {
            eprintln!("[e-lite] fetch thread did not start: {e}");
        }
    }

    /// A failure of `hash` younger than `FAILURE_MEMORY`, if any.
    pub(super) fn recent_failure(&self, hash: &Hash32) -> Option<RawtxError> {
        let mut failed = self.failed.lock().ok()?;
        let now = Instant::now();
        failed.retain(|_, (at, _)| now.duration_since(*at) < FAILURE_MEMORY);
        failed.get(hash).map(|(_, e)| e.clone())
    }

    /// Remember that every source failed for `hash`. "No source" is not
    /// remembered: it is a configuration state, not a download failure.
    fn remember_failure(&self, hash: &Hash32, e: &RawtxError) {
        if !matches!(e, RawtxError::Unavailable(_)) {
            return;
        }
        if let Ok(mut failed) = self.failed.lock() {
            let now = Instant::now();
            failed.retain(|_, (at, _)| now.duration_since(*at) < FAILURE_MEMORY);
            failed.insert(*hash, (now, e.clone()));
        }
    }
}

/// A download that never got a download slot (local queueing, not a source
/// failure).
fn is_queue_timeout(e: &RawtxError) -> bool {
    matches!(
        e,
        RawtxError::Unavailable(m)
            if m.contains("download slot stayed busy") || m.contains("attempt slot stayed busy")
    )
}

/// Whether the newest attempt of a race deserves a parallel one: nothing
/// received after the first-byte limit, or still running at the slow limit.
fn should_hedge(bytes: u64, elapsed: Duration, via_tor: bool) -> bool {
    let (first_byte, rate_after, min_rate, slow) = if via_tor {
        (
            HEDGE_FIRST_BYTE_TOR,
            HEDGE_RATE_AFTER_TOR,
            HEDGE_MIN_RATE_TOR,
            HEDGE_SLOW_TOR,
        )
    } else {
        (
            HEDGE_FIRST_BYTE_DIRECT,
            HEDGE_RATE_AFTER_DIRECT,
            HEDGE_MIN_RATE_DIRECT,
            HEDGE_SLOW_DIRECT,
        )
    };
    let rate = bytes as f64 / elapsed.as_secs_f64().max(0.001);
    (bytes == 0 && elapsed >= first_byte)
        || (elapsed >= rate_after && rate < min_rate as f64)
        || elapsed >= slow
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AUD-187: a request that already spent time on the per-txid path
    /// gives the block path what is left of its one deadline, and the
    /// network budget is never shortened.
    #[test]
    fn the_block_path_inherits_the_request_deadline() {
        let now = Instant::now();
        let full = now + EXTERNAL_QUEUE_MAX + EXTERNAL_BUDGET;
        assert_eq!(block_path_deadline(now, None), full);
        // Fresh request: the whole wait.
        assert_eq!(block_path_deadline(now, Some(full)), full);
        // Five seconds of the request went elsewhere: the block path gets the
        // rest of its one deadline, not a new full wait.
        let left = now + EXTERNAL_BUDGET + Duration::from_secs(1);
        assert_eq!(block_path_deadline(now, Some(left)), left);
        // Never below the network part, never above the full wait.
        assert_eq!(
            block_path_deadline(now, Some(now + Duration::from_secs(1))),
            now + WALLET_BLOCK_NET
        );
        assert_eq!(
            block_path_deadline(now, Some(full + Duration::from_secs(600))),
            full
        );
    }

    #[test]
    fn a_silent_or_slow_attempt_is_hedged() {
        let s = Duration::from_secs;
        assert!(!should_hedge(0, s(1), true));
        assert!(should_hedge(0, HEDGE_FIRST_BYTE_TOR, true));
        let fast_tor = 300 * 1024 * 10; // 300 KB/s for 10 s
        assert!(
            !should_hedge(fast_tor, s(10), true),
            "a good transfer is not hedged"
        );
        assert!(
            should_hedge(60 * 1024 * 6, s(6), true),
            "a 60 KB/s circuit is"
        );
        assert!(!should_hedge(10, s(4), true), "too early to judge the rate");
        assert!(should_hedge(fast_tor, HEDGE_SLOW_TOR, true));
        assert!(should_hedge(0, HEDGE_FIRST_BYTE_DIRECT, false));
        assert!(!should_hedge(10, s(1), false));
        assert!(should_hedge(10, s(3), false), "a crawling direct transfer");
        assert!(should_hedge(10 << 20, HEDGE_SLOW_DIRECT, false));
    }
}
