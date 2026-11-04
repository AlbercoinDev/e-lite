//! Tee stdout/stderr to a file so the console and `e-lite.log` show the same
//! lines. Implemented with a pipe + `dup2` so every existing `println!` and
//! `eprintln!` is captured without changing call sites.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;

static HANDLE: OnceLock<Mutex<Option<JoinHandle<()>>>> = OnceLock::new();

/// Start capturing stdout/stderr to `path` in append mode. `max_mib = 0`
/// disables rotation. Returns an error only if the log cannot be created;
/// callers should warn and continue in that case.
pub fn init(path: &Path, max_mib: u64) -> Result<(), String> {
    if HANDLE.get().is_some() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create log directory {}: {e}", parent.display()))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("cannot open log {}: {e}", path.display()))?;
    restrict_permissions(path);

    // Flush anything already buffered before redirecting the descriptors.
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();

    let out_fd = unsafe { libc::dup(1) };
    if out_fd < 0 {
        return Err("cannot duplicate stdout for the log tee".to_string());
    }
    let mut pipe = [0i32; 2];
    if unsafe { libc::pipe(pipe.as_mut_ptr()) } != 0 {
        unsafe { libc::close(out_fd) };
        return Err("cannot create the log pipe".to_string());
    }
    if unsafe { libc::dup2(pipe[1], 1) } < 0 || unsafe { libc::dup2(pipe[1], 2) } < 0 {
        unsafe {
            libc::close(out_fd);
            libc::close(pipe[0]);
            libc::close(pipe[1]);
        }
        return Err("cannot redirect stdout/stderr for the log tee".to_string());
    }
    unsafe { libc::close(pipe[1]) };

    let path = path.to_path_buf();
    let max_bytes = max_mib.saturating_mul(1024 * 1024);
    let handle = std::thread::spawn(move || {
        let mut file = file;
        let mut out = unsafe { File::from_raw_fd(out_fd) };
        let mut reader = unsafe { File::from_raw_fd(pipe[0]) };
        let mut buf = [0u8; 8192];
        let mut pending = Vec::new();
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    pending.extend_from_slice(&buf[..n]);
                    while let Some(pos) = pending.iter().position(|b| *b == b'\n') {
                        let chunk: Vec<u8> = pending.drain(..=pos).collect();
                        write_chunk(&mut out, &mut file, &path, max_bytes, &chunk);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        if !pending.is_empty() {
            write_chunk(&mut out, &mut file, &path, max_bytes, &pending);
        }
    });

    let slot = HANDLE.get_or_init(|| Mutex::new(None));
    if let Ok(mut guard) = slot.lock() {
        *guard = Some(handle);
    }
    Ok(())
}

fn write_chunk(out: &mut File, file: &mut File, path: &Path, max_bytes: u64, chunk: &[u8]) {
    if max_bytes > 0 {
        if let Ok(meta) = file.metadata() {
            if meta.len().saturating_add(chunk.len() as u64) > max_bytes {
                rotate(file, path);
            }
        }
    }
    let _ = out.write_all(chunk);
    let _ = out.flush();
    let _ = file.write_all(chunk);
    let _ = file.flush();
}

fn rotate(file: &mut File, path: &Path) {
    let backup = path.with_file_name(format!(
        "{}.1",
        path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "e-lite.log".to_string())
    ));
    let _ = file.flush();
    if std::fs::rename(path, &backup).is_ok() {
        if let Ok(new) = OpenOptions::new().create(true).append(true).open(path) {
            *file = new;
            restrict_permissions(path);
        }
    }
}

fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o640));
}

/// Drain the tee and restore the process exit path. Safe to call more than once.
pub fn shutdown() {
    let Some(slot) = HANDLE.get() else {
        return;
    };
    let Some(handle) = slot.lock().ok().and_then(|mut g| g.take()) else {
        return;
    };
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    // Closing the pipe write ends lets the tee thread read EOF and flush the
    // tail of the log before the process exits.
    unsafe {
        libc::close(1);
        libc::close(2);
    }
    let _ = handle.join();
}
