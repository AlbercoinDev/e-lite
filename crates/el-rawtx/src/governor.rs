//! Global budget of concurrent Tor streams for one resolver.
//!
//! A home Tor chokes when too many streams are opened at once: measured on a
//! mainnet node, 16 concurrent block downloads plus the per-txid sessions and
//! decoys filled the SOCKS listener's backlog (`socks connect: Resource
//! temporarily unavailable`), stalled every circuit and ended in provider
//! 429s. Every external stream of this resolver therefore goes through this
//! governor: a fixed budget with wallet > prefetch > decoy priority, and a
//! global pause (exponential backoff) when the local proxy refuses.
//!
//! The fast path has no cost: with a free slot and no pause the permit is
//! granted at once, a request ends the moment its result arrives (condvar,
//! not polling), and any success lifts the pause immediately.
//!
//! A permit is one **stream**, not one request (29.25.2): a block race takes
//! one per attempt over Tor and holds it in the attempt's thread until the
//! connection is really gone; a per-txid session takes one per connection.
//! Only a refusal by the local Tor itself pauses the governor: a provider's
//! 429 or a circuit that is slow to build is that endpoint's problem and is
//! handled by its health (`health.rs`).

use el_model::sync::Mutex;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Weak};
use std::time::{Duration, Instant};

/// Lanes of the governor, highest priority first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Lane {
    Wallet = 0,
    Prefetch = 1,
    Decoy = 2,
}

const LEVELS: usize = 3;
const LANE_NAMES: [&str; LEVELS] = ["wallet", "prefetch", "decoy"];

impl Lane {
    pub fn name(self) -> &'static str {
        LANE_NAMES[self as usize]
    }
}
const BASE_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(4);

/// Who is asking for a wallet stream. A per-txid batch is a small request
/// that is ready to go and answers the wallet directly; a block download is
/// the shared fallback. They take turns at a freed stream, so a backlog of
/// block downloads cannot starve the ready batches (and the fallback still
/// makes progress instead of waiting for the whole load to finish).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Demand {
    #[default]
    Batch = 0,
    Block = 1,
}

impl Demand {
    fn other(self) -> Self {
        match self {
            Demand::Batch => Demand::Block,
            Demand::Block => Demand::Batch,
        }
    }
}

/// Both wallet demands have their own FIFO, so a ticket queued by a block
/// download is not a ticket queued by a batch.
const DEMANDS: usize = 2;

struct Capacity {
    in_use: usize,
    /// Waiting tickets per lane: `[batch, block]`.
    queues: [[VecDeque<u64>; DEMANDS]; LEVELS],
    next: u64,
    /// The wallet demand that takes the next freed stream.
    turn: Demand,
    pause_until: Option<Instant>,
    backoff: Duration,
}

impl Capacity {
    fn higher_lanes_empty(&self, lane: Lane) -> bool {
        self.queues[..lane as usize]
            .iter()
            .flatten()
            .all(VecDeque::is_empty)
    }
}

/// Alternation between the two wallet demands. The demand whose turn it is
/// goes first; when it has nobody waiting, the other one may use the stream
/// at once, so a one-sided load never waits for a turn that nobody will take.
fn turn_allows(state: &Capacity, lane: Lane, demand: Demand) -> bool {
    lane != Lane::Wallet
        || state.turn == demand
        || state.queues[lane as usize][demand.other() as usize].is_empty()
}

fn advance_turn(state: &mut Capacity, lane: Lane, demand: Demand) {
    if lane != Lane::Wallet {
        return;
    }
    state.turn = if state.queues[lane as usize][demand.other() as usize].is_empty() {
        demand
    } else {
        demand.other()
    };
}

/// Event sequence shared with the transaction scheduler. Taking a snapshot
/// before inspecting work makes a notification between inspection and sleep
/// observable, even if nobody was waiting yet.
#[derive(Default)]
pub(crate) struct Availability {
    seq: Mutex<u64>,
    cv: Condvar,
}

