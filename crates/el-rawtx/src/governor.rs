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

use el_model::sync::Mutex;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
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
        })
    }

    /// Tor or a provider refused a stream (EAGAIN, 429, proxy unavailable):
    /// hold every new stream for a moment instead of hammering. The backoff
    /// doubles per pause up to `MAX_BACKOFF` and is reset by any success.
    pub fn pause(&self) {
        let mut backoff = self.backoff.lock().unwrap_or_else(|p| p.into_inner());
        *backoff = (*backoff * 2).min(MAX_BACKOFF);
        if let Ok(mut until) = self.pause_until.lock() {
            *until = Some(Instant::now() + *backoff);
        }
        self.pauses.fetch_add(1, Ordering::Relaxed);
    }

    /// A stream succeeded: Tor is healthy again. Lift the pause at once and
    /// wake every waiter so nobody waits out the rest of a pause.
    pub fn healthy(&self) {
        if let Ok(mut backoff) = self.backoff.lock() {
            *backoff = BASE_BACKOFF;
        }
        if let Ok(mut until) = self.pause_until.lock() {
            *until = None;
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
                Some(until) if until > now => {
                    std::thread::sleep(until.min(deadline) - now);
                }
                _ => return true,
            }
        }
    }

    /// Hold one Tor stream until `deadline`. The highest-priority waiter gets
    /// the next freed slot; a wallet stream never queues behind a decoy.
    pub fn acquire(self: &Arc<Self>, lane: Lane, deadline: Instant) -> Option<GovernorPermit> {
        loop {
            if !self.pause_wait(deadline) {
                return None;
            }
            {
                let mut in_use = self.in_use.lock().unwrap_or_else(|p| p.into_inner());
                if *in_use < self.total {
                    *in_use += 1;
                    return Some(GovernorPermit {
                        governor: Arc::clone(self),
                    });
                }
            }
            let waiter = Arc::new(Waiter::new());
            {
                let mut queues = self.queues.lock().unwrap_or_else(|p| p.into_inner());
                queues[lane as usize].push_back(Arc::clone(&waiter));
            }
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
    }

    fn release(&self) {
        let mut queues = self.queues.lock().unwrap_or_else(|p| p.into_inner());
        for lane in queues.iter_mut() {
            if let Some(waiter) = lane.pop_front() {
                if let Ok(mut done) = waiter.done.lock() {
                    *done = true;
                }
                waiter.cv.notify_all();
                return;
            }
        }
        drop(queues);
        if let Ok(mut in_use) = self.in_use.lock() {
            *in_use = in_use.saturating_sub(1);
        }
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
        governor.pause();
        governor.pause();
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
        governor.pause();
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
}
