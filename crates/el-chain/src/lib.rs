//! Chain supervisor: disk backpressure, safe prune gate and pruned-IBD state.
//!
//! This module is deliberately pure (no RPC): the caller supplies the latest
//! Core/index observations and applies the returned actions. All prune
//! decisions are conservative and never delete block files directly.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ----------------------------------------------------------------- disk space

/// Free bytes on the filesystem containing `path`, or `None` if it cannot be
/// measured (in which case the supervisor does not act on disk).
pub fn disk_free_bytes(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    let c = CString::new(path.to_string_lossy().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut stat) };
    if rc != 0 {
        return None;
    }
    Some(stat.f_bavail as u64 * stat.f_frsize as u64)
}

// -------------------------------------------------------------- reader tracker

/// Heights currently being read from Core (rawtx resolution). The supervisor
/// must never ask Core to prune at or above the earliest active reader.
#[derive(Default)]
pub struct ReaderTracker {
    inner: Mutex<BTreeMap<u32, usize>>,
}

impl ReaderTracker {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn enter(self: &Arc<Self>, height: u32) -> ReaderGuard {
        if let Ok(mut m) = self.inner.lock() {
            *m.entry(height).or_insert(0) += 1;
        }
        ReaderGuard {
            tracker: Arc::clone(self),
            height,
        }
    }

    pub fn earliest(&self) -> Option<u32> {
        self.inner
            .lock()
            .ok()
            .and_then(|m| m.iter().find(|(_, n)| **n > 0).map(|(h, _)| *h))
    }
}

pub struct ReaderGuard {
    tracker: Arc<ReaderTracker>,
    height: u32,
}

impl Drop for ReaderGuard {
    fn drop(&mut self) {
        if let Ok(mut m) = self.tracker.inner.lock() {
            if let Some(n) = m.get_mut(&self.height) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    m.remove(&self.height);
                }
            }
        }
    }
}

// ------------------------------------------------------------------- states

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Bootstrap,
    CatchUp,
    Indexing,
    PruneEligible,
    Pruning,
    PauseDownload,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Bootstrap => "BOOTSTRAP",
            State::CatchUp => "CATCH_UP",
            State::Indexing => "INDEXING",
            State::PruneEligible => "PRUNE_ELIGIBLE",
            State::Pruning => "PRUNING",
            State::PauseDownload => "PAUSE_DOWNLOAD",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    PauseDownloads,
    ResumeDownloads,
    Prune(u32),
}

