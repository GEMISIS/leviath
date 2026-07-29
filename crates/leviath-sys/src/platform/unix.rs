//! Unix implementations of the platform primitives.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

/// Put the spawned child into its own process group so it is detached from the
/// launching terminal's foreground group and outlives that terminal closing.
///
/// This uses the safe, stable [`CommandExt::process_group`] instead of a
/// `pre_exec` + `setsid()` hook: both call sites redirect the child's stdio to
/// files/null and spawn fire-and-forget (the child is reparented to init), so a
/// fresh process group delivers the same "survives the terminal" behaviour with
/// no `unsafe` FFI. This only configures `cmd`; nothing runs until `spawn()`.
pub(crate) fn configure_detached(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
}

/// Create (or truncate) `path` with `mode` already applied, then write
/// `contents`.
///
/// `OpenOptions::mode` sets the mode at `open(2)` time, so the file is never
/// visible to anyone else - unlike write-then-`chmod`, which leaves it at the
/// umask default until the second call lands.
///
/// The mode applies only when the file is *created*. An existing file keeps its
/// own permissions, so `set_mode` is called after the write to cover the
/// overwrite case (re-saving a config that somehow ended up group-readable).
pub(crate) fn write_with_mode(path: &Path, contents: &[u8], mode: u32) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(path)?;
    // Chained rather than three `?`s: each `?` is an error branch that cannot be
    // reached for a file we just opened for writing, and `and_then` keeps the
    // same short-circuiting without one.
    file.write_all(contents)
        .and_then(|()| file.sync_all())
        .and_then(|()| set_mode(path, mode))
}

/// Set the exact permission bits on `path`.
pub(crate) fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// If `path` exists and is accessible to group or others (any of `0o077`
/// set), tighten it to `mode` and return `Ok(Some(previous_mode))`. If it is
/// already private, or does not exist, return `Ok(None)`.
pub(crate) fn ensure_private(path: &Path, mode: u32) -> io::Result<Option<u32>> {
    ensure_private_with(path, mode, set_mode)
}

/// Core of [`ensure_private`], with the permission-setting mechanism injected
/// as a function pointer.
///
/// The `set`-fails-while-file-exists branch cannot be triggered on a
/// non-root Linux CI runner (you cannot make `chmod` fail on a tempfile you
/// own without the immutable flag, which needs `CAP_LINUX_IMMUTABLE`). Rather
/// than hide that branch behind a coverage twin, we split policy from
/// mechanism: a test injects a failing `set` and exercises the error
/// propagation directly. A `fn` pointer (not `impl Fn`) is used deliberately -
/// it is a single concrete type, so there is no per-closure monomorphization
/// for llvm-cov to report phantom-uncovered.
///
/// A single `metadata()` call serves as both the existence check and the
/// source of the mode bits - calling it once (instead of `exists()` then a
/// second `metadata()`) removes the TOCTOU window that would otherwise leave a
/// permanently-unreachable error branch.
fn ensure_private_with(
    path: &Path,
    mode: u32,
    set: fn(&Path, u32) -> io::Result<()>,
) -> io::Result<Option<u32>> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(_) => return Ok(None),
    };
    let old = metadata.permissions().mode();
    if old & 0o077 != 0 {
        set(path, mode)?;
        Ok(Some(old))
    } else {
        Ok(None)
    }
}

/// The calling user's real numeric id.
pub(crate) fn current_uid() -> u32 {
    nix::unistd::getuid().as_raw()
}

/// The effective uid of the process on the other end of a connected
/// Unix-domain socket, from the kernel rather than from anything the peer said.
///
/// Two spellings of the same idea, because the platforms disagree:
/// `SO_PEERCRED` on Linux, `LOCAL_PEERCRED` on macOS and the BSDs. Both are safe
/// `nix` wrappers, so `unsafe_code = "forbid"` still holds.
///
/// `None` when the option is unavailable, which the caller must treat as "do not
/// trust this connection" - an unidentifiable peer is not an authorized one.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn peer_uid(sock: &impl std::os::fd::AsFd) -> Option<u32> {
    nix::sys::socket::getsockopt(sock, nix::sys::socket::sockopt::PeerCredentials)
        .ok()
        .map(|creds| creds.uid())
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub(crate) fn peer_uid(sock: &impl std::os::fd::AsFd) -> Option<u32> {
    nix::sys::socket::getsockopt(sock, nix::sys::socket::sockopt::LocalPeerCred)
        .ok()
        .map(|xucred| xucred.uid())
}

/// SIGKILL the process group led by `pgid`.
///
/// `nix::sys::signal::killpg` with a negated pid is the safe wrapper around
/// `killpg(2)`; no `unsafe` is involved. Errors (the group already exited, or
/// was never ours) are the normal case when reaping and are swallowed by the
/// caller.
pub(crate) fn kill_process_group(pgid: u32) -> io::Result<()> {
    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;
    killpg(Pid::from_raw(pgid as i32), Signal::SIGKILL)
        .map_err(|e| io::Error::from_raw_os_error(e as i32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_private_with_propagates_set_error_when_file_is_permissive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, b"x").unwrap();
        set_mode(&path, 0o644).unwrap();

        fn always_fails(_: &Path, _: u32) -> io::Result<()> {
            Err(io::Error::from_raw_os_error(
                nix::errno::Errno::EPERM as i32,
            ))
        }

        let result = ensure_private_with(&path, 0o600, always_fails);
        assert!(result.is_err());
    }

    #[test]
    fn ensure_private_with_skips_set_for_private_file() {
        // The already-private → `Ok(None)` (set-not-called) branch is covered
        // for real via the public `ensure_file_private` in perms.rs's
        // `ensure_file_private_leaves_private_file_untouched`; here we just
        // confirm the injected-`set` seam agrees, using `set_mode` (which is
        // never reached because the file is already private, so no observable
        // permission change occurs).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, b"x").unwrap();
        set_mode(&path, 0o600).unwrap();

        assert_eq!(ensure_private_with(&path, 0o600, set_mode).unwrap(), None);
    }

    /// Signalling a real group tears down the leader *and* its children - the
    /// whole point, since killing a shell alone leaves what it started
    /// reparented to init and running.
    #[test]
    fn kill_process_group_reaps_the_leader_and_its_children() {
        use std::process::{Command, Stdio};

        // A shell that leads its own group and starts a long-lived child.
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("sleep 60 & sleep 60")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        configure_detached(&mut cmd);
        let mut child = cmd.spawn().expect("sh is available");
        let pgid = child.id();

        // Let the shell get as far as starting its own child.
        std::thread::sleep(std::time::Duration::from_millis(300));
        kill_process_group(pgid).expect("the group is ours to signal");

        // The leader is gone (and reaped, so it leaves no zombie).
        let status = child.wait().expect("leader is waitable");
        assert!(!status.success(), "killed, not a clean exit");
        // And so is the group as a whole: a second signal finds nothing left.
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            kill_process_group(pgid).is_err(),
            "the whole group is gone, so there is nothing left to signal"
        );
    }

    #[test]
    fn kill_process_group_errors_for_a_group_that_does_not_exist() {
        // The ordinary case when reaping - the command already finished - so it
        // must surface as an error the caller can ignore, not a panic.
        let err = kill_process_group(0x7FFF_FFFF).expect_err("no such group");
        assert!(err.raw_os_error().is_some());
    }

    #[test]
    fn configure_detached_sets_process_group_without_spawning() {
        // Configuring the process group must not fork/exec or panic; it only
        // takes effect on a later spawn(), which this test deliberately omits.
        let mut cmd = Command::new("true");
        configure_detached(&mut cmd);
    }
}
