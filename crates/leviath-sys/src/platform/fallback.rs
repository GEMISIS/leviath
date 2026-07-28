//! Non-Unix fallback implementations.
//!
//! Windows does not use POSIX permission bits, so file-permission hardening is
//! a no-op here (access control is left to the platform's own ACLs). This
//! module is `#[cfg(not(unix))]`, so it is never compiled on the Linux
//! coverage run and never counts toward coverage.

use std::io;
use std::path::Path;

pub(crate) fn set_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

/// A plain write: this platform has no POSIX mode bits, and faking one here
/// would claim a protection that is not being applied. Windows ACL handling is
/// its own piece of work.
pub(crate) fn write_with_mode(path: &Path, contents: &[u8], _mode: u32) -> io::Result<()> {
    std::fs::write(path, contents)
}

pub(crate) fn ensure_private(_path: &Path, _mode: u32) -> io::Result<Option<u32>> {
    Ok(None)
}

pub(crate) fn configure_detached(_cmd: &mut std::process::Command) {}

/// Windows has no POSIX uid; the value is only used to address a per-user
/// launchd/systemd domain, neither of which exists here.
pub(crate) fn current_uid() -> u32 {
    0
}

/// No process groups to signal on this platform; killing the direct child is
/// all that is available (and is what the caller already does).
pub(crate) fn kill_process_group(_pgid: u32) -> io::Result<()> {
    Ok(())
}
