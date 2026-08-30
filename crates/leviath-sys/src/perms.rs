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
/// file is created at `0o666 & ~umask` - typically `0o644` - and is
/// world-readable until the `chmod` lands. That is a moment on every save where
/// `config.toml` (every provider API key) and `mcp-auth.json` (OAuth access and
/// refresh tokens) are readable by any local user.
///
/// Creating the file with the mode already set closes that. On non-Unix this is
/// a plain write - the mode argument has no meaning there, and Windows ACL
/// handling is a separate piece of work rather than something to fake here.
pub fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    write_atomic(path, contents, Some(0o600))
}

/// Replace `path` with `contents` in one step: the bytes are written to a
/// fresh file beside it and renamed over it, so a reader never sees a
/// half-written file and a crash mid-write leaves the old one whole.
///
/// `mode` is the permission the new file is created with (`Some(0o600)` for
/// a secret). `None` keeps what the target had, or the platform default for
/// a new file, so an agent manifest saved by the editor is not quietly made
/// owner-only. A symlink at `path` is followed: the file it points at is what
/// is replaced, not the link.
///
/// Use this rather than `fs::write`, which truncates the target and then
/// writes into it: a process that dies between the two leaves `config.toml`,
/// a blueprint or the dashboard's memory empty. The inode changes on every
/// save, which is what atomic replacement means; anything holding the old
/// file open keeps the old bytes.
pub fn write_atomic(path: &Path, contents: &[u8], mode: Option<u32>) -> io::Result<()> {
    write_atomic_with(path, contents, mode, persist)
}

/// The rename step of [`write_atomic`], as `tempfile` does it.
fn persist(staged: tempfile::TempPath, target: &Path) -> io::Result<()> {
    staged.persist(target).map_err(|e| e.error)
}

/// [`write_atomic`] with the rename injected, so the arm where the old file
/// has to survive a failed replacement is provable without a filesystem
/// that refuses renames.
pub fn write_atomic_with(
    path: &Path,
    contents: &[u8],
    mode: Option<u32>,
    persist: fn(tempfile::TempPath, &Path) -> io::Result<()>,
) -> io::Result<()> {
    let target = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    // A file the owner marked read-only stays that way. Replacing it by
    // rename would succeed where a write into it fails, and "cannot be
    // written" is the answer a read-only secrets file is there to give.
    if let Ok(existing) = std::fs::metadata(&target)
        && existing.permissions().readonly()
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("'{}' is read-only", target.display()),
        ));
    }
    let dir = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| io::Error::other(format!("'{}' has no parent directory", path.display())))?;
    let staged = tempfile::Builder::new()
        .prefix(".lev-write-")
        .tempfile_in(dir)?
        .into_temp_path();
    // Written through the platform's mode-setting write, so a secret is
    // owner-only from the moment it has bytes; a plain write otherwise, and
    // the existing file's permissions are carried over when it has any.
    let written = match mode {
        Some(mode) => crate::platform::write_with_mode(&staged, contents, mode),
        None => std::fs::write(&staged, contents).and_then(|()| match std::fs::metadata(&target) {
            Ok(existing) => std::fs::set_permissions(&staged, existing.permissions()),
            Err(_) => Ok(()),
        }),
    };
    // Chained rather than `?`: a write into a file just created for writing
    // has no reachable failure, and the chain keeps the short-circuit
    // without a branch nothing can exercise.
    written.and_then(|()| persist(staged, &target))
}

/// Open `path` for appending, owner-only (`0o600` on Unix, an owner-only ACL on
/// Windows, plain elsewhere).
///
/// [`write_private`] covers a file written in one shot. A file that is appended
/// to over time cannot use it, and opening one plainly creates it at the umask
/// default, so a run's archive (`run.lvr`) and its stage logs inherit `0o644`
/// rather than anyone choosing it.
///
/// The containing run directory is `0o700`, so those files are not reachable in
/// place. Directory permissions do not survive a copy though, and `tar`, `rsync`
/// or a backup tool preserves the per-file mode while dropping the protection
/// the directory was providing.
///
/// On Windows the restriction is best-effort: a failed ACL call still yields an
/// open file. [`write_private`] guards secrets and fails instead, but these are
/// a run's own files, and refusing to open one because `icacls` did not run
/// would trade "less protected than intended" for "the run cannot record
/// anything".
pub fn open_private_append(path: &Path) -> io::Result<std::fs::File> {
    crate::platform::open_append_with_mode(path, 0o600)
}

