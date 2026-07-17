//! Unix implementations of the platform primitives.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Set the exact permission bits on `path`.
pub(crate) fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// If `path` exists and is accessible to group or others (any of `0o077`
/// set), tighten it to `mode` and return `Ok(Some(previous_mode))`. If it is
/// already private, or does not exist, return `Ok(None)`.
///
/// A single `metadata()` call serves as both the existence check and the
/// source of the mode bits — calling it once (instead of `exists()` then a
/// second `metadata()`) removes the TOCTOU window that would otherwise leave a
/// permanently-unreachable error branch.
pub(crate) fn ensure_private(path: &Path, mode: u32) -> io::Result<Option<u32>> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(_) => return Ok(None),
    };
    let old = metadata.permissions().mode();
    if old & 0o077 != 0 {
        set_mode(path, mode)?;
        Ok(Some(old))
    } else {
        Ok(None)
    }
}
