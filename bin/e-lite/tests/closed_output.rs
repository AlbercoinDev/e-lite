//! AUD-192: a one-shot command whose stdout is already closed ends quietly
//! (killed by SIGPIPE, like any Unix tool), not with a FATAL internal error
//! and an abort.

use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};

/// A pipe whose read end is closed before the child starts, so the child's
/// first write to stdout fails every time (no race with a reader).
fn closed_pipe() -> OwnedFd {
    let mut fds = [0; 2];
    // SAFETY: `pipe` fills two new descriptors that this function owns.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    // SAFETY: both descriptors were just created and are owned here.
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    drop(read);
    write
}

#[test]
fn a_one_shot_command_ends_quietly_when_its_output_is_closed() {
    for args in [&["help"][..], &["version"][..]] {
        let out = Command::new(env!("CARGO_BIN_EXE_e-lite"))
            .args(args)
            .stdout(Stdio::from(closed_pipe()))
            .stderr(Stdio::piped())
            .output()
            .expect("run e-lite");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !stderr.contains("FATAL") && !stderr.contains("internal error"),
            "e-lite {args:?} reported an internal error: {stderr}"
        );
        assert_eq!(
            out.status.signal(),
            Some(libc::SIGPIPE),
            "e-lite {args:?} should end by SIGPIPE, got {:?}",
            out.status
        );
    }
}
