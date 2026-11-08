//! One intensive synchronisation per user.
//!
//! Two instances indexing two chains at once compete for the same disk, page
//! cache and CPU and both end up slower. A second instance must **wait** for
//! its turn and start on its own, not fail: it can keep serving its existing
//! index while it waits.
//!
//! The lock is a `flock` on a per-user file. The kernel releases it when the
//! process exits, including a crash, so a dead owner cannot block the queue
//! forever. It coordinates e-lite processes only; inside a container a local
//! lock cannot see other containers (documented in TUNING.md).

use el_model::Chain;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const RETRY: Duration = Duration::from_secs(10);

pub struct IbdLock {
    file: File,
    pub path: PathBuf,
}

fn lock_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("e-lite");
        }
    }
    PathBuf::from(format!("/tmp/e-lite-{}", unsafe { libc::getuid() }))
}

pub fn lock_path() -> PathBuf {
    lock_dir().join("ibd.lock")
}

fn try_lock(file: &File) -> bool {
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

fn holder(file: &mut File) -> Option<String> {
    let mut out = String::new();
    file.seek(SeekFrom::Start(0)).ok()?;
    file.read_to_string(&mut out).ok()?;
    let line = out.lines().next()?.trim().to_string();
    if line.is_empty() {
        None
    } else {
        Some(line)
    }
}

impl IbdLock {
    /// Try once. `None` means another instance holds the slot.
    pub fn try_acquire(chain: Chain) -> Option<Self> {
        Self::try_acquire_at(lock_path(), chain)
    }

    fn try_acquire_at(path: PathBuf, chain: Chain) -> Option<Self> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .ok()?;
        if !try_lock(&file) {
            return None;
        }
        let _ = file.set_len(0);
        let _ = file.seek(SeekFrom::Start(0));
        let _ = writeln!(
            file,
            "pid={} network={} since=unix-{}",
            std::process::id(),
            chain.name(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        );
        let _ = file.flush();
        Some(IbdLock { file, path })
    }

    /// Wait until the slot is free, checking the shutdown flag. Returns `None`
    /// when the process is shutting down.
    pub fn acquire_waiting(flag: &Arc<AtomicBool>, chain: Chain) -> Option<Self> {
        let mut announced = false;
        loop {
            if flag.load(Ordering::SeqCst) {
                return None;
            }
            if let Some(lock) = Self::try_acquire(chain) {
                if announced {
                    eprintln!(
                        "[e-lite] intensive-sync slot acquired ({}); starting",
                        lock.path.display()
                    );
                } else {
                    eprintln!("[e-lite] intensive-sync slot acquired");
                }
                return Some(lock);
            }
            if !announced {
                let mut who = "another e-lite instance".to_string();
                if let Ok(mut f) = OpenOptions::new().read(true).open(lock_path()) {
                    if let Some(line) = holder(&mut f) {
                        who = line;
                    }
                }
                eprintln!(
                    "[e-lite] another instance is already synchronising ({who}); waiting up to \
                     a few minutes (serving the current index meanwhile). Set \
                     E_LITE_NO_SYNC_LOCK=1 only if you really want two intensive syncs at once."
                );
                announced = true;
            }
            // Sleep in one-second slices so Ctrl-C is responsive.
            for _ in 0..RETRY.as_secs() {
                if flag.load(Ordering::SeqCst) {
                    return None;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }

    pub fn enabled() -> bool {
        !std::env::var("E_LITE_NO_SYNC_LOCK")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    }
}

impl Drop for IbdLock {
    fn drop(&mut self) {
        // Closing the file releases the flock; be explicit about intent.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lock_is_exclusive_and_released_on_drop() {
        let dir = std::env::temp_dir().join(format!("e-lite-lock-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ibd.lock");
        let first = IbdLock::try_acquire_at(path.clone(), Chain::Regtest);
        assert!(first.is_some(), "first instance must get the slot");
        let holder = IbdLock::try_acquire_at(path.clone(), Chain::Regtest);
        assert!(holder.is_none(), "the second instance must wait");
        drop(first);
        let third = IbdLock::try_acquire_at(path.clone(), Chain::Regtest);
        assert!(third.is_some(), "dropping the lock must release the slot");
        drop(third);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