impl Availability {
    pub(crate) fn sequence(&self) -> u64 {
        *self.seq.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub(crate) fn notify(&self) {
        let mut seq = self.seq.lock().unwrap_or_else(|p| p.into_inner());
        *seq = seq.wrapping_add(1);
        self.cv.notify_all();
    }

    pub(crate) fn wait(&self, before: u64, until: Option<Instant>) {
        let mut seq = self.seq.lock().unwrap_or_else(|p| p.into_inner());
        while *seq == before {
            seq = match until {
                Some(end) => {
                    let left = end.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return;
                    }
                    self.cv
                        .wait_timeout(seq, left)
                        .unwrap_or_else(|p| p.into_inner())
                        .0
                }
                None => self.cv.wait(seq).unwrap_or_else(|p| p.into_inner()),
            };
        }
    }
}

/// A held stream slot. Released on drop.
pub struct GovernorPermit {
    governor: Arc<TorGovernor>,
}

impl Drop for GovernorPermit {
    fn drop(&mut self) {
        self.governor.release();
    }
}

pub struct TorGovernor {
    total: usize,
    capacity: Mutex<Capacity>,
    changed: Condvar,
    scheduler: Mutex<Weak<Availability>>,
    pauses: AtomicUsize,
    peak: AtomicUsize,
    lanes: [LaneStats; LEVELS],
    /// Last periodic `debug:` summary and the counters it printed.
    summary: Mutex<(Instant, GovernorSnapshot)>,
}

/// How often the governor writes its `debug:` summary while it is in use.
const SUMMARY_EVERY: Duration = Duration::from_secs(10);

/// Counters of one lane, for `/status` and the periodic debug summary.
#[derive(Default)]
struct LaneStats {
    granted: AtomicU64,
    /// Grants that had to wait (no free slot or a pause).
    waited: AtomicU64,
    wait_ms: AtomicU64,
    max_wait_ms: AtomicU64,
    timeouts: AtomicU64,
}

/// Point-in-time view of the governor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GovernorSnapshot {
    pub total: usize,
    pub in_use: usize,
    pub peak: usize,
    pub pauses: usize,
    pub paused: bool,
    /// Waiting now, per lane (wallet, prefetch, decoy).
    pub queued: [usize; LEVELS],
    pub granted: [u64; LEVELS],
    pub waited: [u64; LEVELS],
    pub wait_ms: [u64; LEVELS],
    pub max_wait_ms: [u64; LEVELS],
    pub timeouts: [u64; LEVELS],
}

impl TorGovernor {
    fn capacity_for(&self, lane: Lane) -> usize {
        self.total - usize::from(lane != Lane::Wallet && self.total > 1)
    }
    pub fn new(total: usize) -> Arc<Self> {
        Arc::new(Self {
            total: total.clamp(1, 32),
            capacity: Mutex::new(Capacity {
                in_use: 0,
                queues: Default::default(),
                next: 0,
                turn: Demand::Batch,
                pause_until: None,
                backoff: BASE_BACKOFF,
            }),
            changed: Condvar::new(),
            scheduler: Mutex::new(Weak::new()),
            pauses: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            lanes: Default::default(),
            summary: Mutex::new((Instant::now(), GovernorSnapshot::default())),
        })
    }

    /// The local Tor refused a stream (its SOCKS listener did not accept or
    /// answer the greeting): hold every new stream for a moment instead of
    /// hammering. The backoff doubles per pause up to `MAX_BACKOFF` and is
    /// reset by any success. `reason` goes to the debug log only.
    pub fn pause(&self, reason: &str) {
        let mut state = self.capacity.lock().unwrap_or_else(|p| p.into_inner());
        state.backoff = (state.backoff * 2).min(MAX_BACKOFF);
        let hold = state.backoff;
        state.pause_until = Some(Instant::now() + hold);
        drop(state);
        self.notify();
        let n = self.pauses.fetch_add(1, Ordering::Relaxed) + 1;
        if crate::debug_logs() {
            eprintln!(
                "[e-lite] debug: tor governor: pause {} ms ({reason}); streams {}/{}, pause #{n}",
                hold.as_millis(),
                self.in_use(),
                self.total
            );
        }
    }

