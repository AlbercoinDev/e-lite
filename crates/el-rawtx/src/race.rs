//! Downloading a whole block from the external sources: one shared
//! download per block (leader and followers), the race over shuffled
//! endpoints with hedged attempts, and the memory of recent failures.

use super::*;
use crate::governor::Availability;
use crate::qualification::{block_fast, Route, Sample};
use std::sync::atomic::AtomicUsize;

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
    result: Mutex<Option<Result<Arc<bitcoin::Block>>>>,
    deadline: Instant,
    interested: AtomicUsize,
    wallet_waiting: AtomicBool,
    exhausted: AtomicBool,
}

impl InflightBlock {
    fn new(deadline: Instant, wallet: bool) -> Self {
        Self {
            result: Mutex::new(None),
            deadline,
            interested: AtomicUsize::new(1),
            wallet_waiting: AtomicBool::new(wallet),
            exhausted: AtomicBool::new(false),
        }
    }

    fn wanted(&self) -> bool {
        self.interested.load(Ordering::Acquire) != 0
    }

    fn join(&self) -> bool {
        let mut n = self.interested.load(Ordering::Acquire);
        while n != 0 {
            match self.interested.compare_exchange_weak(
                n,
                n + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(now) => n = now,
            }
        }
        false
    }
}

pub(super) struct BlockLease {
    job: Arc<InflightBlock>,
    event: Arc<Availability>,
    governor: Arc<TorGovernor>,
    pub(super) joined_existing: bool,
}

impl BlockLease {
    pub(super) fn result(&self) -> Option<Result<Arc<bitcoin::Block>>> {
        self.job
            .result
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

impl Drop for BlockLease {
    fn drop(&mut self) {
        if self.job.interested.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.governor.wake_waiters();
            self.event.notify();
        }
    }
}

impl Resolver {
    /// Join first, otherwise start a job only when the block lane has
    /// capacity. No request owns the job: any waiter may return early.
    pub(super) fn join_block(&self, hash: &Hash32, priority: Priority) -> Option<BlockLease> {
        let map = self.inflight.lock().unwrap_or_else(|p| p.into_inner());
        let job = map.get(hash)?;
        if !job.join() {
            return None;
        }
        if priority == Priority::Wallet {
            self.promote(job);
        }
        Some(self.block_lease(Arc::clone(job), true))
    }

    fn block_lease(&self, job: Arc<InflightBlock>, joined_existing: bool) -> BlockLease {
        BlockLease {
            job,
            event: Arc::clone(&self.recovery_event),
            governor: Arc::clone(&self.governor),
            joined_existing,
        }
    }

    fn promote(&self, job: &InflightBlock) {
        if !job.wallet_waiting.swap(true, Ordering::AcqRel) {
            self.governor.wake_waiters();
        }
    }

    pub(super) fn start_block(
        &self,
        rec: &el_model::BlockRecord,
        priority: Priority,
        request_deadline: Option<Instant>,
    ) -> Result<Option<BlockLease>> {
        self.start_provider_probes();
        let mut map = self.inflight.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(job) = map.get(&rec.hash) {
            if job.join() {
                if priority == Priority::Wallet {
                    self.promote(job);
                }
                return Ok(Some(self.block_lease(Arc::clone(job), true)));
            }
            // The last waiter left. Let the canceled job drain and publish
            // any genuine source exhaustion before starting another race.
            return Ok(None);
        }
        if let Some(block) = self.recent_block(&rec.hash) {
            let job = Arc::new(InflightBlock::new(Instant::now(), false));
            *job.result.lock().unwrap_or_else(|p| p.into_inner()) = Some(Ok(block));
            return Ok(Some(self.block_lease(job, false)));
        }
        if self.sources().is_empty() {
            if self.p2p.is_some() && priority == Priority::Wallet {
                return Ok(None); // discovery may still publish a peer
            }
            self.stats.unavailable.fetch_add(1, Ordering::Relaxed);
            return Err(RawtxError::NoSource(
                "no external source configured (or Tor is required and unavailable)".into(),
            ));
        }
        if self.block_endpoints(FetchMode::Quiet).is_empty() {
            return Ok(None); // a probe will wake wallet waiters; no slot used
        }
        if let Some(e) = self.recent_failure(&rec.hash) {
            return Err(e);
        }
        let Some(permit) = self
            .block_jobs
            .try_acquire(usize::from(priority != Priority::Wallet))
        else {
            return Ok(None);
        };
        let job = Arc::new(InflightBlock::new(
            block_path_deadline(Instant::now(), request_deadline),
            priority == Priority::Wallet,
        ));
        let lease = self.block_lease(Arc::clone(&job), false);
        let runner = self.worker_handle();
        let rec = *rec;
        map.insert(rec.hash, Arc::clone(&job));
        let started = std::thread::Builder::new()
            .name(el_model::sync::THREAD_FETCH.into())
            .spawn(move || runner.run_block_job(rec, job, permit));
        if started.is_err() {
            map.remove(&rec.hash);
            self.recovery_event.notify();
            return Err(RawtxError::Unavailable("could not start block job".into()));
        }
        Ok(Some(lease))
    }

