//! Chain supervisor: disk backpressure, safe prune gate and pruned-IBD state.
//!
//! This module is deliberately pure (no RPC): the caller supplies the latest
//! Core/index observations and applies the returned actions. All prune
//! decisions are conservative and never delete block files directly.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

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
}

/// Maximum index lag before pausing Core downloads. Core keeps at least
/// `MIN_BLOCKS_TO_KEEP` (288) blocks behind its tip, so a configured value is
/// capped by the real retention window with a 32-block safety margin.
pub fn lag_limit(core_tip: u32, pruneheight: u32, configured: u32) -> u32 {
    if configured == 0 {
        return 0;
    }
    if core_tip == 0 || pruneheight == 0 || pruneheight >= core_tip {
        return configured;
    }
    let retention = core_tip.saturating_sub(pruneheight);
    configured.min(retention.saturating_sub(32)).max(1)
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
        }
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn paused(&self) -> bool {
        self.paused
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

        // Prune gate: only manual-pruned Core, never while paused, only with a
        // durable index ahead of the cut, and with a minimum gain.
        if self.cfg.prune_enabled && input.core_pruned && !self.paused {
            let safe = safe_prune_height(
                input.committed_height,
                input.readers_min,
                input.core_tip,
                self.cfg.undo_margin,
                self.cfg.core_recent_margin,
            );
            let advanced = safe > input.core_pruneheight
                && self
                    .last_prune
                    .is_none_or(|last| safe >= last.saturating_add(self.cfg.min_gain));
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

    pub fn on_prune_requested(&mut self) {
        self.state = State::Pruning;
    }

    pub fn on_pruned(&mut self, height: u32) {
        self.last_prune = Some(height);
        self.state = State::CatchUp;
    }

    pub fn on_prune_failed(&mut self) {
        // Do not update last_prune: retry on the next tick after backoff.
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
    fn lag_limit_respects_core_retention() {
        // Core's normal 288-block retention caps a 256 configured lag.
        assert_eq!(lag_limit(1_000, 712, 256), 256);
        // A larger retention allows the configured value.
        assert_eq!(lag_limit(1_000, 500, 256), 256);
        // A smaller retention (regtest/fastprune) shrinks the limit.
        assert_eq!(lag_limit(1_000, 950, 256), 18);
        // Manual pruning (pruneheight 0) uses the configured value.
        assert_eq!(lag_limit(1_000, 0, 256), 256);
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
    fn pause_blocks_pruning() {
        let mut s = Supervisor::new(cfg());
        let mut i = input(100, 200);
        i.free_bytes = Some(10);
        assert_eq!(s.tick(&i), vec![Action::PauseDownloads]);
        i.free_bytes = Some(150);
        // Still paused until the resume watermark.
        assert_eq!(s.tick(&i), vec![]);
        i.free_bytes = Some(300);
        assert_eq!(s.tick(&i), vec![Action::ResumeDownloads]);
        i.free_bytes = Some(300);
        assert_eq!(s.tick(&i), vec![Action::Prune(90)]);
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
