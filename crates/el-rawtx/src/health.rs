//! Per-endpoint health shared by the Esplora and P2P sources.
//!
//! A failure cools an endpoint (provider or peer) for a time that depends on
//! what failed: a slow Tor circuit or a dropped connection is retried soon
//! with exponential backoff, a rate limit honours `Retry-After`, and only a
//! hard failure (wrong data, wrong services) gets the full cooldown. A success
//! forgets the endpoint's history (AUD-155).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How a failed attempt reflects on the endpoint that served it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Blame {
    /// Timeout, stalled or truncated body, refused/unreachable through a Tor
    /// exit, 5xx: the next circuit may work. Short backoff.
    Transient,
    /// 429/503: the provider asked us to slow down (`Retry-After`, if sent).
    /// Over Tor the limit is per exit IP, so this is still short.
    RateLimited(Option<Duration>),
    /// Wrong services, `notfound`, invalid or oversized block, other 4xx: the
    /// full `provider_cooldown_seconds`.
    Hard,
    /// The endpoint never answered: no connection, or no response byte
    /// before the connect/first-byte limit. Backs off like `Transient` but
    /// up to `UNREACHABLE_CAP`, so an endpoint that is unreachable from this
    /// host (a provider that drops Tor exits, a dead peer) stops costing an
    /// attempt per block while the others work (AUD-160). One success
    /// forgets it.
    Unreachable,
}

/// Transient failures back off from this, doubling per consecutive failure.
pub(crate) const TRANSIENT_BASE: Duration = Duration::from_secs(5);
/// Upper bound of the transient backoff of a provider.
pub(crate) const TRANSIENT_CAP: Duration = Duration::from_secs(60);
/// A rate limit backs off from this when the server sent no `Retry-After`.
pub(crate) const LIMITED_BASE: Duration = Duration::from_secs(15);
/// Upper bound of a rate-limit pause (also caps `Retry-After`).
pub(crate) const LIMITED_CAP: Duration = Duration::from_secs(120);

/// Upper bound of the backoff of an endpoint that never answers.
pub(crate) const UNREACHABLE_CAP: Duration = Duration::from_secs(600);
/// Over Tor a 429 is about one exit, not the provider: the provider is paused
/// only when this many distinct circuits were rate limited within
/// `LIMITED_WINDOW` (AUD-160).
pub(crate) const LIMITED_CIRCUITS: usize = 3;
pub(crate) const LIMITED_WINDOW: Duration = Duration::from_secs(60);

/// `base * 2^(streak-1)`, capped. `streak` counts the failure being recorded.
pub(crate) fn backoff(base: Duration, streak: u32, cap: Duration) -> Duration {
    let shift = streak.saturating_sub(1).min(16);
    base.saturating_mul(1u32 << shift).min(cap)
}

#[derive(Clone, Copy, Debug)]
struct Health {
    until: Instant,
    streak: u32,
    limited: bool,
}

/// Cooldowns keyed by endpoint (provider base URL or peer address).
#[derive(Default)]
pub(crate) struct HealthMap {
    inner: Mutex<HashMap<String, Health>>,
    /// Recent per-circuit rate limits (Tor), by endpoint.
    limited: Mutex<HashMap<String, Vec<Instant>>>,
}

impl HealthMap {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Whether `key` is cooling down right now.
    pub(crate) fn cooling(&self, key: &str) -> bool {
        self.until(key).is_some_and(|until| until > Instant::now())
    }

    /// When `key` may be tried again, if it has a recorded failure.
    pub(crate) fn until(&self, key: &str) -> Option<Instant> {
        self.inner
            .lock()
            .ok()
            .and_then(|m| m.get(key).map(|h| h.until))
    }

    /// Whether any endpoint is inside a rate-limit pause.
    pub(crate) fn any_limited(&self) -> bool {
        let now = Instant::now();
        self.inner
            .lock()
            .map(|m| m.values().any(|h| h.limited && h.until > now))
            .unwrap_or(false)
    }

