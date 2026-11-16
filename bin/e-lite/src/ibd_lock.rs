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
    /// `None` when no lock file could be created: the sync runs unlocked.
    file: Option<File>,
    pub path: PathBuf,
}

/// One try at one lock file.
enum Attempt {
    Got(IbdLock),
    /// Another process holds it.
    Held,
    /// The file cannot be created or opened (read-only or foreign runtime
    /// directory). Not the same as held: nobody will ever release it.
    Unusable(std::io::Error),
}

/// Where the lock may live, in order: the user's runtime directory, then a
/// per-user directory under `/tmp`.
fn lock_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        if !dir.is_empty() {
            out.push(PathBuf::from(dir).join("e-lite").join("ibd.lock"));
        }
    }
    out.push(PathBuf::from(format!("/tmp/e-lite-{}", unsafe { libc::getuid() })).join("ibd.lock"));
    out
}

pub fn lock_path() -> PathBuf {
    lock_paths().remove(0)
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
        Self::try_acquire_in(&lock_paths(), chain)
    }

    /// The first usable lock file decides. When none can be created the sync
    /// goes on without a lock, with one warning: waiting would never end,
    /// because nobody holds what cannot be opened (AUD-188).
    fn try_acquire_in(paths: &[PathBuf], chain: Chain) -> Option<Self> {
        let mut last_error = None;
        for path in paths {
            match Self::try_acquire_at(path.clone(), chain) {
                Attempt::Got(lock) => return Some(lock),
                Attempt::Held => return None,
                Attempt::Unusable(e) => last_error = Some((path.clone(), e)),
            }
        }
        let (path, e) = last_error?;
        eprintln!(
            "[e-lite] WARNING: the intensive-sync lock {} cannot be used ({e}); continuing \
             without it. Two instances synchronising at once will be slower.",
            path.display()
        );
        Some(IbdLock { file: None, path })
    }

    fn try_acquire_at(path: PathBuf, chain: Chain) -> Attempt {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
        {
            Ok(file) => file,
            Err(e) => return Attempt::Unusable(e),
        };
        if !try_lock(&file) {
            return Attempt::Held;
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
        Attempt::Got(IbdLock {
            file: Some(file),
            path,
        })
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
        if let Some(file) = &self.file {
            unsafe {
                libc::flock(file.as_raw_fd(), libc::LOCK_UN);
            }
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
        let paths = [path];
        let first = IbdLock::try_acquire_in(&paths, Chain::Regtest);
        assert!(first.is_some(), "first instance must get the slot");
        let holder = IbdLock::try_acquire_in(&paths, Chain::Regtest);
        assert!(holder.is_none(), "the second instance must wait");
        drop(first);
        let third = IbdLock::try_acquire_in(&paths, Chain::Regtest);
        assert!(third.is_some(), "dropping the lock must release the slot");
        drop(third);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// AUD-188: a lock file that cannot be created is not "held by another
    /// instance". The next location is used, and with none the sync runs
    /// unlocked instead of waiting for ever.
    #[test]
    fn an_unusable_lock_location_is_not_a_held_lock() {
        let dir = std::env::temp_dir().join(format!("e-lite-lock-unusable-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A regular file where the lock directory should be.
        let blocker = dir.join("not-a-directory");
        std::fs::write(&blocker, b"x").unwrap();
        let unusable = blocker.join("e-lite").join("ibd.lock");
        let usable = dir.join("fallback").join("ibd.lock");

        let lock = IbdLock::try_acquire_in(&[unusable.clone(), usable.clone()], Chain::Regtest)
            .expect("the fallback location is used");
        assert_eq!(lock.path, usable);
        assert!(lock.file.is_some());
        // The fallback is a real lock: a second instance waits.
        assert!(IbdLock::try_acquire_in(&[unusable.clone(), usable], Chain::Regtest).is_none());
        drop(lock);

        // Nowhere to put it: run without a lock.
        let unlocked = IbdLock::try_acquire_in(&[unusable], Chain::Regtest)
            .expect("an unusable lock must not block the sync");
        assert!(unlocked.file.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