    /// A stream succeeded: Tor is healthy again. Lift the pause at once and
    /// wake every waiter so nobody waits out the rest of a pause.
    pub fn healthy(&self) {
        let mut state = self.capacity.lock().unwrap_or_else(|p| p.into_inner());
        state.backoff = BASE_BACKOFF;
        let lifted = state.pause_until.take().is_some();
        drop(state);
        if lifted && crate::debug_logs() {
            eprintln!(
                "[e-lite] debug: tor governor: pause lifted by a success; streams {}/{}",
                self.in_use(),
                self.total
            );
        }
        if lifted {
            self.notify();
        }
    }

    pub(crate) fn paused_until(&self) -> Option<Instant> {
        self.capacity
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pause_until
            .filter(|until| *until > Instant::now())
    }

    pub fn in_use(&self) -> usize {
        self.capacity.lock().map(|state| state.in_use).unwrap_or(0)
    }

    pub fn total(&self) -> usize {
        self.total
    }

    pub fn pauses(&self) -> usize {
        self.pauses.load(Ordering::Relaxed)
    }

    pub fn paused(&self) -> bool {
        self.paused_until().is_some()
    }

    pub(crate) fn watch(&self, event: &Arc<Availability>) {
        *self.scheduler.lock().unwrap_or_else(|p| p.into_inner()) = Arc::downgrade(event);
    }

    fn notify(&self) {
        self.changed.notify_all();
        if let Some(event) = self.scheduler.lock().ok().and_then(|event| event.upgrade()) {
            event.notify();
        }
    }

    /// Hold one Tor stream until `deadline`. The highest-priority waiter gets
    /// the next freed slot; a wallet stream never queues behind a decoy. The
    /// two wallet demands alternate, so neither waits out the other.
    pub fn acquire(self: &Arc<Self>, lane: Lane, deadline: Instant) -> Option<GovernorPermit> {
        self.acquire_while(lane, Demand::Batch, deadline, || true)
    }

    pub(crate) fn wake_waiters(&self) {
        // Synchronize with the waiter's predicate check and transition to
        // sleep, even when the predicate itself lives outside the governor.
        let _state = self.capacity.lock().unwrap_or_else(|p| p.into_inner());
        self.changed.notify_all();
    }

    pub(crate) fn acquire_while(
        self: &Arc<Self>,
        lane: Lane,
        demand: Demand,
        deadline: Instant,
        wanted: impl Fn() -> bool,
    ) -> Option<GovernorPermit> {
        let started = Instant::now();
        let permit = self.acquire_inner(lane, demand, deadline, &wanted);
        let stats = &self.lanes[lane as usize];
        match &permit {
            Some(_) => {
                stats.granted.fetch_add(1, Ordering::Relaxed);
                let waited = started.elapsed().as_millis() as u64;
                if waited > 0 {
                    stats.waited.fetch_add(1, Ordering::Relaxed);
                    stats.wait_ms.fetch_add(waited, Ordering::Relaxed);
                    stats.max_wait_ms.fetch_max(waited, Ordering::Relaxed);
                }
            }
            None if wanted() => {
                stats.timeouts.fetch_add(1, Ordering::Relaxed);
            }
            None => {}
        }
        permit
    }

    /// A stream now if one is free and Tor is not paused; never waits (a
    /// hedge must not queue: the attempt it hedges is still running). A
    /// queued waiter of either wallet demand goes first.
    pub fn try_acquire(self: &Arc<Self>, lane: Lane) -> Option<GovernorPermit> {
        self.try_acquire_demand(lane, Demand::Batch)
    }

    /// The same probe for a block download's hedge.
    pub(crate) fn try_acquire_block(self: &Arc<Self>, lane: Lane) -> Option<GovernorPermit> {
        self.try_acquire_demand(lane, Demand::Block)
    }

