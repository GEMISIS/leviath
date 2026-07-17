//! File and directory permission hardening.
//!
//! On Unix these set POSIX mode bits; on other platforms they are no-ops that
//! succeed (Windows ACLs are not modeled here).

use std::io;
use std::path::Path;

/// Restrict a file to owner-only read/write (`0o600` on Unix; no-op elsewhere).
pub fn secure_file_perms(path: &Path) -> io::Result<()> {
    crate::platform::set_mode(path, 0o600)
}

/// Restrict a directory to owner-only read/write/execute (`0o700` on Unix; no-op elsewhere).
pub fn secure_dir_perms(path: &Path) -> io::Result<()> {
    crate::platform::set_mode(path, 0o700)
}

/// If `path` exists and is accessible to group or others, tighten it to
/// owner-only (`0o600`) and return `Ok(Some(previous_mode))`. If it is already
/// private, or does not exist, return `Ok(None)`. On non-Unix platforms this
/// always returns `Ok(None)`.
pub fn ensure_file_private(path: &Path) -> io::Result<Option<u32>> {
    crate::platform::ensure_private(path, 0o600)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn secure_file_perms_sets_600() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        std::fs::write(&path, b"x").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        secure_file_perms(&path).unwrap();

        assert_eq!(mode_of(&path), 0o600);
    }

    #[test]
    fn secure_dir_perms_sets_700() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("d");
        std::fs::create_dir(&sub).unwrap();
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();

        secure_dir_perms(&sub).unwrap();

        assert_eq!(mode_of(&sub), 0o700);
    }

    #[test]
    fn secure_file_perms_errors_on_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        assert!(secure_file_perms(&missing).is_err());
    }

    #[test]
    fn ensure_file_private_tightens_permissive_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cfg");
        std::fs::write(&path, b"x").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let previous = ensure_file_private(&path).unwrap();

        assert_eq!(previous, Some(0o100644));
        assert_eq!(mode_of(&path), 0o600);
    }

    #[test]
    fn ensure_file_private_leaves_private_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cfg");
        std::fs::write(&path, b"x").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        assert_eq!(ensure_file_private(&path).unwrap(), None);
        assert_eq!(mode_of(&path), 0o600);
    }

    #[test]
    fn ensure_file_private_is_noop_for_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        assert_eq!(ensure_file_private(&missing).unwrap(), None);
    }
}
