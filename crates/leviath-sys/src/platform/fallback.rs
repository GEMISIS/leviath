//! Implementations for targets that are neither Unix nor Windows.
//!
//! There is no permission model to apply here, so the hardening calls are
//! no-ops that succeed. That is a real gap on such a target - secrets are
//! written with whatever protection the platform gives by default - and it is
//! stated rather than papered over. Unix uses POSIX modes and Windows uses
//! ACLs; both are implemented in their own modules.

use std::io;
use std::path::Path;

pub(crate) fn set_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

/// A plain write: this platform has no permission model to apply, and faking
/// one here would claim a protection that is not being applied.
pub(crate) fn write_with_mode(path: &Path, contents: &[u8], _mode: u32) -> io::Result<()> {
    std::fs::write(path, contents)
}

pub(crate) fn ensure_private(_path: &Path, _mode: u32) -> io::Result<Option<u32>> {
    Ok(None)
}

pub(crate) fn configure_detached(_cmd: &mut std::process::Command) {}

/// No POSIX uid here; the value is only used to address a per-user
/// launchd/systemd domain, neither of which exists on such a target.
pub(crate) fn current_uid() -> u32 {
    0
}

/// No process groups to signal on this platform; killing the direct child is
/// all that is available (and is what the caller already does).
pub(crate) fn kill_process_group(_pgid: u32) -> io::Result<()> {
    Ok(())
}