    fn try_acquire_demand(self: &Arc<Self>, lane: Lane, demand: Demand) -> Option<GovernorPermit> {
        let mut state = self.capacity.lock().unwrap_or_else(|p| p.into_inner());
        if state.in_use >= self.capacity_for(lane)
            || state
                .pause_until
                .is_some_and(|until| until > Instant::now())
            || !state.higher_lanes_empty(lane)
            || !state.queues[lane as usize][demand as usize].is_empty()
            || !turn_allows(&state, lane, demand)
        {
            return None;
        }
        self.lanes[lane as usize]
            .granted
            .fetch_add(1, Ordering::Relaxed);
        let permit = self.take(&mut state, lane, demand);
        drop(state);
        self.notify();
        Some(permit)
    }

    /// Count one used stream and give the next freed one to the other wallet
    /// demand: a batch that is ready must not wait until the block queue
    /// drains, and a block download must not wait for an endless batch stream.
    fn take(self: &Arc<Self>, state: &mut Capacity, lane: Lane, demand: Demand) -> GovernorPermit {
        state.in_use += 1;
        self.peak.fetch_max(state.in_use, Ordering::Relaxed);
        advance_turn(state, lane, demand);
        GovernorPermit {
            governor: Arc::clone(self),
        }
    }

    /// Counters and the state right now.
    pub fn snapshot(&self) -> GovernorSnapshot {
        let state = self.capacity.lock().unwrap_or_else(|p| p.into_inner());
        let queued = std::array::from_fn(|i| state.queues[i].iter().map(VecDeque::len).sum());
        let read = |f: fn(&LaneStats) -> &AtomicU64| -> [u64; LEVELS] {
            [0, 1, 2].map(|i| f(&self.lanes[i]).load(Ordering::Relaxed))
        };
        GovernorSnapshot {
            total: self.total,
            in_use: state.in_use,
            peak: self.peak.load(Ordering::Relaxed),
            pauses: self.pauses(),
            paused: state
                .pause_until
                .is_some_and(|until| until > Instant::now()),
            queued,
            granted: read(|l| &l.granted),
            waited: read(|l| &l.waited),
            wait_ms: read(|l| &l.wait_ms),
            max_wait_ms: read(|l| &l.max_wait_ms),
            timeouts: read(|l| &l.timeouts),
        }
    }

