//! A mutex without poisoning, and the names of the threads that may fail
//! alone.
//!
//! The release binary unwinds on panic (AUD-173). A panic hook aborts the
//! process when the panicking thread is not one of [`CONTAINED_THREADS`]:
//! the index, the commit, the prune gate, the mempool poller and the notifier
//! keep the crash-only model (RocksDB/WAL recovery on restart). A thread that
//! serves one wallet connection ends alone instead.
//!
//! `std::sync::Mutex` marks itself poisoned when its holder panics, and most
//! callers skip a poisoned lock (`if let Ok(guard) = m.lock()`). After a
//! contained panic that would switch a shared cache or registry off for the
//! rest of the process, silently. [`Mutex`] hands the guard out anyway: the
//! protected values are maps and counters updated in one step, so the data a
//! panicking holder leaves behind is valid. `lock` keeps std's signature, so
//! call sites do not change.

use std::sync::{LockResult, MutexGuard, TryLockError, TryLockResult};

/// Connection thread of the Electrum server.
pub const THREAD_CLIENT: &str = "el-client";
/// One request line of a connection that runs beside its reader.
pub const THREAD_LINE: &str = "el-line";
/// One worker of a JSON-RPC batch.
pub const THREAD_BATCH: &str = "el-batch";
/// One attempt of an external block download.
pub const THREAD_FETCH: &str = "el-fetch";

/// Threads whose panic ends that thread only. Everything else aborts.
pub const CONTAINED_THREADS: [&str; 4] = [THREAD_CLIENT, THREAD_LINE, THREAD_BATCH, THREAD_FETCH];

/// Whether a panic in the thread called `name` is contained.
pub fn panic_is_contained(name: Option<&str>) -> bool {
    name.is_some_and(|name| CONTAINED_THREADS.contains(&name))
}

/// `std::sync::Mutex` that never reports poisoning.
#[derive(Debug, Default)]
pub struct Mutex<T: ?Sized>(std::sync::Mutex<T>);

impl<T> Mutex<T> {
    pub const fn new(value: T) -> Self {
        Mutex(std::sync::Mutex::new(value))
    }

    pub fn into_inner(self) -> LockResult<T> {
        Ok(self.0.into_inner().unwrap_or_else(|p| p.into_inner()))
    }
}

impl<T: ?Sized> Mutex<T> {
    /// Always `Ok`: a lock whose holder panicked is handed out as it is. The
    /// poison mark is cleared, so a `Condvar` waiting on this lock does not
    /// report it either.
    pub fn lock(&self) -> LockResult<MutexGuard<'_, T>> {
        Ok(self.0.lock().unwrap_or_else(|p| {
            self.0.clear_poison();
            p.into_inner()
        }))
    }

    /// `Err` only when the lock is held right now.
    pub fn try_lock(&self) -> TryLockResult<MutexGuard<'_, T>> {
        match self.0.try_lock() {
            Ok(guard) => Ok(guard),
            Err(TryLockError::Poisoned(p)) => {
                self.0.clear_poison();
                Ok(p.into_inner())
            }
            Err(TryLockError::WouldBlock) => Err(TryLockError::WouldBlock),
        }
    }

    pub fn get_mut(&mut self) -> LockResult<&mut T> {
        Ok(self.0.get_mut().unwrap_or_else(|p| p.into_inner()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn a_panicking_holder_does_not_switch_the_lock_off() {
        let shared = Arc::new(Mutex::new(vec![1u32]));
        let std_shared = Arc::new(std::sync::Mutex::new(vec![1u32]));
        let (a, b) = (Arc::clone(&shared), Arc::clone(&std_shared));
        let _ = std::thread::spawn(move || {
            let _guard = a.lock().unwrap();
            let _std_guard = b.lock().unwrap();
            panic!("holder dies");
        })
        .join();
        // std: every later `if let Ok(..) = lock()` is skipped for good.
        assert!(std_shared.lock().is_err());
        // This one keeps working, with the data intact.
        shared.lock().unwrap().push(2);
        assert_eq!(*shared.lock().unwrap(), vec![1, 2]);
        assert!(shared.try_lock().is_ok());
        // The mark is gone: a condition variable on this lock sees a clean one.
        let guard = shared.lock().unwrap();
        let cv = std::sync::Condvar::new();
        assert!(cv
            .wait_timeout(guard, std::time::Duration::from_millis(1))
            .is_ok());
    }

    #[test]
    fn try_lock_reports_a_held_lock() {
        let m = Mutex::new(0u8);
        let held = m.lock().unwrap();
        assert!(matches!(m.try_lock(), Err(TryLockError::WouldBlock)));
        drop(held);
        assert!(m.try_lock().is_ok());
    }

    #[test]
    fn only_wallet_facing_threads_are_contained() {
        for name in CONTAINED_THREADS {
            assert!(panic_is_contained(Some(name)), "{name}");
        }
        // The index, commit, prune, tuning and background threads abort.
        for name in [
            "main",
            "el-prune",
            "el-tune",
            "el-decoys",
            "el-txbatch",
            "el-core-obs",
        ] {
            assert!(!panic_is_contained(Some(name)), "{name}");
        }
        assert!(!panic_is_contained(None));
    }
}
