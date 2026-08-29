//! Permission hardening for the config file and its directory.
//!
//! The mechanism (metadata probe, `chmod`) lives in `leviath_sys`; this module
//! owns the policy of what to log for each outcome, with the hardening
//! operation injected as a `fn` pointer so every arm is coverable on every OS.

use super::Config;

/// Create the config directory with restrictive permissions.
pub(super) fn create_config_dir(dir: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)
        .map_err(|e| anyhow::anyhow!("Failed to create config directory: {}", e))?;
    set_dir_permissions(dir);
    Ok(())
}

/// Check permissions on the config file and auto-fix if too permissive.
///
/// A no-op on non-Unix platforms - see [`leviath_sys::ensure_file_private`].
pub(super) fn check_permissions() {
    check_permissions_at(&Config::config_path());
}

/// Core of [`check_permissions`], parameterized by path so it can be exercised
/// in tests against a tempfile instead of the real config path.
///
/// The permission mechanism (metadata probe + `chmod`) lives in `leviath_sys`;
/// this function owns only the policy of what to log for each outcome.
pub(super) fn check_permissions_at(path: &std::path::Path) {
    check_permissions_at_with(path, leviath_sys::ensure_file_private);
}

/// Core of [`check_permissions_at`] with the permission-hardening operation
/// injected, so the "fix failed" arm can be covered deterministically on every
/// OS. On disk that `Err` only occurs when a file exists but `chmod` fails -
/// forcing that without root differs per platform (macOS `chflags uchg`, no
/// portable Linux equivalent), so a `fn` pointer is injected instead of relying
/// on an OS-specific trick. A `fn` pointer (not `impl Fn`) keeps this to a
/// single monomorphization.
pub(super) fn check_permissions_at_with(
    path: &std::path::Path,
    ensure: fn(&std::path::Path) -> std::io::Result<Option<u32>>,
) {
    match ensure(path) {
        Ok(Some(old_mode)) => {
            let masked_mode = old_mode & 0o777;
            tracing::warn!(
                "Config file has overly permissive permissions ({:o}), fixing to 600",
                masked_mode
            );
        }
        Ok(None) => {}
        Err(e) => tracing::warn!("Failed to fix config file permissions: {}", e),
    }
}

/// Set restrictive permissions on the config directory.
pub(super) fn set_dir_permissions(path: &std::path::Path) {
    set_dir_permissions_with(path, leviath_sys::secure_dir_perms);
}

/// Core of [`set_dir_permissions`] with the hardening operation injected; see
/// [`check_permissions_at_with`] for why.
pub(super) fn set_dir_permissions_with(
    path: &std::path::Path,
    secure: fn(&std::path::Path) -> std::io::Result<()>,
) {
    if let Err(e) = secure(path) {
        tracing::warn!("Failed to set config directory permissions: {}", e);
    }
}
