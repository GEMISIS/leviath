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

/// Write `contents` to `path` such that it is **never** readable by anyone but
/// the owner, not even briefly.
///
/// `fs::write` followed by a `chmod` is the obvious shape and has a window: the
/// file is created at `0o666 & ~umask` — typically `0o644` — and is
/// world-readable until the `chmod` lands. Both files that hold Leviath's
/// secrets were written that way, so `config.toml` (every provider API key) and
/// `mcp-auth.json` (OAuth access and refresh tokens) each had a moment of being
/// readable by any local user, on every save.
///
/// Creating the file with the mode already set closes that. On non-Unix this is
/// a plain write — the mode argument has no meaning there, and Windows ACL
/// handling is a separate piece of work rather than something to fake here.
pub fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    crate::platform::write_with_mode(path, contents, 0o600)
}

// Cross-platform tests: they run on every OS so the public API (and, on
// non-Unix, the `fallback` no-op impls) is covered everywhere. Only the
// Unix-specific *assertions* about concrete mode bits are gated behind
// `#[cfg(unix)]`; on non-Unix the same public calls exercise the no-op
// fallback, which succeeds and leaves permissions untouched.
#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// The point of `write_private` over `fs::write` + `chmod`: there is no
    /// moment where the file exists at the umask default. The absence of that
    /// window cannot be observed after the fact, so what is asserted is the mode
    /// on a freshly created file — which the two-step version also reaches, but
    /// only eventually.
    #[test]
    fn write_private_creates_an_owner_only_file_with_the_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");

        write_private(&path, b"the key").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"the key");
        #[cfg(unix)]
        assert_eq!(mode_of(&path), 0o600);
    }

    /// The mode passed to `open(2)` applies only on *creation*, so an existing
    /// file keeps whatever permissions it had. Overwriting one that became
    /// permissive must tighten it again.
    #[test]
    fn write_private_retightens_an_existing_permissive_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        std::fs::write(&path, b"old").unwrap();
        #[cfg(unix)]
        set_mode(&path, 0o644);

        write_private(&path, b"new").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        #[cfg(unix)]
        assert_eq!(mode_of(&path), 0o600);
    }

    /// A failed secret write must never be mistaken for a successful one.
    #[test]
    fn write_private_propagates_an_unwritable_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no-such-dir").join("secret");
        assert!(write_private(&path, b"x").is_err());
    }

    #[test]
    fn secure_file_perms_restricts_on_unix_and_succeeds_everywhere() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        std::fs::write(&path, b"x").unwrap();
        #[cfg(unix)]
        set_mode(&path, 0o644);

        secure_file_perms(&path).unwrap();

        #[cfg(unix)]
        assert_eq!(mode_of(&path), 0o600);
    }

    #[test]
    fn secure_dir_perms_restricts_on_unix_and_succeeds_everywhere() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("d");
        std::fs::create_dir(&sub).unwrap();
        #[cfg(unix)]
        set_mode(&sub, 0o755);

        secure_dir_perms(&sub).unwrap();

        #[cfg(unix)]
        assert_eq!(mode_of(&sub), 0o700);
    }

    #[test]
    fn secure_file_perms_missing_path_behavior() {
        // Unix `chmod` and Windows `icacls` both fail on a path that is not
        // there; only a platform with no permission model at all succeeds,
        // because it genuinely did nothing. Reporting the failure is the point:
        // silently succeeding at protecting a file that does not exist is how a
        // caller ends up believing a secret is restricted when it is not.
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        let result = secure_file_perms(&missing);
        #[cfg(any(unix, windows))]
        assert!(result.is_err());
        #[cfg(not(any(unix, windows)))]
        assert!(result.is_ok());
    }

    #[test]
    fn ensure_file_private_tightens_permissive_file_on_unix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cfg");
        std::fs::write(&path, b"x").unwrap();
        #[cfg(unix)]
        set_mode(&path, 0o644);

        let previous = ensure_file_private(&path).unwrap();

        #[cfg(unix)]
        {
            assert_eq!(previous, Some(0o100644));
            assert_eq!(mode_of(&path), 0o600);
        }
        // Non-Unix always reports "already private / nothing to do".
        #[cfg(not(unix))]
        assert_eq!(previous, None);
    }

    #[test]
    fn ensure_file_private_leaves_private_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cfg");
        std::fs::write(&path, b"x").unwrap();
        #[cfg(unix)]
        set_mode(&path, 0o600);

        assert_eq!(ensure_file_private(&path).unwrap(), None);

        #[cfg(unix)]
        assert_eq!(mode_of(&path), 0o600);
    }

    #[test]
    fn ensure_file_private_is_noop_for_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        assert_eq!(ensure_file_private(&missing).unwrap(), None);
    }
}