    fn run_block_job(&self, rec: el_model::BlockRecord, job: Arc<InflightBlock>, permit: Permit) {
        let work = std::panic::catch_unwind(AssertUnwindSafe(|| {
            self.fetch_from_external_sources(&rec, &job)
        }))
        .unwrap_or_else(|_| {
            Err(RawtxError::Unavailable(
                "external block fetch panicked".into(),
            ))
        });
        let result = work.map(|block| {
            self.stats.verifications.fetch_add(1, Ordering::Relaxed);
            let block = self.remember(rec.hash, block);
            self.owe_decoys(rec.hash);
            block
        });
        if let Err(e) = &result {
            if (job.wanted() || job.exhausted.load(Ordering::Acquire))
                && job.wallet_waiting.load(Ordering::Acquire)
                && !is_queue_timeout(e)
            {
                self.remember_failure(&rec.hash, e);
            }
            if job.wanted() && job.wallet_waiting.load(Ordering::Acquire) && !is_queue_timeout(e) {
                let height = rec.height;
                eprintln!(
                    "[e-lite] rawtx at height {height} failed ({}); sources: {}",
                    e.kind(),
                    redact_hashes(&e.to_string())
                );
            }
        }
        *job.result.lock().unwrap_or_else(|p| p.into_inner()) = Some(result);
        {
            let mut map = self.inflight.lock().unwrap_or_else(|p| p.into_inner());
            if map
                .get(&rec.hash)
                .is_some_and(|other| Arc::ptr_eq(other, &job))
            {
                map.remove(&rec.hash);
            }
        }
        drop(permit);
        self.recovery_event.notify();
    }

