//! The dashboard's own log file: where it lives and how it is capped.
//! Split out of `runstate.rs` for size.

use super::*;

/// Inner implementation of `dashboard_log_path`, parameterised so it can be
/// tested without touching the process-global env. All callers go through
/// `dashboard_log_path`.
pub(super) fn dashboard_log_path_from(env_override: Option<&str>) -> PathBuf {
    if let Some(path) = env_override {
        return PathBuf::from(path);
    }
    leviath_core::paths::data_dir()
        .unwrap_or_default()
        .join("dashboard.log")
}

/// Path to the persistent dashboard activity log (~/.leviath/dashboard.log).
///
/// Honours the `LEVIATH_DASHBOARD_LOG_PATH` override when set (tests use it via
/// `isolate_runs_dir_for_test`); otherwise resolves the real home-relative
/// path. This function only *computes* a `PathBuf` - it never writes - so both
/// arms are safe to exercise directly in tests. The write side
/// ([`append_dashboard_log`] and `Dashboard::add_log`) is what must stay off
/// the user's real log in tests: `append_dashboard_log`'s own tests set the
/// override, and `Dashboard` carries an injected log path (a temp dir under
/// `make_test_dashboard`) so no dashboard-input test ever appends to the real
/// `~/.leviath/dashboard.log`.
pub fn dashboard_log_path() -> PathBuf {
    match std::env::var("LEVIATH_DASHBOARD_LOG_PATH") {
        Ok(path) => dashboard_log_path_from(Some(&path)),
        Err(_) => dashboard_log_path_from(None),
    }
}

/// Append a timestamped line to the persistent dashboard activity log at the
/// default [`dashboard_log_path`]. Silently ignores I/O errors - best-effort.
pub fn append_dashboard_log(msg: &str) {
    append_dashboard_log_to(&dashboard_log_path(), msg);
}

/// Append a timestamped line to the dashboard activity log at an explicit
/// `path`. Silently ignores I/O errors - the dashboard log is best-effort.
///
/// The path is a parameter so `Dashboard` can inject a test-isolated log
/// location, guaranteeing no dashboard-input test appends to the user's real
/// `~/.leviath/dashboard.log` (see [`dashboard_log_path`]).
pub fn append_dashboard_log_to(path: &Path, msg: &str) {
    append_dashboard_log_capped(path, msg, DASHBOARD_LOG_MAX_BYTES);
}

/// The dashboard log is capped at this size; once the live file reaches it, the
/// file is rolled (see [`roll_log_if_over_cap`]) so it can't grow without bound
/// across a long-lived daemon's lifetime.
const DASHBOARD_LOG_MAX_BYTES: u64 = 5 * 1024 * 1024;

/// Append with an explicit cap (the public entry points use
/// [`DASHBOARD_LOG_MAX_BYTES`]; tests pass a small cap to exercise rolling).
pub(super) fn append_dashboard_log_capped(path: &Path, msg: &str, max_bytes: u64) {
    use std::io::Write;
    // Ensure the parent directory exists (first-run case).
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    roll_log_if_over_cap(path, max_bytes);
    if let Ok(mut file) = leviath_sys::open_private_append(path) {
        let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        let _ = writeln!(file, "{} {}", timestamp, msg);
    }
}

/// The path the rolled (previous-generation) log is moved to: `<name>.1`.
pub(super) fn rolled_log_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".1");
    PathBuf::from(name)
}

/// Roll the live log to `<name>.1` once it reaches `max_bytes`, replacing any
/// existing rolled file, so the live file restarts empty and at most one
/// previous generation is retained (bounded ~2×cap on disk). Best-effort - a
/// failed rename just leaves the log to keep growing rather than erroring.
pub(super) fn roll_log_if_over_cap(path: &Path, max_bytes: u64) {
    let over = std::fs::metadata(path)
        .map(|m| m.len() >= max_bytes)
        .unwrap_or(false);
    if over {
        let _ = std::fs::rename(path, rolled_log_path(path));
    }
}
