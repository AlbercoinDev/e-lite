//! Reader threads of one catch-up call.
//!
//! Readers fetch, decode and prefetch blocks in parallel; the apply thread
//! consumes them in order. Two bounds keep memory flat:
//!
//! * `window` permits cap the blocks that are prepared but not yet applied
//!   (in a reader, in the channel or parked out of order);
//! * the result channel holds at most `2 * workers` finished items.
//!
//! The pool owns its teardown. Stopping must work from any state, including
//! the saturated one: every permit is held by a prepared block the apply loop
//! will never consume, and every reader is asleep waiting for a permit. The
//! readers keep the permit channel alive themselves, so dropping the pool's
//! sender does not wake them; `shutdown` hands out wake-up permits and drains
//! the results until the last reader has left (AUD-169).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

pub(crate) struct ReaderPool<T> {
    rx: Receiver<(usize, T)>,
    permit_tx: Option<SyncSender<()>>,
    stop: Arc<AtomicBool>,
    handles: Vec<JoinHandle<()>>,
}

impl<T: Send + 'static> ReaderPool<T> {
    /// Start `workers` readers over the items `0..items`. `work` runs once per
    /// item, on whichever reader takes it; each reader gets its own clone, so
    /// per-thread handles (an RPC client) are not shared.
    pub(crate) fn spawn<F>(workers: usize, window: usize, items: usize, work: F) -> Self
    where
        F: Fn(usize) -> T + Send + Clone + 'static,
    {
        let workers = workers.max(1);
        let window = window.max(1);
        let (tx, rx) = sync_channel::<(usize, T)>(workers * 2);
        let (permit_tx, permit_rx) = sync_channel::<()>(window);
        for _ in 0..window {
            let _ = permit_tx.try_send(());
        }
        let permit_rx = Arc::new(Mutex::new(permit_rx));
        let next = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            let work = work.clone();
            let next = Arc::clone(&next);
            let stop = Arc::clone(&stop);
            let permit_rx = Arc::clone(&permit_rx);
            let permit_tx = permit_tx.clone();
            let tx = tx.clone();
            handles.push(std::thread::spawn(move || loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let acquired = permit_rx
                    .lock()
                    .map(|rx| rx.recv().is_ok())
                    .unwrap_or(false);
                if !acquired || stop.load(Ordering::Relaxed) {
                    break;
                }
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= items {
                    // Give the permit back: the apply loop is the only other
                    // producer and this reader is done.
                    let _ = permit_tx.try_send(());
                    break;
                }
                if tx.send((i, work(i))).is_err() {
                    break;
                }
            }));
        }
        ReaderPool {
            rx,
            permit_tx: Some(permit_tx),
            stop,
            handles,
        }
    }

    /// Next finished item, in completion order (not item order).
    pub(crate) fn recv_timeout(&self, timeout: Duration) -> Result<(usize, T), RecvTimeoutError> {
        self.rx.recv_timeout(timeout)
    }

    /// Return the permit of an item the apply loop has consumed.
    pub(crate) fn release(&self) {
        if let Some(tx) = &self.permit_tx {
            let _ = tx.try_send(());
        }
    }

    /// Stop the readers, wait for them and return how many finished items
    /// were still queued (prepared work that will not be applied).
    pub(crate) fn finish(mut self) -> u64 {
        self.shutdown()
    }
}

impl<T> ReaderPool<T> {
    fn shutdown(&mut self) -> u64 {
        self.stop.store(true, Ordering::Relaxed);
        let permit_tx = self.permit_tx.take();
        let mut queued = 0u64;
        if !self.handles.is_empty() {
            loop {
                // A reader asleep on the permit channel needs a permit to see
                // the stop flag. Repeat: the channel may be too small to wake
                // them all at once.
                if let Some(tx) = &permit_tx {
                    for _ in 0..self.handles.len() {
                        let _ = tx.try_send(());
                    }
                }
                // Draining also frees a reader blocked on a full result
                // channel. Disconnected = every reader has returned.
                match self.rx.recv_timeout(Duration::from_millis(20)) {
                    Ok(_) => queued += 1,
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        }
        drop(permit_tx);
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
        queued
    }
}

impl<T> Drop for ReaderPool<T> {
    fn drop(&mut self) {
        // `finish` already ran unless the caller left early (an error path).
        // Never leave readers behind: they hold store and RPC handles.
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;
    use std::time::Instant;

    /// Run `f` on another thread and fail if it does not return in time: a
    /// teardown bug here is a hang, not a wrong value.
    fn within<R: Send + 'static>(limit: Duration, f: impl FnOnce() -> R + Send + 'static) -> R {
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(limit)
            .expect("the reader pool did not stop: readers are still blocked")
    }

    #[test]
    fn items_arrive_once_and_the_pool_drains() {
        let got = within(Duration::from_secs(10), || {
            let pool = ReaderPool::spawn(4, 8, 100, |i| i * 2);
            let mut seen = [false; 100];
            for _ in 0..100 {
                let (i, v) = pool.recv_timeout(Duration::from_secs(5)).unwrap();
                assert_eq!(v, i * 2);
                assert!(!seen[i], "item {i} delivered twice");
                seen[i] = true;
                pool.release();
            }
            (seen.iter().all(|s| *s), pool.finish())
        });
        assert_eq!(got, (true, 0));
    }

    /// The saturated state: the apply loop stops consuming (shutdown, disk
    /// pause, scheduler switch) while every permit is held by a prepared
    /// block. Before AUD-169 the readers slept on the permit channel forever
    /// and the join never returned.
    #[test]
    fn a_saturated_pool_stops() {
        let queued = within(Duration::from_secs(10), || {
            let window = 4;
            let pool = ReaderPool::spawn(4, window, 1_000, |i| i);
            // Consume one item and keep its permit, as the apply loop does
            // when it breaks right after taking a block.
            let _ = pool.recv_timeout(Duration::from_secs(5)).unwrap();
            // Let the readers use every remaining permit and go to sleep.
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut parked = Vec::new();
            while parked.len() < window - 1 && Instant::now() < deadline {
                if let Ok(item) = pool.recv_timeout(Duration::from_millis(50)) {
                    parked.push(item);
                }
            }
            assert_eq!(parked.len(), window - 1, "the window did not fill");
            std::thread::sleep(Duration::from_millis(50));
            pool.finish()
        });
        // Everything prepared was already taken off the channel.
        assert_eq!(queued, 0);
    }

    #[test]
    fn a_window_smaller_than_the_readers_still_stops() {
        within(Duration::from_secs(10), || {
            let pool = ReaderPool::spawn(8, 2, 1_000, |i| i);
            let _ = pool.recv_timeout(Duration::from_secs(5)).unwrap();
            std::thread::sleep(Duration::from_millis(50));
            pool.finish()
        });
    }

    #[test]
    fn dropping_without_finish_joins_the_readers() {
        within(Duration::from_secs(10), || {
            let pool = ReaderPool::spawn(4, 4, 1_000, |i| i);
            let _ = pool.recv_timeout(Duration::from_secs(5)).unwrap();
            std::thread::sleep(Duration::from_millis(50));
            drop(pool);
        });
    }

    #[test]
    fn an_empty_list_returns_at_once() {
        let queued = within(Duration::from_secs(10), || {
            ReaderPool::spawn(4, 8, 0, |i| i).finish()
        });
        assert_eq!(queued, 0);
    }
}