    fn block_endpoints(&self, mode: FetchMode) -> Vec<(Arc<dyn BlockSource>, Endpoint)> {
        let mut plan = Vec::new();
        for source in self.sources() {
            let endpoints = if source.requires_qualification() {
                source.probe_endpoints()
            } else {
                source.endpoints(mode)
            };
            for endpoint in endpoints {
                if !source.requires_qualification()
                    || self
                        .qualification
                        .admit(&Route::block(source.as_ref(), &endpoint))
                        .is_some()
                {
                    plan.push((Arc::clone(&source), endpoint));
                }
            }
        }
        plan
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
        rec: &el_model::BlockRecord,
        entry: &InflightBlock,
    ) -> Result<bitcoin::Block> {
        let hash = &rec.hash;
        // A wallet request may take the last free slot; the prefetch leaves
        // it free and steps aside while any wallet request waits for a slot.
        // A prefetch that a wallet joined becomes a wallet download (AUD-165).
        let is_wallet = || entry.wallet_waiting.load(Ordering::Acquire);
        // The queue gets whatever the request budget leaves after the network
        // part (~25 s of queue and 20 s of network for a wallet request, 45 s
        // total); the full 120 s for a background prefetch. The governor caps
        // every Tor **stream** (each race attempt, each per-txid connection)
        // with wallet > prefetch > decoy priority, so a cold burst waits for
        // a stream instead of saturating Tor (29.25.2: one permit per
        // attempt, not per block).
        let mut plan = self.block_endpoints(FetchMode::Normal);
        plan.shuffle(&mut thread_rng());
        // Stable: random order inside each tier, primaries first.
        plan.sort_by_key(|(_, endpoint)| endpoint.tier);
        if plan.is_empty() {
            self.stats.unavailable.fetch_add(1, Ordering::Relaxed);
            return Err(RawtxError::Unavailable(format!(
                "no qualified block route: {LOCAL_BUSY}"
            )));
        }
        // The first stream is the queue: it is waited for with what the
        // request leaves after the network budget.
        let queue_started = Instant::now();
        let (wallet, lane, budget, first) = loop {
            let wallet = is_wallet();
            let lane = if wallet { Lane::Wallet } else { Lane::Prefetch };
            let budget = if wallet {
                WALLET_BLOCK_NET
            } else {
                EXTERNAL_BUDGET
            };
            if !plan[0].1.via_tor {
                break (wallet, lane, budget, None);
            }
            let queue_deadline = entry
                .deadline
                .checked_sub(budget)
                .unwrap_or_else(Instant::now);
            let Some(permit) = self.governor.acquire_while(
                lane,
                Demand::Block,
                queue_deadline.max(Instant::now() + MIN_ATTEMPT),
                || entry.wanted() && wallet == is_wallet(),
            ) else {
                if entry.wanted() && wallet != is_wallet() {
                    continue;
                }
                self.stats.unavailable.fetch_add(1, Ordering::Relaxed);
                if debug_logs() {
                    el_model::debug_event!(
                        "block obj={} lane={} no tor stream after {} ms (streams {}/{})",
                        log_tag(hash),
                        lane.name(),
                        queue_started.elapsed().as_millis(),
                        self.governor.in_use(),
                        self.governor.total()
                    );
                }
                return Err(RawtxError::Unavailable(
                    "every download slot stayed busy until the deadline".into(),
                ));
            };
            break (wallet, lane, budget, Some(permit));
        };
        let queued_ms = queue_started.elapsed().as_millis();
        // The network budget starts now, not when the request queued.
        let deadline = Instant::now() + budget;
        let _wallet = wallet.then(|| Busy::new(&self.wallet_downloads));
        let started = Instant::now();
        let result = self.race(rec, deadline, plan, entry, first);
        if result.is_err() && entry.wanted() {
            self.stats.unavailable.fetch_add(1, Ordering::Relaxed);
        }
        if debug_logs() {
            el_model::debug_event!(
                "block obj={} lane={} queued {queued_ms} ms, race {} ms: {}",
                log_tag(hash),
                lane.name(),
                started.elapsed().as_millis(),
                match &result {
                    Ok(_) => "ok (verified)".into(),
                    Err(e) => format!("failed ({})", redact_hashes(&e.to_string())),
                }
            );
        }
        result
    }

