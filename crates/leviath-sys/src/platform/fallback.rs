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

pub(crate) fn ensure_private(_path: &Path, _mode: u32) -> io::Result<Option<u32>> {
    Ok(None)
}

pub(crate) fn configure_detached(_cmd: &mut std::process::Command) {}

pub(crate) fn terminate(_pid: u32) -> bool {
    false
}