    fn acquire_inner(
        self: &Arc<Self>,
        lane: Lane,
        demand: Demand,
        deadline: Instant,
        wanted: &dyn Fn() -> bool,
    ) -> Option<GovernorPermit> {
        let mut state = self.capacity.lock().unwrap_or_else(|p| p.into_inner());
        let limit = self.capacity_for(lane);
        let lane_index = lane as usize;
        let demand_index = demand as usize;
        let mut ticket = None;
        loop {
            let now = Instant::now();
            if now >= deadline || !wanted() {
                if let Some(id) = ticket {
                    state.queues[lane_index][demand_index].retain(|&other| other != id);
                    drop(state);
                    self.notify();
                }
                return None;
            }
            let pause = state.pause_until.filter(|&until| until > now);
            if state.in_use < limit
                && pause.is_none()
                && state.higher_lanes_empty(lane)
                && state.queues[lane_index][demand_index].front().copied() == ticket
                && turn_allows(&state, lane, demand)
            {
                if ticket.is_some() {
                    state.queues[lane_index][demand_index].pop_front();
                }
                let permit = self.take(&mut state, lane, demand);
                drop(state);
                self.notify();
                return Some(permit);
            }
            if ticket.is_none() {
                let id = state.next;
                state.next = id.wrapping_add(1);
                state.queues[lane_index][demand_index].push_back(id);
                ticket = Some(id);
            }
            let until = pause.map_or(deadline, |until| deadline.min(until));
            state = self
                .changed
                .wait_timeout(state, until - now)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }

    /// At most every `SUMMARY_EVERY`, and only when streams were granted
    /// since the last one, one `debug:` line with what happened in between:
    /// enough to tell who held the streams during a slow load without a line
    /// per stream. Called on release, so an idle governor writes nothing.
    fn maybe_summary(&self) {
        if !crate::debug_logs() {
            return;
        }
        let Ok(mut last) = self.summary.try_lock() else {
            return;
        };
        if last.0.elapsed() < SUMMARY_EVERY {
            return;
        }
        let now = self.snapshot();
        let before = &last.1;
        let delta = |a: [u64; LEVELS], b: [u64; LEVELS]| [0, 1, 2].map(|i| a[i] - b[i].min(a[i]));
        let granted = delta(now.granted, before.granted);
        if granted.iter().sum::<u64>() == 0 {
            *last = (Instant::now(), now);
            return;
        }
        let waited = delta(now.waited, before.waited);
        let wait_ms = delta(now.wait_ms, before.wait_ms);
        let timeouts = delta(now.timeouts, before.timeouts);
        let lanes: Vec<String> = (0..LEVELS)
            .filter(|&i| granted[i] + timeouts[i] > 0 || now.queued[i] > 0)
            .map(|i| {
                format!(
                    "{} {} granted ({} waited, avg {} ms), {} timed out, {} queued",
                    LANE_NAMES[i],
                    granted[i],
                    waited[i],
                    wait_ms[i].checked_div(waited[i]).unwrap_or(0),
                    timeouts[i],
                    now.queued[i]
                )
            })
            .collect();
        eprintln!(
            "[e-lite] debug: tor governor {}s: streams {}/{} (peak {}), pauses {}{}; {}",
            last.0.elapsed().as_secs(),
            now.in_use,
            now.total,
            now.peak,
            now.pauses - before.pauses.min(now.pauses),
            if now.paused { ", paused now" } else { "" },
            lanes.join("; ")
        );
        *last = (Instant::now(), now);
    }

    fn release(&self) {
        let mut state = self.capacity.lock().unwrap_or_else(|p| p.into_inner());
        state.in_use -= 1;
        drop(state);
        self.notify();
        self.maybe_summary();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    #[test]
    fn wallet_jumps_the_queue_ahead_of_decoys() {
        let governor = TorGovernor::new(1);
        let deadline = Instant::now() + ms(5000);
        let decoy = governor.acquire(Lane::Decoy, deadline).unwrap();
        let worker = std::thread::spawn(move || {
            std::thread::sleep(ms(100));
            drop(decoy);
        });
        std::thread::sleep(ms(50));
        let wallet_started = Instant::now();
        let wallet = governor.acquire(Lane::Wallet, deadline).unwrap();
        assert!(
            wallet_started.elapsed() < ms(400),
            "the wallet takes the freed slot before another decoy"
        );
        drop(wallet);
        worker.join().unwrap();
    }

    #[test]
    fn a_deadline_returns_none_and_does_not_leak_the_slot() {
        let governor = TorGovernor::new(1);
        let held = governor
            .acquire(Lane::Wallet, Instant::now() + ms(5000))
            .unwrap();
        assert!(governor
            .acquire(Lane::Wallet, Instant::now() + ms(50))
            .is_none());
        drop(held);
        assert!(governor
            .acquire(Lane::Wallet, Instant::now() + ms(100))
            .is_some());
    }

    #[test]
    fn a_success_lifts_the_pause_at_once() {
        let governor = TorGovernor::new(2);
        governor.pause("test");
        governor.pause("test");
        let started = Instant::now();
        governor.healthy();
        assert!(governor
            .acquire(Lane::Wallet, Instant::now() + ms(200))
            .is_some());
        assert!(
            started.elapsed() < ms(150),
            "a success lifts the pause: nobody waits it out"
        );
        assert!(!governor.paused());
    }

    #[test]
    fn a_ready_txid_batch_takes_its_turn_instead_of_the_whole_block_queue() {
        let g = TorGovernor::new(2);
        // Two streams held by block downloads, as during a cold wallet load.
        let first = g.try_acquire_block(Lane::Wallet).unwrap();
        let second = g.try_acquire_block(Lane::Wallet).unwrap();
        let order = Arc::new(Mutex::new(Vec::<&'static str>::new()));
        let gov = &g;
        std::thread::scope(|scope| {
            let mut blocks = Vec::new();
            for _ in 0..6 {
                let order = Arc::clone(&order);
                blocks.push(scope.spawn(move || {
                    let permit = gov.acquire_while(
                        Lane::Wallet,
                        Demand::Block,
                        Instant::now() + ms(3000),
                        || true,
                    );
                    if let Some(permit) = permit {
                        order.lock().unwrap().push("block");
                        std::thread::sleep(ms(5));
                        drop(permit);
                    }
                }));
            }
            let start = Instant::now();
            while g.snapshot().queued[0] < 6 && start.elapsed() < ms(1000) {
                std::thread::yield_now();
            }
            assert_eq!(g.snapshot().queued[0], 6, "six block downloads wait");
            // A ready per-txid batch arrives behind every queued block.
            let (done, granted) = std::sync::mpsc::channel();
            let batch_order = Arc::clone(&order);
            let batch = scope.spawn(move || {
                let permit = gov.acquire(Lane::Wallet, Instant::now() + ms(3000));
                batch_order.lock().unwrap().push("batch");
                done.send(()).unwrap();
                drop(permit);
            });
            // Both streams are held, so the batch must be queued before one
            // frees: wait for it instead of racing the drop below.
            let start = Instant::now();
            while g.snapshot().queued[0] < 7 && start.elapsed() < ms(1000) {
                std::thread::yield_now();
            }
            assert_eq!(g.snapshot().queued[0], 7, "the batch is queued too");
            drop(first);
            granted
                .recv_timeout(ms(2000))
                .expect("the ready batch must not wait for the block queue to drain");
            let position = order
                .lock()
                .unwrap()
                .iter()
                .position(|who| *who == "batch")
                .unwrap();
            assert!(
                position <= 2,
                "the batch went at position {position}: {order:?}"
            );
            drop(second);
            batch.join().unwrap();
            for block in blocks {
                block.join().unwrap();
            }
        });
        assert_eq!(g.in_use(), 0);
    }

    #[test]
    fn ready_batches_do_not_starve_the_block_fallback() {
        let g = TorGovernor::new(1);
        let held = g.try_acquire(Lane::Wallet).unwrap();
        let order = Arc::new(Mutex::new(Vec::<&'static str>::new()));
        let gov = &g;
        std::thread::scope(|scope| {
            let mut batches = Vec::new();
            for _ in 0..4 {
                let order = Arc::clone(&order);
                batches.push(scope.spawn(move || {
                    let permit = gov.acquire(Lane::Wallet, Instant::now() + ms(3000));
                    if let Some(permit) = permit {
                        order.lock().unwrap().push("batch");
                        drop(permit);
                    }
                }));
            }
            let start = Instant::now();
            while g.snapshot().queued[0] < 4 && start.elapsed() < ms(1000) {
                std::thread::yield_now();
            }
            assert_eq!(g.snapshot().queued[0], 4, "four ready batches wait");
            let (done, granted) = std::sync::mpsc::channel();
            let block_order = Arc::clone(&order);
            let block = scope.spawn(move || {
                let permit = gov.acquire_while(
                    Lane::Wallet,
                    Demand::Block,
                    Instant::now() + ms(3000),
                    || true,
                );
                block_order.lock().unwrap().push("block");
                done.send(()).unwrap();
                drop(permit);
            });
            // The stream is held, so the block download must be queued before
            // it frees: wait for it instead of racing the drop below.
            let start = Instant::now();
            while g.snapshot().queued[0] < 5 && start.elapsed() < ms(1000) {
                std::thread::yield_now();
            }
            assert_eq!(g.snapshot().queued[0], 5, "the block download is queued");
            drop(held);
            granted
                .recv_timeout(ms(2000))
                .expect("the block fallback must not wait for every ready batch");
            let position = order
                .lock()
                .unwrap()
                .iter()
                .position(|who| *who == "block")
                .unwrap();
            assert!(
                position <= 2,
                "the block went at position {position}: {order:?}"
            );
            block.join().unwrap();
            for batch in batches {
                batch.join().unwrap();
            }
        });
        assert_eq!(g.in_use(), 0);
    }

    #[test]
    fn pause_blocks_new_streams_until_it_expires() {
        let governor = TorGovernor::new(2);
        governor.pause("test");
        let started = Instant::now();
        assert!(governor
            .acquire(Lane::Wallet, Instant::now() + ms(100))
            .is_none());
        assert!(started.elapsed() >= ms(90), "the pause is respected");
        assert!(governor.paused());
    }

    #[test]
    fn a_stream_released_during_a_pause_stays_free_until_resume() {
        let g = TorGovernor::new(1);
        let held = g.try_acquire(Lane::Wallet).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::scope(|s| {
            s.spawn(|| {
                let permit = g.acquire(Lane::Wallet, Instant::now() + ms(3000));
                tx.send(permit).unwrap();
            });
            let start = Instant::now();
            while g.snapshot().queued[0] == 0 && start.elapsed() < ms(1000) {
                std::thread::yield_now();
            }
            assert_eq!(g.snapshot().queued[0], 1);
            g.pause("test");
            drop(held);
            assert!(rx.recv_timeout(ms(50)).is_err());
            assert_eq!(g.in_use(), 0);
            g.healthy();
            drop(rx.recv_timeout(ms(500)).unwrap().unwrap());
        });
        assert_eq!(g.in_use(), 0);
    }

    #[test]
    fn the_fast_path_has_no_added_latency() {
        let governor = TorGovernor::new(4);
        let started = Instant::now();
        for _ in 0..4 {
            assert!(governor
                .acquire(Lane::Wallet, Instant::now() + ms(1000))
                .is_some());
        }
        assert!(
            started.elapsed() < ms(50),
            "free capacity is granted at once"
        );
    }

    #[test]
    fn try_acquire_never_waits_and_counts() {
        let governor = TorGovernor::new(1);
        let held = governor.try_acquire(Lane::Wallet).unwrap();
        let started = Instant::now();
        assert!(governor.try_acquire(Lane::Wallet).is_none());
        assert!(started.elapsed() < ms(20), "a hedge never queues");
        drop(held);
        assert!(governor.try_acquire(Lane::Prefetch).is_some());
        governor.pause("test");
        assert!(governor.try_acquire(Lane::Wallet).is_none(), "paused");
        let snap = governor.snapshot();
        assert_eq!(snap.granted, [1, 1, 0]);
        assert_eq!(snap.in_use, 0);
        assert_eq!(snap.peak, 1);
    }

    #[test]
    fn waits_and_timeouts_are_counted_per_lane() {
        let governor = TorGovernor::new(1);
        let held = governor
            .acquire(Lane::Wallet, Instant::now() + ms(1000))
            .unwrap();
        assert!(governor
            .acquire(Lane::Decoy, Instant::now() + ms(30))
            .is_none());
        let g = Arc::clone(&governor);
        let waiter = std::thread::spawn(move || {
            g.acquire(Lane::Prefetch, Instant::now() + ms(2000))
                .is_some()
        });
        std::thread::sleep(ms(60));
        assert_eq!(governor.snapshot().queued, [0, 1, 0]);
        drop(held);
        assert!(waiter.join().unwrap());
        let snap = governor.snapshot();
        assert_eq!(snap.timeouts, [0, 0, 1]);
        assert_eq!(snap.waited[1], 1);
        assert!(snap.max_wait_ms[1] >= 50);
        assert_eq!(snap.in_use, 0, "the waiter's permit was released");
    }

    #[test]
    fn a_success_wakes_a_caller_already_waiting_out_a_pause() {
        let governor = TorGovernor::new(2);
        for _ in 0..4 {
            governor.pause("test"); // backoff at its 4 s cap
        }
        let g = Arc::clone(&governor);
        let started = Instant::now();
        let waiter = std::thread::spawn(move || {
            g.acquire(Lane::Wallet, Instant::now() + ms(5000)).is_some()
        });
        std::thread::sleep(ms(100));
        governor.healthy();
        assert!(waiter.join().unwrap());
        assert!(
            started.elapsed() < ms(600),
            "woken by the success, not by the end of the pause: {:?}",
            started.elapsed()
        );
    }
}