    /// Record a failure. `transient_base`/`transient_cap` let P2P use a
    /// longer backoff than Esplora (a dead peer costs a whole timeout);
    /// `hard` is the full cooldown.
    pub(crate) fn fail(
        &self,
        key: &str,
        blame: Blame,
        transient_base: Duration,
        transient_cap: Duration,
        hard: Duration,
    ) -> Duration {
        let Ok(mut m) = self.inner.lock() else {
            return Duration::ZERO;
        };
        let streak = m.get(key).map_or(0, |h| h.streak).saturating_add(1);
        let (pause, limited) = match blame {
            Blame::Transient => (backoff(transient_base, streak, transient_cap), false),
            Blame::RateLimited(retry_after) => {
                let own = backoff(LIMITED_BASE, streak, LIMITED_CAP);
                let asked = retry_after.unwrap_or(Duration::ZERO).min(LIMITED_CAP);
                (own.max(asked), true)
            }
            Blame::Hard => (hard, false),
            Blame::Unreachable => (backoff(transient_base, streak, UNREACHABLE_CAP), false),
        };
        m.insert(
            key.to_string(),
            Health {
                until: Instant::now() + pause,
                streak,
                limited,
            },
        );
        pause
    }

    /// A success forgets every past failure of `key`.
    /// Record a rate limit seen on one Tor circuit of `key`. Returns true when
    /// enough distinct circuits were limited recently that the endpoint
    /// itself should be paused.
    pub(crate) fn circuit_limited(&self, key: &str) -> bool {
        let Ok(mut m) = self.limited.lock() else {
            return false;
        };
        let now = Instant::now();
        let hits = m.entry(key.to_string()).or_default();
        hits.retain(|t| now.duration_since(*t) < LIMITED_WINDOW);
        hits.push(now);
        if hits.len() >= LIMITED_CIRCUITS {
            hits.clear();
            return true;
        }
        false
    }

    pub(crate) fn succeed(&self, key: &str) {
        if let Ok(mut m) = self.inner.lock() {
            m.remove(key);
        }
    }

    /// Forget `key`'s cooldown but keep its streak, so a forced attempt that
    /// fails again backs off further.
    pub(crate) fn release(&self, key: &str) {
        if let Ok(mut m) = self.inner.lock() {
            if let Some(h) = m.get_mut(key) {
                h.until = Instant::now();
            }
        }
    }

    /// Keep only the listed keys (a refreshed peer list).
    pub(crate) fn retain(&self, keep: impl Fn(&str) -> bool) {
        if let Ok(mut m) = self.inner.lock() {
            m.retain(|k, _| keep(k));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        let s = Duration::from_secs;
        assert_eq!(backoff(s(5), 1, s(60)), s(5));
        assert_eq!(backoff(s(5), 2, s(60)), s(10));
        assert_eq!(backoff(s(5), 4, s(60)), s(40));
        assert_eq!(backoff(s(5), 5, s(60)), s(60));
        assert_eq!(backoff(s(5), 500, s(60)), s(60));
    }

    #[test]
    fn rate_limit_honours_retry_after_and_success_resets() {
        let h = HealthMap::new();
        let s = Duration::from_secs;
        let pause = h.fail("a", Blame::RateLimited(Some(s(90))), s(5), s(60), s(300));
        assert_eq!(pause, s(90));
        assert!(h.any_limited());
        // Retry-After above the cap is capped.
        let pause = h.fail("b", Blame::RateLimited(Some(s(3600))), s(5), s(60), s(300));
        assert_eq!(pause, LIMITED_CAP);
        h.succeed("a");
        h.succeed("b");
        assert!(!h.any_limited());
        assert_eq!(h.fail("a", Blame::Transient, s(5), s(60), s(300)), s(5));
        assert_eq!(h.fail("a", Blame::Transient, s(5), s(60), s(300)), s(10));
        h.succeed("a");
        assert_eq!(h.fail("a", Blame::Transient, s(5), s(60), s(300)), s(5));
        assert_eq!(h.fail("a", Blame::Hard, s(5), s(60), s(300)), s(300));
    }

    #[test]
    fn an_unreachable_endpoint_backs_off_up_to_ten_minutes() {
        let h = HealthMap::new();
        let s = Duration::from_secs;
        let mut pause = Duration::ZERO;
        for _ in 0..12 {
            pause = h.fail("a", Blame::Unreachable, s(5), s(60), s(300));
        }
        assert_eq!(pause, UNREACHABLE_CAP);
        h.succeed("a");
        assert_eq!(h.fail("a", Blame::Unreachable, s(5), s(60), s(300)), s(5));
    }

    #[test]
    fn one_limited_circuit_does_not_pause_the_provider() {
        let h = HealthMap::new();
        assert!(!h.circuit_limited("p"));
        assert!(!h.circuit_limited("p"));
        assert!(h.circuit_limited("p"));
        assert!(!h.circuit_limited("p"), "the window restarts after a pause");
        assert!(!h.circuit_limited("q"));
    }
}
