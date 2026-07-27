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
/// propagation directly. A `fn` pointer (not `impl Fn`) is used deliberately —
/// it is a single concrete type, so there is no per-closure monomorphization
/// for llvm-cov to report phantom-uncovered.
///
/// A single `metadata()` call serves as both the existence check and the
/// source of the mode bits — calling it once (instead of `exists()` then a
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

    #[test]
    fn configure_detached_sets_process_group_without_spawning() {
        // Configuring the process group must not fork/exec or panic; it only
        // takes effect on a later spawn(), which this test deliberately omits.
        let mut cmd = Command::new("true");
        configure_detached(&mut cmd);
    }
}
