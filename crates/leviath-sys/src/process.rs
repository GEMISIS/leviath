//! Process control: detached spawning.

use std::process::Command;

/// Configure `cmd` so the spawned child detaches into its own process group,
/// surviving the terminal that launched it.
///
/// On Unix this uses the safe, stable `Command::process_group(0)`; on non-Unix
/// platforms it is a no-op. Call this before `cmd.spawn()`.
pub fn configure_detached(cmd: &mut Command) {
    crate::platform::configure_detached(cmd);
}

/// SIGKILL every process in the group led by `pgid` (a no-op on platforms
/// without process groups).
///
/// Killing a child shell is not enough to stop what it started: the shell's own
/// children are reparented to init and keep running. A cancelled agent's
/// `sleep 400` outliving the run that started it is exactly that. Spawning the
/// shell into its own group (via [`configure_detached`]) and signalling the
/// group tears down the whole tree.
///
/// Errors are the ordinary case (the group already exited) and are the caller's
/// to ignore.
pub fn kill_process_group(pgid: u32) -> std::io::Result<()> {
    crate::platform::kill_process_group(pgid)
}

/// The calling user's numeric id.
///
/// Used to address a per-user service domain (`launchctl bootstrap gui/<uid>`).
/// Returns `0` on platforms with no POSIX uid, where no such domain exists.
pub fn current_uid() -> u32 {
    crate::platform::current_uid()
}

/// The effective uid of the process on the other end of a connected
/// Unix-domain socket.
///
/// The kernel's answer, not the peer's claim, so it cannot be spoofed by
/// anything the peer sends. This is what makes the daemon's control socket
/// safe: file permissions on a Unix socket are advisory on macOS and the BSDs
/// (the mode is not consulted on `connect`), so the mode alone was never the
/// guarantee it was documented to be.
///
/// `None` when the platform cannot report it - which the caller must treat as
/// "refuse", since an unidentifiable peer is not an authorized one.
///
/// Unix-only: on Windows the control channel is not a Unix socket, so there is
/// no fd to interrogate and no caller for this.
#[cfg(unix)]
pub fn peer_uid(sock: &impl std::os::fd::AsFd) -> Option<u32> {
    crate::platform::peer_uid(sock)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The check that makes the daemon's control socket safe: the peer's uid
    /// comes from the kernel, not from anything the peer claims. A socket we
    /// connected to ourselves must report our own uid.
    #[cfg(unix)]
    #[test]
    fn peer_uid_reports_our_own_uid_for_a_self_connection() {
        use std::os::unix::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let listener = UnixListener::bind(&path).expect("bind");
        let client = std::os::unix::net::UnixStream::connect(&path).expect("connect");
        let (server, _) = listener.accept().expect("accept");

        // Both ends agree, and both agree with the process's own uid - a peer
        // check that returned `None` here would refuse every legitimate
        // connection, which is the failure mode worth catching.
        assert_eq!(peer_uid(&server), Some(current_uid()));
        assert_eq!(peer_uid(&client), Some(current_uid()));
    }

    #[test]
    fn current_uid_is_reported() {
        // Just has to answer without panicking; root (0) is a legitimate value
        // on Unix and the only value on non-Unix.
        let _uid: u32 = current_uid();
    }

    #[test]
    fn kill_process_group_public_wrapper_reports_a_missing_group() {
        // Exercises the public shim (distinct from the platform impl its own
        // tests cover). A group that cannot exist errors on Unix and is a no-op
        // where the platform has no process groups - both are outcomes the
        // caller ignores, so either result is acceptable here.
        let _ = kill_process_group(0x7FFF_FFFF);
    }

    #[test]
    fn configure_detached_public_wrapper_registers_without_spawning() {
        // Exercises the public `process::configure_detached` shim (distinct
        // from the platform impl its own tests cover); registering the hook
        // must not fork/exec or panic.
        let mut cmd = Command::new("true");
        configure_detached(&mut cmd);
    }
}