    fn race(
        &self,
        rec: &el_model::BlockRecord,
        deadline: Instant,
        plan: Vec<(Arc<dyn BlockSource>, Endpoint)>,
        job: &InflightBlock,
        first: Option<GovernorPermit>,
    ) -> Result<bitcoin::Block> {
        // The stream the caller queued for, used by the first Tor attempt.
        let mut first = first;
        struct Running {
            ctl: AttemptCtl,
            started: Instant,
            via_tor: bool,
        }
        let (tx, rx) = mpsc::channel::<AttemptResult>();
        let race_started = Instant::now();
        let mut exhausted_since: Option<Instant> = None;
        // Attempts started per endpoint in this race.
        let mut tried: HashMap<Route, u32> = HashMap::new();
        let mut running: HashMap<usize, Running> = HashMap::new();
        let mut errors: Vec<String> = Vec::new();
        let mut next = 0usize;
        let mut plan = plan;
        let mut retried: HashMap<Route, u32> = HashMap::new();

        let finish = |running: &mut HashMap<usize, Running>| {
            for r in running.values() {
                r.ctl.abort();
            }
            running.clear();
        };

        loop {
            let now = Instant::now();
            if !job.wanted() || now >= deadline {
                if !job.exhausted.load(Ordering::Acquire) || errors.is_empty() {
                    errors.push("external fetch deadline reached".to_string());
                }
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
                let (source, endpoint) = &plan[next];
                if source.requires_qualification()
                    && self
                        .qualification
                        .admit(&Route::block(source.as_ref(), endpoint))
                        .is_none()
                {
                    self.qualification.skipped();
                    next += 1;
                    continue;
                }
                let fallback = plan[next].1.tier > 0;
                // The fallback tier waits for every primary to be done
                // (none running, none queued after it).
                let allowed = !fallback
                    || (running.is_empty() && plan[next..].iter().all(|(_, e)| e.tier > 0));
                if allowed {
                    let wallet = job.wallet_waiting.load(Ordering::Acquire);
                    let lane = if wallet { Lane::Wallet } else { Lane::Prefetch };
                    let role = if running.is_empty() {
                        if attempts == 0 {
                            "first"
                        } else {
                            "retry"
                        }
                    } else {
                        "hedge"
                    };
                    // One Tor stream per attempt (29.25.2). A hedge never
                    // waits for one: the attempt it hedges is still running.
                    // With nothing running, wait while an attempt still fits
                    // in the network budget.
                    let stream_wait = Instant::now();
                    let stream = if !plan[next].1.via_tor {
                        Ok(None)
                    } else if let Some(permit) = first.take() {
                        Ok(Some(permit))
                    } else if running.is_empty() {
                        let until = deadline.checked_sub(MIN_ATTEMPT).unwrap_or(now).max(now);
                        self.governor
                            .acquire_while(lane, Demand::Block, until, || job.wanted())
                            .map(Some)
                            .ok_or(())
                    } else {
                        self.governor.try_acquire_block(lane).map(Some).ok_or(())
                    };
                    let stream_wait = stream_wait.elapsed();
                    match stream {
                        Err(()) if running.is_empty() => {
                            errors.push("every tor stream stayed busy".to_string());
                            break;
                        }
                        // No free stream for a hedge now: the running
                        // attempts go on and the hedge is looked at again on
                        // the next tick.
                        Err(()) => {}
                        Ok(stream) => {
                            // Waiting for an attempt permit must not outlive
                            // the network budget: `queue_deadline` covers the
                            // download slot only (AUD-167). A prefetch or
                            // decoy leaves one permit free so a wallet never
                            // queues behind them.
                            let keep_free = usize::from(!wallet);
                            if let Some(permit) = self.external_attempts.try_acquire(keep_free) {
                                let (source, endpoint) = plan[next].clone();
                                *tried
                                    .entry(Route::block(source.as_ref(), &endpoint))
                                    .or_insert(0) += 1;
                                let ctl = if source.requires_qualification() {
                                    AttemptCtl::for_endpoint(&endpoint)
                                } else {
                                    AttemptCtl::new()
                                };
                                running.insert(
                                    next,
                                    Running {
                                        ctl: ctl.clone(),
                                        started: Instant::now(),
                                        via_tor: endpoint.via_tor,
                                    },
                                );
                                self.spawn_attempt(
                                    next,
                                    source,
                                    endpoint,
                                    *rec,
                                    deadline,
                                    ctl,
                                    tx.clone(),
                                    AttemptLog {
                                        stream,
                                        lane,
                                        role,
                                        stream_wait,
                                        permit,
                                    },
                                );
                                next += 1;
                                continue;
                            } else if running.is_empty() {
                                // Waiting for one resource must not hold the other.
                                drop(stream);
                                let seq = self.recovery_event.sequence();
                                if self.external_attempts.can_acquire(keep_free) {
                                    continue;
                                }
                                self.recovery_event
                                    .wait(seq, Some(deadline.min(Instant::now() + RACE_TICK)));
                                continue;
                            }
                        }
                    }
                }
            }
            if running.is_empty() && next >= plan.len() {
                if attempts > 0 {
                    job.exhausted.store(true, Ordering::Release);
                }
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
                let until = deadline.min(Instant::now() + Duration::from_secs(1));
                while job.wanted() && Instant::now() < until {
                    let seq = self.recovery_event.sequence();
                    if job.wanted() {
                        self.recovery_event.wait(seq, Some(until));
                    }
                }
                let mut fresh: Vec<(Arc<dyn BlockSource>, Endpoint)> = Vec::new();
                for (source, endpoint) in self.block_endpoints(FetchMode::Quiet) {
                    let route = Route::block(source.as_ref(), &endpoint);
                    let tries = tried.get(&route).copied().unwrap_or(0);
                    let allowed =
                        tries == 0 || (endpoint.via_tor && tries <= TOR_RETRIES_PER_ENDPOINT);
                    if allowed
                        && !fresh
                            .iter()
                            .any(|(s, e)| Route::block(s.as_ref(), e) == route)
                    {
                        fresh.push((source, endpoint));
                    }
                }
                if !fresh.is_empty() {
                    exhausted_since = None;
                    job.exhausted.store(false, Ordering::Release);
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
                Ok((idx, Ok((block, size)))) => {
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
                    self.stats.bytes.fetch_add(size as u64, Ordering::Relaxed);
                    if plan[idx].1.via_tor && job.wallet_waiting.load(Ordering::Acquire) {
                        self.governor.healthy();
                    }
                    return Ok(block);
                }
                Ok((idx, Err(e))) => {
                    if let Some(ended) = running.remove(&idx) {
                        let (source, endpoint) = plan[idx].clone();
                        if is_queue_timeout(&e) {
                            let route = Route::block(source.as_ref(), &endpoint);
                            if let Some(count) = tried.get_mut(&route) {
                                *count = count.saturating_sub(1);
                            }
                            continue;
                        }
                        errors.push(format!("{}: {e}", source.name()));
                        // Over Tor the next attempt is a new circuit (new exit):
                        // after a failure a new circuit may fix, the endpoint
                        // gets one more try in this race, at the end of the
                        // queue. The attempt says so itself: by now the
                        // endpoint is cooling down and no longer listed, so
                        // asking the source would always say no.
                        let tries = retried
                            .entry(Route::block(source.as_ref(), &endpoint))
                            .or_insert(0);
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
            format!("no external attempt could be made: {LOCAL_BUSY}")
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
        rec: el_model::BlockRecord,
        deadline: Instant,
        ctl: AttemptCtl,
        tx: mpsc::Sender<AttemptResult>,
        log: AttemptLog,
    ) {
        let stats = Arc::clone(&self.stats);
        let qualification = Arc::clone(&self.qualification);
        let event = Arc::clone(&self.recovery_event);
        let failed = tx.clone();
        let spawned = std::thread::Builder::new()
            .name(el_model::sync::THREAD_FETCH.to_string())
            .spawn(move || {
                let AttemptLog {
                    stream,
                    lane,
                    role,
                    stream_wait,
                    permit,
                } = log;
                let hash = rec.hash;
                let route = Route::block(source.as_ref(), &endpoint);
                let generation = if source.requires_qualification() {
                    let Some(generation) = qualification.admit(&route) else {
                        qualification.skipped();
                        drop((stream, permit));
                        let _ = tx.send((idx, Err(RawtxError::Unavailable(LOCAL_BUSY.into()))));
                        event.notify();
                        return;
                    };
                    Some(generation)
                } else {
                    None
                };
                let started = Instant::now();
                stats.race_attempts.fetch_add(1, Ordering::Relaxed);
                if role == "hedge" {
                    stats.race_hedges.fetch_add(1, Ordering::Relaxed);
                }
                let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    let mode = if lane == Lane::Wallet && generation.is_none() {
                        FetchMode::Normal
                    } else {
                        FetchMode::Quiet
                    };
                    source.fetch_endpoint(&endpoint, &hash, deadline, mode, &ctl)
                }))
                .unwrap_or_else(|_| Err(RawtxError::Unavailable("attempt panicked".into())));
                // The connection is gone once the source returns: only now is
                // the Tor stream given back, also for an aborted loser that
                // took a while to notice (29.25.2).
                ctl.close_sockets();
                drop(stream);
                let finished = Instant::now();
                if debug_logs() {
                    el_model::debug_event!(
                        "stream block obj={} src={} ep={} lane={} {role} wait {} ms, {} ms, {} KiB: {}",
                        log_tag(&hash),
                        source.name(),
                        endpoint_label(&endpoint),
                        lane.name(),
                        stream_wait.as_millis(),
                        started.elapsed().as_millis(),
                        ctl.bytes() / 1024,
                        match &result {
                            Ok(_) => "ok".to_string(),
                            Err(_) if ctl.aborted() => "aborted (lost the race)".to_string(),
                            Err(e) => redact_hashes(&e.to_string()),
                        }
                    );
                }
                let result = result.and_then(|bytes| {
                    match verify_full(&bytes, &rec) {
                        Ok(block) => Ok((block, bytes.len())),
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
                if let Some(generation) = generation {
                    if !ctl.aborted() && !ctl.non_health_failure() {
                        let sample = match &result {
                            Ok(_) if block_fast(&ctl, started, finished, endpoint.via_tor) => Some(Sample::Fast),
                            Ok(_) => Some(Sample::Slow),
                            Err(RawtxError::Verify(_)) => Some(Sample::Invalid),
                            Err(_) if ctl.retry_after().is_some() => {
                                ctl.retry_after().map(Sample::RateLimited)
                            }
                            Err(e) if !is_queue_timeout(e)
                                && deadline.saturating_duration_since(started) >= if endpoint.via_tor {
                                    HEDGE_FIRST_BYTE_TOR
                                } else {
                                    HEDGE_FIRST_BYTE_DIRECT
                                } => Some(Sample::Failed),
                            _ => None,
                        };
                        if let Some(sample) = sample {
                            qualification.observe(&route, generation, sample);
                        }
                    }
                }
                drop(permit);
                event.notify();
                let _ = tx.send((idx, result));
            });
        if let Err(e) = spawned {
            eprintln!("[e-lite] fetch thread did not start: {e}");
            let _ = failed.send((
                idx,
                Err(RawtxError::Unavailable(
                    "could not start external attempt".into(),
                )),
            ));
        }
    }

    /// A failure of `hash` younger than `FAILURE_MEMORY`, if any.
    pub(super) fn recent_failure(&self, hash: &Hash32) -> Option<RawtxError> {
        let mut failed = self.failed.lock().ok()?;
        let now = Instant::now();
        failed.retain(|_, (at, _)| now.duration_since(*at) < FAILURE_MEMORY);
        failed.get(hash).map(|(_, e)| e.clone())
    }

    /// When the remembered failure of `hash` expires, if there is one.
    pub(super) fn failure_expiry(&self, hash: &Hash32) -> Option<Instant> {
        let failed = self.failed.lock().ok()?;
        failed
            .get(hash)
            .map(|(at, _)| *at + FAILURE_MEMORY)
            .filter(|t| *t > Instant::now())
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

/// What one race attempt carries for the stream budget and its log line.
type AttemptResult = (usize, Result<(bitcoin::Block, usize)>);

struct AttemptLog {
    /// The Tor stream (none for a direct attempt); released when the
    /// attempt's thread is done with the connection.
    stream: Option<GovernorPermit>,
    lane: Lane,
    /// `first`, `hedge` or `retry`.
    role: &'static str,
    /// Time spent waiting for the stream.
    stream_wait: Duration,
    permit: Permit,
}

/// Endpoint for a debug line: the transport and the provider or peer, never
/// anything about the wallet.
fn endpoint_label(endpoint: &Endpoint) -> String {
    if endpoint.key.is_empty() {
        "whole".to_string()
    } else {
        endpoint.key.replace(' ', ":")
    }
}

/// A download that never got a download slot (local queueing, not a source
/// failure).
fn is_queue_timeout(e: &RawtxError) -> bool {
    matches!(
        e,
        RawtxError::Unavailable(m)
            if m.contains("download slot stayed busy") || m.contains("attempt slot stayed busy")
                || m.contains(LOCAL_BUSY)
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
