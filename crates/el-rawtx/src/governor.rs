//! Global budget of concurrent Tor streams for one resolver.
//!
//! A home Tor chokes when too many streams are opened at once: measured on a
//! mainnet node, 16 concurrent block downloads plus the per-txid sessions and
//! decoys filled the SOCKS listener's backlog (`socks connect: Resource
//! temporarily unavailable`), stalled every circuit and ended in provider
//! 429s. Every external stream of this resolver therefore goes through this
//! governor: a fixed budget with wallet > prefetch > decoy priority, and a
//! global pause (exponential backoff) whenever Tor or a provider refuses.
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
use std::sync::{Arc, Condvar};
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
const WAIT_SLICE: Duration = Duration::from_millis(25);
const BASE_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(4);

struct Waiter {
    done: Mutex<bool>,
    cv: Condvar,
}

impl Waiter {
    fn new() -> Self {
        Self {
            done: Mutex::new(false),
            cv: Condvar::new(),
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
    in_use: Mutex<usize>,
    queues: Mutex<[VecDeque<Arc<Waiter>>; LEVELS]>,
    pause_until: Mutex<Option<Instant>>,
    backoff: Mutex<Duration>,
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
    pub fn new(total: usize) -> Arc<Self> {
        Arc::new(Self {
            total: total.clamp(1, 32),
            in_use: Mutex::new(0),
            queues: Mutex::new([VecDeque::new(), VecDeque::new(), VecDeque::new()]),
            pause_until: Mutex::new(None),
            backoff: Mutex::new(BASE_BACKOFF),
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
        let mut backoff = self.backoff.lock().unwrap_or_else(|p| p.into_inner());
        *backoff = (*backoff * 2).min(MAX_BACKOFF);
        let hold = *backoff;
        drop(backoff);
        if let Ok(mut until) = self.pause_until.lock() {
            *until = Some(Instant::now() + hold);
        }
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
        if let Ok(mut backoff) = self.backoff.lock() {
            *backoff = BASE_BACKOFF;
        }
        let lifted = match self.pause_until.lock() {
            Ok(mut until) => {
                let was = until.is_some_and(|t| t > Instant::now());
                *until = None;
                was
            }
            Err(_) => false,
        };
        if lifted && crate::debug_logs() {
            eprintln!(
                "[e-lite] debug: tor governor: pause lifted by a success; streams {}/{}",
                self.in_use(),
                self.total
            );
        }
        if let Ok(queues) = self.queues.lock() {
            for lane in queues.iter() {
                for waiter in lane.iter() {
                    waiter.cv.notify_all();
                }
            }
        }
    }

    fn paused_until(&self) -> Option<Instant> {
        self.pause_until
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .filter(|until| *until > Instant::now())
    }

    pub fn in_use(&self) -> usize {
        self.in_use.lock().map(|guard| *guard).unwrap_or(0)
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

    fn pause_wait(&self, deadline: Instant) -> bool {
        // True: the pause ended (or there was none) and the caller may go on;
        // false: the deadline passed first.
        loop {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            match self.paused_until() {
                // In short slices: a success (`healthy`) lifts the pause at
                // once and this caller must see it, not sleep out the whole
                // backoff (29.25.2; it slept up to 4 s).
                Some(until) if until > now => {
                    std::thread::sleep((until.min(deadline) - now).min(WAIT_SLICE));
                }
                _ => return true,
            }
        }
    }

    /// Hold one Tor stream until `deadline`. The highest-priority waiter gets
    /// the next freed slot; a wallet stream never queues behind a decoy.
    pub fn acquire(self: &Arc<Self>, lane: Lane, deadline: Instant) -> Option<GovernorPermit> {
        let started = Instant::now();
        let permit = self.acquire_inner(lane, deadline);
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
                self.peak.fetch_max(self.in_use(), Ordering::Relaxed);
            }
            None => {
                stats.timeouts.fetch_add(1, Ordering::Relaxed);
            }
        }
        permit
    }

    /// A stream now if one is free and Tor is not paused; never waits (a
    /// hedge must not queue: the attempt it hedges is still running).
    pub fn try_acquire(self: &Arc<Self>, lane: Lane) -> Option<GovernorPermit> {
        if self.paused() {
            return None;
        }
        let mut in_use = self.in_use.lock().unwrap_or_else(|p| p.into_inner());
        if *in_use >= self.total {
            return None;
        }
        *in_use += 1;
        let now = *in_use;
        drop(in_use);
        self.lanes[lane as usize]
            .granted
            .fetch_add(1, Ordering::Relaxed);
        self.peak.fetch_max(now, Ordering::Relaxed);
        Some(GovernorPermit {
            governor: Arc::clone(self),
        })
    }

    /// Counters and the state right now.
    pub fn snapshot(&self) -> GovernorSnapshot {
        let queued = self
            .queues
            .lock()
            .map(|q| [q[0].len(), q[1].len(), q[2].len()])
            .unwrap_or_default();
        let read = |f: fn(&LaneStats) -> &AtomicU64| -> [u64; LEVELS] {
            [0, 1, 2].map(|i| f(&self.lanes[i]).load(Ordering::Relaxed))
        };
        GovernorSnapshot {
            total: self.total,
            in_use: self.in_use(),
            peak: self.peak.load(Ordering::Relaxed),
            pauses: self.pauses(),
            paused: self.paused(),
            queued,
            granted: read(|l| &l.granted),
            waited: read(|l| &l.waited),
            wait_ms: read(|l| &l.wait_ms),
            max_wait_ms: read(|l| &l.max_wait_ms),
            timeouts: read(|l| &l.timeouts),
        }
    }

    fn acquire_inner(self: &Arc<Self>, lane: Lane, deadline: Instant) -> Option<GovernorPermit> {
        if !self.pause_wait(deadline) {
            return None;
        }
        // The free-slot check and the enqueue happen under the queues lock,
        // which `release` takes first: a slot freed between "full" and
        // "queued" is handed to this waiter instead of staying free while it
        // waits out its deadline (29.25.2; the check used to be outside).
        let waiter = {
            let mut queues = self.queues.lock().unwrap_or_else(|p| p.into_inner());
            let mut in_use = self.in_use.lock().unwrap_or_else(|p| p.into_inner());
            if *in_use < self.total {
                *in_use += 1;
                return Some(GovernorPermit {
                    governor: Arc::clone(self),
                });
            }
            drop(in_use);
            let waiter = Arc::new(Waiter::new());
            queues[lane as usize].push_back(Arc::clone(&waiter));
            waiter
        };
        loop {
            // The queues lock guards the served flag: a release that pops
            // this waiter marks it done while holding that lock, so a
            // timing-out waiter either sees done or is still queued.
            let mut queues = self.queues.lock().unwrap_or_else(|p| p.into_inner());
            if *waiter.done.lock().unwrap_or_else(|p| p.into_inner()) {
                return Some(GovernorPermit {
                    governor: Arc::clone(self),
                });
            }
            let now = Instant::now();
            if now >= deadline {
                if let Some(position) = queues[lane as usize]
                    .iter()
                    .position(|w| Arc::ptr_eq(w, &waiter))
                {
                    queues[lane as usize].remove(position);
                    return None;
                }
                // Not queued any more: the slot was handed to us.
                return Some(GovernorPermit {
                    governor: Arc::clone(self),
                });
            }
            let left = (deadline - now).min(WAIT_SLICE);
            let (guard, _) = waiter
                .cv
                .wait_timeout(queues, left)
                .unwrap_or_else(|p| p.into_inner());
            drop(guard);
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
        let mut queues = self.queues.lock().unwrap_or_else(|p| p.into_inner());
        // The slot goes straight to the highest-priority waiter, if any.
        let mut handed = false;
        for lane in queues.iter_mut() {
            if let Some(waiter) = lane.pop_front() {
                if let Ok(mut done) = waiter.done.lock() {
                    *done = true;
                }
                waiter.cv.notify_all();
                handed = true;
                break;
            }
        }
        drop(queues);
        if !handed {
            if let Ok(mut in_use) = self.in_use.lock() {
                *in_use = in_use.saturating_sub(1);
            }
        }
        // Outside the queues lock: the summary reads them.
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