#[derive(Debug, Clone)]
pub struct SupervisorConfig {
    pub disk_enabled: bool,
    /// Pause Core downloads when free space is below this.
    pub disk_low_bytes: u64,
    /// Resume when free space reaches this (must be > low).
    pub disk_resume_bytes: u64,
    pub prune_enabled: bool,
    /// Blocks kept behind the committed tip for reorgs/undo.
    pub undo_margin: u32,
    /// Blocks kept behind Core's tip to avoid pruning what it still serves.
    pub core_recent_margin: u32,
    /// Do not issue another prune request until the safe height advanced this much.
    pub min_gain: u32,
    /// Minimum wall-clock interval between prune requests. Emergency pruning
    /// on low disk bypasses this.
    pub prune_interval: Duration,
    /// Skip normal pruning while free space is at or above this many bytes.
    /// Emergency pruning (below `disk_low_bytes`) still runs. Once the index
    /// is caught up with a Core that left IBD, the deferral is bypassed so
    /// already-indexed blocks are released; `min_gain` keeps batching after.
    pub prune_defer_free_bytes: Option<u64>,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        SupervisorConfig {
            disk_enabled: true,
            disk_low_bytes: 5 * 1024 * 1024 * 1024,
            disk_resume_bytes: 10 * 1024 * 1024 * 1024,
            prune_enabled: true,
            undo_margin: 288,
            core_recent_margin: 288,
            min_gain: 288,
            prune_interval: Duration::from_secs(30),
            prune_defer_free_bytes: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct TickInput {
    pub committed_height: u32,
    pub core_tip: u32,
    pub core_pruned: bool,
    pub core_pruneheight: u32,
    pub free_bytes: Option<u64>,
    pub readers_min: Option<u32>,
    /// Core is still in IBD. When it is not and the index matches the tip,
    /// the free-space deferral stops holding already-indexed blocks hostage.
    pub core_ibd: bool,
}

/// Maximum index lag allowed when Core is pruning automatically. Core always
/// keeps at least `MIN_BLOCKS_TO_KEEP` (288) blocks, so a lag above
/// `288 - margin` is a race: Core may delete a file that the index still needs.
pub const MAX_SAFE_CORE_LAG: u32 = 224;

/// Maximum index lag before pausing Core downloads. Core keeps at least
/// `MIN_BLOCKS_TO_KEEP` (288) blocks behind its tip, so a configured value is
/// capped both by the real retention window and by `MAX_SAFE_CORE_LAG`.
pub fn lag_limit(core_tip: u32, pruneheight: u32, configured: u32) -> u32 {
    if configured == 0 {
        return 0;
    }
    if core_tip == 0 || pruneheight == 0 || pruneheight >= core_tip {
        return configured.min(MAX_SAFE_CORE_LAG);
    }
    let retention = core_tip.saturating_sub(pruneheight);
    configured
        .min(retention.saturating_sub(32))
        .clamp(1, MAX_SAFE_CORE_LAG)
}

/// Conservative upper bound for `pruneblockchain`. Returns 0 when unsure.
pub fn safe_prune_height(
    committed_height: u32,
    readers_min: Option<u32>,
    core_tip: u32,
    undo_margin: u32,
    core_recent_margin: u32,
) -> u32 {
    if committed_height == 0 || core_tip == 0 {
        return 0;
    }
    let by_undo = committed_height.saturating_sub(undo_margin.max(1));
    let by_reader = match readers_min {
        Some(h) => h.saturating_sub(1),
        None => u32::MAX,
    };
    let by_core = core_tip.saturating_sub(core_recent_margin.max(1));
    by_undo.min(by_reader).min(by_core)
}

pub struct Supervisor {
    cfg: SupervisorConfig,
    state: State,
    paused: bool,
    last_prune: Option<u32>,
    /// Last height we asked Core to prune to. Core prunes at block-file
    /// granularity and can report a much lower height, so retries are
    /// throttled on the request, not on the reported result.
    last_attempt: Option<u32>,
    /// Wall-clock time of the last prune request. Core may flush block files
    /// when asked to prune, so no-op requests are rate-limited as well.
    last_prune_at: Option<Instant>,
    /// Earliest time to retry a failed prune at the same safe height. Without
    /// this, a failed RPC would never be retried until the safe height moved.
    retry_at: Option<Instant>,
    /// Height whose prune RPC failed, eligible for retry at `retry_at`.
    retry_height: Option<u32>,
}

impl Supervisor {
    pub fn new(cfg: SupervisorConfig) -> Self {
        let mut cfg = cfg;
        if cfg.disk_resume_bytes <= cfg.disk_low_bytes {
            cfg.disk_resume_bytes = cfg.disk_low_bytes.saturating_add(1024 * 1024 * 1024);
        }
        Supervisor {
            cfg,
            state: State::Bootstrap,
            paused: false,
            last_prune: None,
            last_attempt: None,
            last_prune_at: None,
            retry_at: None,
            retry_height: None,
        }
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn paused(&self) -> bool {
        self.paused
    }

    /// Replace the tunable parts without losing the current state/hysteresis.
    pub fn update_config(&mut self, cfg: SupervisorConfig) {
        let mut cfg = cfg;
        if cfg.disk_resume_bytes <= cfg.disk_low_bytes {
            cfg.disk_resume_bytes = cfg.disk_low_bytes.saturating_add(1024 * 1024 * 1024);
        }
        self.cfg = cfg;
    }

    pub fn last_prune(&self) -> Option<u32> {
        self.last_prune
    }

    /// Disk backpressure with hysteresis. This is deliberately independent of
    /// Core RPC: a full disk must pause the writers even when Core is slow or
    /// temporarily unreachable.
    pub fn disk_tick(&mut self, free: Option<u64>) -> Vec<Action> {
        let mut actions = Vec::new();
        if self.cfg.disk_enabled {
            if let Some(free) = free {
                if !self.paused && free < self.cfg.disk_low_bytes {
                    self.paused = true;
                    self.state = State::PauseDownload;
                    actions.push(Action::PauseDownloads);
                } else if self.paused && free >= self.cfg.disk_resume_bytes {
                    self.paused = false;
                    self.state = State::CatchUp;
                    actions.push(Action::ResumeDownloads);
                }
            }
        }
        actions
    }

    /// Force the supervisor into the paused state when the indexer detected a
    /// low disk first. Returns `PauseDownloads` once, so the caller can pause
    /// Core even if `disk_tick` would not emit it again.
    pub fn force_disk_pause(&mut self) -> Vec<Action> {
        if !self.cfg.disk_enabled || self.paused {
            return Vec::new();
        }
        self.paused = true;
        self.state = State::PauseDownload;
        vec![Action::PauseDownloads]
    }

    pub fn tick(&mut self, input: &TickInput) -> Vec<Action> {
        let mut actions = self.disk_tick(input.free_bytes);
        if !actions.is_empty() {
            return actions;
        }

        // Prune gate: durable index ahead of the cut, with a minimum gain and
        // a minimum wall-clock interval. It runs even while Core downloads are
        // paused for disk pressure: draining the index and pruning is how we
        // recover free space. Low disk bypasses the interval (emergency prune).
        // With plenty of free space, normal pruning is pure I/O churn —
        // but only while Core is still building: once the index is caught up,
        // the deferral would hold every already-indexed block forever (a
        // from-zero IBD leaves ~90k blocks / ~140 GiB behind the safe cut).
        // Caught up, the gate prunes to the safe max and `min_gain` keeps
        // batching the trickle afterwards.
        // Emergency (low-disk) pruning remains active below the threshold.
        let caught_up = !input.core_ibd && input.committed_height >= input.core_tip;
        let defer_prune = self
            .cfg
            .prune_defer_free_bytes
            .zip(input.free_bytes)
            .map(|(threshold, free)| free >= threshold)
            .unwrap_or(false)
            && !caught_up;
        if self.cfg.prune_enabled && input.core_pruned && !defer_prune {
            let safe = safe_prune_height(
                input.committed_height,
                input.readers_min,
                input.core_tip,
                self.cfg.undo_margin,
                self.cfg.core_recent_margin,
            );
            let emergency = self.cfg.disk_enabled
                && input
                    .free_bytes
                    .map(|free| free < self.cfg.disk_low_bytes)
                    .unwrap_or(false);
            let interval_elapsed = self
                .last_prune_at
                .map(|at| at.elapsed() >= self.cfg.prune_interval)
                .unwrap_or(true);
            // Emergency pruning may be more frequent, but still not once per
            // supervisor tick: Core prunes whole files and a no-op request can
            // force a flush.
            let emergency_interval = Duration::from_secs(5).min(self.cfg.prune_interval);
            let emergency_due = self
                .last_prune_at
                .map(|at| at.elapsed() >= emergency_interval)
                .unwrap_or(true);
            let retry_due = self.retry_at.map(|at| Instant::now() >= at).unwrap_or(true);
            let last_success = self.last_prune.unwrap_or(0);
            let last = last_success.max(self.last_attempt.unwrap_or(0));
            let retry_same = self.retry_height == Some(safe) && retry_due;
            // Emergency pruning (low disk) bypasses the gain and the normal
            // interval, but still requires a durable index ahead of the cut.
            // A failed request at the same safe height may be retried after
            // `retry_at`; otherwise normal pruning needs a new gain.
            let advanced = safe > input.core_pruneheight
                && safe > last_success
                && (safe > self.last_attempt.unwrap_or(0) || retry_same)
                && if emergency {
                    emergency_due
                } else {
                    (retry_same || safe >= last.saturating_add(self.cfg.min_gain))
                        && interval_elapsed
                };
            if advanced && safe > 0 {
                self.state = State::PruneEligible;
                actions.push(Action::Prune(safe));
            }
        }

        if actions.is_empty() && !self.paused {
            self.state = if input.committed_height < input.core_tip {
                State::Indexing
            } else {
                State::CatchUp
            };
        }
        actions
    }

    pub fn on_prune_requested(&mut self, height: u32) {
        self.last_attempt = Some(height);
        self.last_prune_at = Some(Instant::now());
        self.retry_at = None;
        self.retry_height = None;
        self.state = State::Pruning;
    }

    pub fn on_pruned(&mut self, height: u32) {
        self.last_prune = Some(height);
        self.retry_at = None;
        self.retry_height = None;
        self.state = State::CatchUp;
    }

    pub fn on_prune_failed(&mut self) {
        // Keep the height: a transient RPC failure must be retried even if the
        // safe height did not move.
        self.retry_at = Some(Instant::now() + Duration::from_secs(30));
        self.retry_height = self.last_attempt;
        self.state = State::CatchUp;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> SupervisorConfig {
        SupervisorConfig {
            disk_enabled: true,
            disk_low_bytes: 100,
            disk_resume_bytes: 200,
            prune_enabled: true,
            undo_margin: 10,
            core_recent_margin: 10,
            min_gain: 5,
            prune_interval: Duration::ZERO,
            prune_defer_free_bytes: None,
        }
    }

    fn input(committed: u32, core_tip: u32) -> TickInput {
        TickInput {
            committed_height: committed,
            core_tip,
            core_pruned: true,
            core_pruneheight: 0,
            free_bytes: Some(1000),
            readers_min: None,
            core_ibd: true,
        }
    }

    fn caught_up_input(committed: u32, core_tip: u32) -> TickInput {
        TickInput {
            committed_height: committed,
            core_tip,
            core_pruned: true,
            core_pruneheight: 0,
            free_bytes: Some(1000),
            readers_min: None,
            core_ibd: false,
        }
    }

    #[test]
    fn disk_tick_is_independent_of_core_input() {
        let mut s = Supervisor::new(cfg());
        assert_eq!(s.disk_tick(Some(50)), vec![Action::PauseDownloads]);
        assert!(s.paused());
        assert_eq!(s.disk_tick(Some(150)), Vec::new());
        assert_eq!(s.disk_tick(Some(250)), vec![Action::ResumeDownloads]);
        assert!(!s.paused());
    }

    #[test]
    fn force_disk_pause_emits_once() {
        let mut s = Supervisor::new(cfg());
        assert_eq!(s.force_disk_pause(), vec![Action::PauseDownloads]);
        assert!(s.paused());
        assert!(s.force_disk_pause().is_empty());
        assert_eq!(s.disk_tick(Some(250)), vec![Action::ResumeDownloads]);
        assert!(!s.paused());
    }

    #[test]
    fn prune_is_deferred_while_free_space_is_high() {
        let mut c = cfg();
        c.prune_defer_free_bytes = Some(500);
        let mut s = Supervisor::new(c);
        let mut i = input(100, 200);
        i.core_pruneheight = 0;
        i.free_bytes = Some(1000);
        assert!(s.tick(&i).is_empty(), "high free space defers pruning");
        i.free_bytes = Some(400);
        assert!(
            s.tick(&i).iter().any(|a| matches!(a, Action::Prune(_))),
            "below the threshold the normal prune gate runs"
        );
    }

    #[test]
    fn caught_up_prune_bypasses_the_free_space_deferral() {
        let mut c = cfg();
        c.prune_defer_free_bytes = Some(500);
        let mut s = Supervisor::new(c.clone());
        // Core out of IBD and the index at its tip: the 1000 free bytes would
        // defer pruning forever while the safe cut sits 10 blocks below the
        // index. Caught up, the gate prunes to the safe max.
        let mut i = caught_up_input(100, 100);
        i.core_pruneheight = 0;
        i.free_bytes = Some(1000);
        assert_eq!(
            s.tick(&i),
            vec![Action::Prune(90)],
            "caught up prunes to committed - undo_margin"
        );
        // Core still in IBD at the same heights: the deferral holds.
        let mut s2 = Supervisor::new(c.clone());
        let mut j = input(100, 100);
        j.core_pruneheight = 0;
        j.free_bytes = Some(1000);
        assert!(
            s2.tick(&j).is_empty(),
            "IBD defers pruning on high free space"
        );
    }

    #[test]
    fn caught_up_live_mode_batches_after_the_transition_prune() {
        let mut c = cfg();
        c.prune_defer_free_bytes = Some(500);
        c.min_gain = 5;
        let mut s = Supervisor::new(c);
        let mut i = caught_up_input(100, 100);
        i.core_pruneheight = 0;
        i.free_bytes = Some(1000);
        assert_eq!(s.tick(&i), vec![Action::Prune(90)]);
        s.on_prune_requested(90);
        s.on_pruned(90);
        // One block later in live mode: below min_gain (5), no request even
        // with the deferral bypassed. The batching keeps live churn low.
        let mut j = caught_up_input(101, 101);
        j.core_pruneheight = 90;
        j.free_bytes = Some(1000);
        assert!(s.tick(&j).is_empty(), "live gain below min_gain: no prune");
    }

    #[test]
    fn lag_limit_respects_core_retention() {
        // Huge retention still cannot exceed the hard 288-block floor margin.
        assert_eq!(lag_limit(1_000, 200, 512), 224);
        // Core's normal 288-block retention leaves 256 by retention alone,
        // but the hard safety cap is 224.
        assert_eq!(lag_limit(1_000, 712, 256), 224);
        // A larger retention also stops at the hard cap.
        assert_eq!(lag_limit(1_000, 500, 256), 224);
        // A smaller retention (regtest/fastprune) shrinks the limit.
        assert_eq!(lag_limit(1_000, 950, 256), 18);
        // No pruned blocks yet: still cap the automatic guard.
        assert_eq!(lag_limit(1_000, 0, 256), 224);
        // 0 disables the guard.
        assert_eq!(lag_limit(1_000, 712, 0), 0);
    }

    #[test]
    fn gate_uses_the_minimum_margin() {
        assert_eq!(safe_prune_height(100, None, 200, 10, 10), 90);
        assert_eq!(safe_prune_height(100, Some(50), 200, 10, 10), 49);
        assert_eq!(safe_prune_height(100, None, 20, 10, 10), 10);
        assert_eq!(safe_prune_height(0, None, 200, 10, 10), 0);
        assert_eq!(safe_prune_height(100, Some(0), 200, 10, 10), 0);
    }

    #[test]
    fn disk_hysteresis_pauses_once_and_resumes_once() {
        let mut s = Supervisor::new(cfg());
        let mut i = input(100, 200);
        i.core_pruned = false; // isolate the disk state from the prune gate
        i.free_bytes = Some(50);
        assert_eq!(s.tick(&i), vec![Action::PauseDownloads]);
        assert_eq!(s.state(), State::PauseDownload);
        assert!(s.paused());
        assert!(s.tick(&i).is_empty(), "must not pause twice");

        i.free_bytes = Some(150);
        assert!(s.tick(&i).is_empty(), "still below resume watermark");
        i.free_bytes = Some(250);
        assert_eq!(s.tick(&i), vec![Action::ResumeDownloads]);
        assert!(!s.paused());
    }

    #[test]
    fn prune_requires_durable_index_and_gain() {
        let mut s = Supervisor::new(cfg());
        let mut i = input(100, 200);
        assert_eq!(s.tick(&i), vec![Action::Prune(90)]);
        s.on_pruned(90);

        // Same safe height: no new request.
        assert!(s.tick(&i).is_empty());
        // Advanced less than min_gain: no request.
        i.committed_height = 103;
        assert_eq!(safe_prune_height(103, None, 200, 10, 10), 93);
        assert!(s.tick(&i).is_empty());
        // Enough gain.
        i.committed_height = 110;
        assert_eq!(s.tick(&i), vec![Action::Prune(100)]);
    }

    #[test]
    fn prune_retry_waits_for_safe_gain_even_if_core_reports_less() {
        let mut s = Supervisor::new(cfg());
        let mut i = input(100, 200);
        i.core_pruneheight = 80;
        assert_eq!(s.tick(&i), vec![Action::Prune(90)]);
        s.on_prune_requested(90);
        // Core could only prune up to a block-file boundary, far below the
        // requested height. That must not trigger a request on every tick.
        s.on_pruned(80);
        assert!(s.tick(&i).is_empty(), "same safe height: no retry");
        i.committed_height = 104;
        assert!(s.tick(&i).is_empty(), "gain below min_gain: no retry");
        i.committed_height = 110;
        assert_eq!(s.tick(&i), vec![Action::Prune(100)]);
    }

    #[test]
    fn failed_prune_retries_the_same_height_after_backoff() {
        let mut s = Supervisor::new(cfg());
        let mut i = input(100, 200);
        i.core_pruneheight = 80;
        assert_eq!(s.tick(&i), vec![Action::Prune(90)]);
        s.on_prune_requested(90);
        s.on_prune_failed();
        assert!(
            s.tick(&i).is_empty(),
            "a failed prune must not spin immediately"
        );
        s.retry_at = Some(Instant::now() - Duration::from_secs(1));
        assert_eq!(s.tick(&i), vec![Action::Prune(90)]);
    }

    #[test]
    fn readers_block_pruning() {
        let mut s = Supervisor::new(cfg());
        let mut i = input(100, 200);
        i.readers_min = Some(95);
        // by_reader = 94, by_undo = 90 -> still 90.
        assert_eq!(s.tick(&i), vec![Action::Prune(90)]);
        s.on_pruned(90);
        i.committed_height = 120;
        i.readers_min = Some(95);
        // by_reader = 94, by_undo = 110 -> 94, but gain 4 < 5.
        assert!(s.tick(&i).is_empty());
        i.readers_min = Some(50);
        // 49 is below the last pruned 90: never go backwards.
        assert!(s.tick(&i).is_empty());
    }

    #[test]
    fn prune_runs_while_downloads_are_paused() {
        let mut s = Supervisor::new(cfg());
        let mut i = input(100, 200);
        i.free_bytes = Some(10);
        assert_eq!(s.tick(&i), vec![Action::PauseDownloads]);
        i.free_bytes = Some(150);
        // Downloads stay paused, but the prune gate still runs so indexing
        // can free disk space.
        assert_eq!(s.tick(&i), vec![Action::Prune(90)]);
        s.on_pruned(90);
        i.free_bytes = Some(300);
        assert_eq!(s.tick(&i), vec![Action::ResumeDownloads]);
        i.free_bytes = Some(300);
        assert!(s.tick(&i).is_empty());
    }

    #[test]
    fn prune_interval_and_emergency() {
        let mut c = cfg();
        c.prune_interval = Duration::from_secs(3600);
        let mut s = Supervisor::new(c);
        let mut i = input(100, 200);
        assert_eq!(s.tick(&i), vec![Action::Prune(90)]);
        s.on_prune_requested(90);
        s.on_pruned(90);
        i.committed_height = 110;
        assert!(
            s.tick(&i).is_empty(),
            "time interval must throttle no-op prune requests"
        );

        // A fresh supervisor with low disk prunes on the next tick even though
        // the download pause action was emitted first; there is no prior
        // request to rate-limit.
        let mut e = Supervisor::new(cfg());
        let mut low = input(100, 200);
        low.free_bytes = Some(50);
        assert_eq!(e.tick(&low), vec![Action::PauseDownloads]);
        assert_eq!(e.tick(&low), vec![Action::Prune(90)]);
    }

    #[test]
    fn reader_tracker_reports_earliest() {
        let t = ReaderTracker::new();
        let g1 = t.enter(100);
        let g2 = t.enter(50);
        let g3 = t.enter(150);
        assert_eq!(t.earliest(), Some(50));
        drop(g2);
        assert_eq!(t.earliest(), Some(100));
        drop(g1);
        assert_eq!(t.earliest(), Some(150));
        drop(g3);
        assert_eq!(t.earliest(), None);
    }

    #[test]
    fn disk_free_of_existing_path() {
        let free = disk_free_bytes(Path::new("/"));
        assert!(free.unwrap_or(0) > 0);
    }
}