/// Create `path` and any missing parents, owner-only (`0o700` on Unix, an
/// owner-only ACL on Windows, plain elsewhere).
///
/// `create_dir_all` makes directories at the umask default, typically `0o755`.
/// [`secure_dir_perms`] fixes that afterwards, leaving a window; this closes it
/// and is the right call for a directory created on a hot path, where the
/// after-the-fact `chmod` is easy to forget.
/// Best-effort on the restriction on Windows, for the reason
/// [`open_private_append`] gives.
pub fn create_private_dir_all(path: &Path) -> io::Result<()> {
    crate::platform::create_dir_all_with_mode(path, 0o700)
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

    /// On Windows these calls shell out to `icacls`, resolved from `SystemRoot`
    /// with a grant for `USERNAME` - and the platform tests mutate both
    /// process-wide. Every test here that spawns it takes the same lock they
    /// do, or a mutator's mid-flight environment turns a passing test into a
    /// spawn failure.
    #[cfg(windows)]
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::platform::ENV_LOCK.lock().expect("env lock")
    }

    /// The point of `write_private` over `fs::write` + `chmod`: there is no
    /// moment where the file exists at the umask default. The absence of that
    /// window cannot be observed after the fact, so what is asserted is the mode
    /// on a freshly created file - which the two-step version also reaches, but
    /// only eventually.
    #[test]
    fn write_private_creates_an_owner_only_file_with_the_content() {
        #[cfg(windows)]
        let _env = env_lock();
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
        #[cfg(windows)]
        let _env = env_lock();
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

    /// A file opened plainly for append is created at the umask default,
    /// typically `0o644`; `run.lvr` and the stage logs must not be.
    #[test]
    fn open_private_append_creates_an_owner_only_file() {
        #[cfg(windows)]
        let _env = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.lvr");

        {
            use std::io::Write;
            let mut f = open_private_append(&path).unwrap();
            f.write_all(b"first").unwrap();
        }

        assert_eq!(std::fs::read(&path).unwrap(), b"first");
        #[cfg(unix)]
        assert_eq!(mode_of(&path), 0o600);
    }

    /// Appending is the common case, and it must add to the file rather than
    /// truncate it: the archive is built up record by record over a whole run.
    #[test]
    fn open_private_append_adds_to_an_existing_file() {
        #[cfg(windows)]
        let _env = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");

        for chunk in [b"one", b"two"] {
            use std::io::Write;
            let mut f = open_private_append(&path).unwrap();
            f.write_all(chunk).unwrap();
        }

        assert_eq!(std::fs::read(&path).unwrap(), b"onetwo");
    }

    /// The mode passed to `open(2)` applies only on creation, so a file that
    /// already exists at looser permissions has to be tightened.
    #[test]
    fn open_private_append_retightens_an_existing_permissive_file() {
        #[cfg(windows)]
        let _env = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.lvr");
        std::fs::write(&path, b"old").unwrap();
        #[cfg(unix)]
        set_mode(&path, 0o644);

        drop(open_private_append(&path).unwrap());

        #[cfg(unix)]
        assert_eq!(mode_of(&path), 0o600);
    }

    #[test]
    fn open_private_append_propagates_an_unwritable_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no-such-dir").join("log");
        assert!(open_private_append(&path).is_err());
    }

    #[test]
    fn write_atomic_replaces_the_file_and_keeps_a_secret_private() {
        #[cfg(windows)]
        let _env = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, b"old").unwrap();
        write_atomic(&path, b"new", Some(0o600)).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        #[cfg(unix)]
        assert_eq!(mode_of(&path), 0o600);
        // Nothing staged is left behind.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        // A fresh file, no mode asked for: created and readable.
        let plain = dir.path().join("agent.leviath");
        write_atomic(&plain, b"[agent]", None).unwrap();
        assert_eq!(std::fs::read(&plain).unwrap(), b"[agent]");
    }

    /// `None` keeps the permissions the target already had, so replacing a
    /// file never quietly changes who can read it.
    #[cfg(unix)]
    #[test]
    fn write_atomic_without_a_mode_keeps_the_existing_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        std::fs::write(&path, b"old").unwrap();
        set_mode(&path, 0o640);
        write_atomic(&path, b"new", None).unwrap();
        assert_eq!(mode_of(&path), 0o640);
    }

    /// A symlink is followed: the file it points at is replaced and the link
    /// still points there.
    #[cfg(unix)]
    #[test]
    fn write_atomic_follows_a_symlink_to_the_real_file() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.toml");
        std::fs::write(&real, b"old").unwrap();
        let link = dir.path().join("link.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        write_atomic(&link, b"new", None).unwrap();
        assert_eq!(std::fs::read(&real).unwrap(), b"new");
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    /// The failures are reported, and a failed replacement leaves the old
    /// file exactly as it was with nothing staged beside it.
    #[test]
    fn write_atomic_reports_a_bad_target_and_leaves_the_old_file_on_failure() {
        #[cfg(windows)]
        let _env = env_lock();
        // No parent directory to stage in.
        assert!(write_atomic(Path::new(""), b"x", None).is_err());
        // A parent that does not exist.
        let dir = tempfile::tempdir().unwrap();
        assert!(write_atomic(&dir.path().join("missing").join("f"), b"x", None).is_err());
        // A read-only target is refused, not replaced around.
        let locked = dir.path().join("locked.toml");
        std::fs::write(&locked, b"keep").unwrap();
        let original = std::fs::metadata(&locked).unwrap().permissions();
        let mut perms = original.clone();
        perms.set_readonly(true);
        std::fs::set_permissions(&locked, perms).unwrap();
        let err = write_atomic(&locked, b"new", Some(0o600)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(std::fs::read(&locked).unwrap(), b"keep");
        std::fs::set_permissions(&locked, original).unwrap();
        // A rename the disk refuses: the old bytes survive, the staging
        // file is gone.
        fn refuse(_staged: tempfile::TempPath, _target: &Path) -> io::Result<()> {
            Err(io::Error::other("rename refused"))
        }
        let alone = tempfile::tempdir().unwrap();
        let path = alone.path().join("kept.toml");
        std::fs::write(&path, b"old").unwrap();
        let err = write_atomic_with(&path, b"new", None, refuse).unwrap_err();
        assert!(err.to_string().contains("rename refused"));
        assert_eq!(std::fs::read(&path).unwrap(), b"old");
        assert_eq!(std::fs::read_dir(alone.path()).unwrap().count(), 1);
    }

    #[test]
    fn create_private_dir_all_makes_owner_only_directories() {
        #[cfg(windows)]
        let _env = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("stages").join("0");

        create_private_dir_all(&nested).unwrap();

        assert!(nested.is_dir());
        #[cfg(unix)]
        assert_eq!(mode_of(&nested), 0o700);
    }

    /// A failed create has to be reported, not swallowed. The caller decides
    /// whether it can carry on without the directory; it cannot decide that if
    /// it was told the directory is there.
    #[test]
    fn create_private_dir_all_propagates_a_path_it_cannot_create() {
        #[cfg(windows)]
        let _env = env_lock();
        let dir = tempfile::tempdir().unwrap();
        // A file where a parent directory would have to be.
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, b"x").unwrap();

        assert!(create_private_dir_all(&blocker.join("child")).is_err());
        // And a file sitting exactly where the directory should be. Reported
        // rather than mistaken for "it is already there".
        assert!(create_private_dir_all(&blocker).is_err());
    }

    /// Called on every stage line, so it has to be idempotent rather than
    /// failing once the directory is there.
    #[test]
    fn create_private_dir_all_is_idempotent() {
        #[cfg(windows)]
        let _env = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("stages").join("0");

        create_private_dir_all(&nested).unwrap();
        create_private_dir_all(&nested).unwrap();

        assert!(nested.is_dir());
    }

    #[test]
    fn secure_file_perms_restricts_on_unix_and_succeeds_everywhere() {
        #[cfg(windows)]
        let _env = env_lock();
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
        #[cfg(windows)]
        let _env = env_lock();
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
        #[cfg(windows)]
        let _env = env_lock();
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
        #[cfg(windows)]
        let _env = env_lock();
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
        #[cfg(windows)]
        let _env = env_lock();
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
