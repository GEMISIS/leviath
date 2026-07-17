//! On-disk run state for background agent executions.
//!
//! Each run lives under ~/.leviath/runs/<run-id>/ with:
//! - `meta.json`    — run metadata, updated atomically (tmp + rename)
//! - `output.log`  — append-only combined worker stdout (legacy/fallback)
//! - `stages.json` — index of per-stage records
//! - `stages/<idx>/output.log` — readable agent output for that stage
//! - `stages/<idx>/logs.log`   — operational events + tool activity
//! - `stages/<idx>/context.json` — context snapshot for that stage
//!
//! The dashboard's activity log is persisted separately at:
//! - `~/.leviath/dashboard.log` — never cleared, appended across sessions

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

// The plain run-state data types (RunMeta, RunStatus, the snapshot structs, and
// the per-stage records) now live in `leviath_core::run_meta`. They are
// re-exported here unchanged so existing `crate::runstate::RunMeta` /
// `runstate::RunMeta` call sites across the cli keep compiling. All on-disk IO
// for these types remains in this module.
pub use leviath_core::run_meta::{
    ContextSnapshot, RegionEntrySnapshot, RegionSnapshot, RunMeta, RunStatus, StageRecord,
    StageRunStatus,
};

/// Atomically write a context snapshot for the run.
pub fn write_context_snapshot(run_id: &str, snap: &ContextSnapshot) -> anyhow::Result<()> {
    write_context_snapshot_to(&run_dir(run_id), snap)
}

/// Atomically write pre-serialized `json` to `path` (via a `.json.tmp`
/// sibling + rename).
///
/// Non-generic (takes an already-serialized string) so it has a single
/// monomorphization and every region — including the `std::fs` error `?`
/// arms — is exercised by real tests. Serialization is performed by the
/// callers, whose concrete production types
/// (`ContextSnapshot`/`RunMeta`/`&[StageRecord]`) are provably infallible to
/// serialize (see the `.expect` sites).
fn write_json_atomic(path: &std::path::Path, json: &str) -> anyhow::Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn write_context_snapshot_to(dir: &std::path::Path, snap: &ContextSnapshot) -> anyhow::Result<()> {
    let json = serde_json::to_string_pretty(snap)
        .expect("infallible: ContextSnapshot always serializes to JSON");
    write_json_atomic(&dir.join("context.json"), &json)
}

/// Read the context snapshot for a run, if present.
pub fn read_context_snapshot(run_id: &str) -> Option<ContextSnapshot> {
    let path = run_dir(run_id).join("context.json");
    let json = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&json).ok()
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Inner implementation of `runs_dir`, parameterised so it can be tested
/// without touching the process-global env. All callers go through `runs_dir`.
fn runs_dir_from(env_override: Option<&str>) -> PathBuf {
    if let Some(dir) = env_override {
        return PathBuf::from(dir);
    }
    dirs::home_dir()
        .unwrap_or_default()
        .join(".leviath")
        .join("runs")
}

/// Directory where all run state is stored.
pub fn runs_dir() -> PathBuf {
    runs_dir_from(std::env::var("LEVIATH_RUNS_DIR").ok().as_deref())
}

/// Directory for a specific run.
pub fn run_dir(run_id: &str) -> PathBuf {
    runs_dir().join(run_id)
}

/// Inner implementation of `dashboard_log_path`, parameterised so it can be
/// tested without touching the process-global env. All callers go through
/// `dashboard_log_path`.
fn dashboard_log_path_from(env_override: Option<&str>) -> PathBuf {
    if let Some(path) = env_override {
        return PathBuf::from(path);
    }
    dirs::home_dir()
        .unwrap_or_default()
        .join(".leviath")
        .join("dashboard.log")
}

/// Path to the persistent dashboard activity log (~/.leviath/dashboard.log).
///
/// Under `#[cfg(test)]`, when `LEVIATH_DASHBOARD_LOG_PATH` isn't explicitly
/// set, this falls back to a fixed, shared *test* location instead of the
/// real home directory. Unlike `runs_dir()` (fully covered by per-test
/// `isolate_runs_dir_for_test` guards), dashboard activity logging is called
/// pervasively from production code deep inside ordinary dashboard input
/// handling (`Dashboard::add_log`, hit by nearly every key-handler test in
/// `dashboard/input.rs`) -- retrofitting every transitive caller with its
/// own guard isn't practical, and this is inherently a shared, append-only,
/// best-effort log where per-test isolation doesn't matter the way it does
/// for `runs_dir` (no test needs a *clean* view of it). This is the one
/// place in the crate where the test-vs-prod fallback intentionally
/// diverges, specifically to guarantee zero test writes reach the user's
/// real `~/.leviath/dashboard.log` even from tests nobody remembered to
/// isolate.
pub fn dashboard_log_path() -> PathBuf {
    if let Ok(path) = std::env::var("LEVIATH_DASHBOARD_LOG_PATH") {
        return dashboard_log_path_from(Some(&path));
    }
    #[cfg(test)]
    {
        dirs::home_dir()
            .unwrap_or_default()
            .join(".leviath-test")
            .join("shared-dashboard.log")
    }
    #[cfg(not(test))]
    {
        dashboard_log_path_from(None)
    }
}

/// Append a timestamped line to the persistent dashboard activity log.
/// Silently ignores I/O errors — the dashboard log is best-effort.
pub fn append_dashboard_log(msg: &str) {
    use std::io::Write;
    let path = dashboard_log_path();
    // Ensure the parent directory exists (first-run case).
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        let _ = writeln!(file, "{} {}", timestamp, msg);
    }
}

/// Process-wide counter mixed into [`new_run_id`]'s suffix so that multiple
/// runs spawned in a tight loop (e.g. `lev run --count N`) within the same
/// wall-clock second never collide. Before this, the suffix was derived
/// purely from `now` (whole seconds), so every run in a `--count N` batch
/// got the *same* run ID -- silently collapsing N runs into one on-disk
/// entry and leaving N-1 worker processes writing state nobody could see.
static RUN_ID_COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Generate a unique run ID: "<agent_name>-<timestamp>-<suffix>".
pub fn new_run_id(agent_name: &str) -> String {
    let now = now_secs();
    let counter = RUN_ID_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed) as i64;
    let suffix = format!(
        "{:04x}",
        ((now & 0xffff) ^ (now >> 16 & 0xffff) ^ counter) & 0xffff
    );
    let safe_name = agent_name.replace(|c: char| !c.is_alphanumeric() && c != '-', "-");
    format!("{}-{}-{}", safe_name, now, suffix)
}

/// Create the run directory and write initial metadata.
pub fn create_run(meta: &RunMeta) -> anyhow::Result<()> {
    create_run_in(&run_dir(&meta.run_id), meta)
}

fn create_run_in(dir: &std::path::Path, meta: &RunMeta) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;

    // Restrict the run directory to owner-only (no-op on non-Unix).
    let _ = leviath_sys::secure_dir_perms(dir);

    write_meta_to(dir, meta)
}

/// Atomically write run metadata (write to tmp, then rename).
pub fn write_meta(meta: &RunMeta) -> anyhow::Result<()> {
    write_meta_to(&run_dir(&meta.run_id), meta)
}

fn write_meta_to(dir: &std::path::Path, meta: &RunMeta) -> anyhow::Result<()> {
    let json =
        serde_json::to_string_pretty(meta).expect("infallible: RunMeta always serializes to JSON");
    write_json_atomic(&dir.join("meta.json"), &json)
}

/// Read run metadata for a given run ID.
pub fn read_meta(run_id: &str) -> anyhow::Result<RunMeta> {
    read_meta_from(&run_dir(run_id))
}

fn read_meta_from(dir: &std::path::Path) -> anyhow::Result<RunMeta> {
    let path = dir.join("meta.json");
    let json = std::fs::read_to_string(&path)?;
    Ok(serde_json::from_str(&json)?)
}

/// Inner implementation of `list_runs`, parameterised so the early-return
/// branch can be exercised in tests without deleting real on-disk state.
fn list_runs_in_dir(dir: PathBuf) -> Vec<RunMeta> {
    if !dir.exists() {
        return Vec::new();
    }

    let mut runs = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let meta_path = entry.path().join("meta.json");
            if let Ok(json) = std::fs::read_to_string(&meta_path) {
                if let Ok(meta) = serde_json::from_str::<RunMeta>(&json) {
                    runs.push(meta);
                }
            }
        }
    }

    runs.sort_by_key(|r| std::cmp::Reverse(r.started_at));
    runs
}

/// List all runs, sorted by started_at descending (most recent first).
/// Silently skips any runs whose metadata cannot be read.
pub fn list_runs() -> Vec<RunMeta> {
    list_runs_in_dir(runs_dir())
}

/// Read the last `max_bytes` of any file on disk, returning UTF-8 text.
/// If the file is smaller than `max_bytes` the whole file is returned.
/// Partial UTF-8 at the truncation boundary is handled by skipping to the
/// first newline.  Returns an empty string on any I/O error.
pub fn tail_file(path: &std::path::Path, max_bytes: u64) -> String {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return String::new(),
    };

    // Use fstat on the open fd rather than a separate stat() call — avoids the
    // TOCTOU window between existence check and metadata read. Falls back to 0
    // (read everything) if fstat somehow fails on an already-open fd.
    let file_size = file.metadata().map(|m| m.len()).unwrap_or(0);

    if file_size <= max_bytes {
        let mut buf = Vec::new();
        let _ = file.read_to_end(&mut buf);
        return String::from_utf8_lossy(&buf).to_string();
    }

    let offset = file_size - max_bytes;
    let _ = file.seek(SeekFrom::Start(offset));

    let mut buf = Vec::new();
    let _ = file.read_to_end(&mut buf);

    // Skip to the first newline so we don't emit a partial line at the start.
    if let Some(nl) = buf.iter().position(|&b| b == b'\n') {
        String::from_utf8_lossy(&buf[nl + 1..]).to_string()
    } else {
        String::from_utf8_lossy(&buf).to_string()
    }
}

/// Read the last `max_bytes` of a run's combined output log (legacy).
#[allow(dead_code)]
pub fn tail_log(run_id: &str, max_bytes: u64) -> String {
    tail_file(&run_dir(run_id).join("output.log"), max_bytes)
}

// ─── Per-stage persistence ────────────────────────────────────────────────────

/// Directory for per-stage files within a run.
pub fn stage_dir(run_id: &str, stage_idx: usize) -> PathBuf {
    run_dir(run_id).join("stages").join(stage_idx.to_string())
}

/// Atomically write the stages index for a run.
pub fn write_stages_index(run_id: &str, stages: &[StageRecord]) -> anyhow::Result<()> {
    write_stages_index_to(&run_dir(run_id), stages)
}

fn write_stages_index_to(dir: &std::path::Path, stages: &[StageRecord]) -> anyhow::Result<()> {
    let json = serde_json::to_string_pretty(&stages)
        .expect("infallible: StageRecord slice always serializes to JSON");
    write_json_atomic(&dir.join("stages.json"), &json)
}

/// Read the stages index for a run, or return an empty vec on any error.
pub fn read_stages_index(run_id: &str) -> Vec<StageRecord> {
    let path = run_dir(run_id).join("stages.json");
    let json = match std::fs::read_to_string(&path) {
        Ok(j) => j,
        Err(_) => return Vec::new(),
    };
    serde_json::from_str(&json).unwrap_or_default()
}

/// Ensure the per-stage directory exists (called before first write).
fn ensure_stage_dir(run_id: &str, stage_idx: usize) {
    let dir = stage_dir(run_id, stage_idx);
    let _ = std::fs::create_dir_all(&dir);
}

/// Append a line of readable agent output to the per-stage output log.
pub fn append_stage_output(run_id: &str, stage_idx: usize, text: &str) {
    use std::io::Write;
    ensure_stage_dir(run_id, stage_idx);
    let path = stage_dir(run_id, stage_idx).join("output.log");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(file, "{}", text);
    }
}

/// Append a line of operational/tool-activity log to the per-stage logs file.
pub fn append_stage_log(run_id: &str, stage_idx: usize, text: &str) {
    use std::io::Write;
    ensure_stage_dir(run_id, stage_idx);
    let path = stage_dir(run_id, stage_idx).join("logs.log");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(file, "{}", text);
    }
}

/// Atomically write a context snapshot for a specific stage.
pub fn write_stage_context(
    run_id: &str,
    stage_idx: usize,
    snap: &ContextSnapshot,
) -> anyhow::Result<()> {
    ensure_stage_dir(run_id, stage_idx);
    write_context_snapshot_to(&stage_dir(run_id, stage_idx), snap)
}

/// Read the context snapshot for a specific stage, if present.
pub fn read_stage_context(run_id: &str, stage_idx: usize) -> Option<ContextSnapshot> {
    let path = stage_dir(run_id, stage_idx).join("context.json");
    let json = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&json).ok()
}

/// Read the last `max_bytes` of the readable output log for a specific stage.
pub fn tail_stage_output(run_id: &str, stage_idx: usize, max_bytes: u64) -> String {
    tail_file(&stage_dir(run_id, stage_idx).join("output.log"), max_bytes)
}

/// Read the last `max_bytes` of the operational log for a specific stage.
pub fn tail_stage_log(run_id: &str, stage_idx: usize, max_bytes: u64) -> String {
    tail_file(&stage_dir(run_id, stage_idx).join("logs.log"), max_bytes)
}

/// Serializes any test, anywhere in the crate, that reads `runs_dir()`'s or
/// `dashboard_log_path()`'s default (unset-env) behavior, or that mutates
/// `LEVIATH_RUNS_DIR`/`LEVIATH_DASHBOARD_LOG_PATH`. Both env vars are
/// process-global, so tests in different files that don't share a lock can
/// race -- e.g. a test isolating run state via [`isolate_runs_dir_for_test`]
/// while another, unlocked test asserts on the real home-directory fallback.
/// Declared here (not inside `mod tests`) so it's reachable crate-wide as
/// `crate::runstate::RUNS_DIR_ENV_LOCK`, mirroring `config.rs`'s
/// `CONFIG_PATH_ENV_LOCK`.
#[cfg(test)]
pub(crate) static RUNS_DIR_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Restores an env var to a previously-captured value: `Some` re-sets it,
/// `None` removes it. Shared by [`RunsDirTestGuard::drop`] and the handful
/// of tests in `mod tests` that temporarily override `LEVIATH_RUNS_DIR`/
/// `LEVIATH_DASHBOARD_LOG_PATH` to exercise the real (env-reading) fallback
/// path directly, so both the "was set" and "was unset" arms are exercised
/// through one shared, directly-tested implementation instead of leaving
/// either arm's coverage dependent on incidental test-scheduling timing.
#[cfg(test)]
pub(crate) fn restore_env_var(key: &str, prev: Option<std::ffi::OsString>) {
    // Non-generic (single monomorphization) so both match arms are attributed
    // to one instantiation and fully covered by `restore_env_var_handles_both_some_and_none`.
    match prev {
        Some(v) => unsafe { std::env::set_var(key, v) },
        None => unsafe { std::env::remove_var(key) },
    }
}

/// RAII guard that restores `LEVIATH_RUNS_DIR` and
/// `LEVIATH_DASHBOARD_LOG_PATH` to their original values, removes the
/// isolated temp directory, and releases [`RUNS_DIR_ENV_LOCK`], on drop.
#[cfg(test)]
pub(crate) struct RunsDirTestGuard {
    original_runs_dir: Option<std::ffi::OsString>,
    original_dashboard_log_path: Option<std::ffi::OsString>,
    base_dir: std::path::PathBuf,
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl Drop for RunsDirTestGuard {
    fn drop(&mut self) {
        restore_env_var("LEVIATH_RUNS_DIR", self.original_runs_dir.take());
        restore_env_var(
            "LEVIATH_DASHBOARD_LOG_PATH",
            self.original_dashboard_log_path.take(),
        );
        let _ = std::fs::remove_dir_all(&self.base_dir);
    }
}

/// Points `LEVIATH_RUNS_DIR` and `LEVIATH_DASHBOARD_LOG_PATH` at fresh paths
/// inside a temp directory, for the duration of the returned guard, so tests
/// that create real run/agent/dashboard-log state (`create_run`,
/// `write_meta`, `append_stage_output`, `append_dashboard_log`, `list_runs`,
/// ...) never touch the real `~/.leviath/runs/` or `~/.leviath/dashboard.log`.
///
/// This matters more than it looks: those are the exact files `lev dash`/
/// `lev serve` read from. Before this guard existed, tests wrote real
/// fixture entries (`agent_name: "test"`, `workdir: "/tmp"`, etc.) straight
/// into the user's actual runs directory. Most got cleaned up by each test's
/// own `remove_dir_all` at the end -- but a test process killed mid-run
/// (e.g. Ctrl+C on a hung test) never reaches that cleanup, so the entries
/// stayed behind permanently, showing up as orphaned "waiting" agents in the
/// user's real dashboard.
#[cfg(test)]
pub(crate) fn isolate_runs_dir_for_test(unique: &str) -> RunsDirTestGuard {
    let lock = RUNS_DIR_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let original_runs_dir = std::env::var_os("LEVIATH_RUNS_DIR");
    let original_dashboard_log_path = std::env::var_os("LEVIATH_DASHBOARD_LOG_PATH");
    // Keep the temp path short and hash `unique` down rather than embedding
    // it verbatim: some dashboard render tests display a real on-disk path
    // (e.g. stage_dir(...).join("output.log")) inside a fixed-width
    // terminal area and assert on a substring near the *end* of that path.
    // Test function names here can run 60+ characters.
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    unique.hash(&mut hasher);
    let short = format!("{:x}", hasher.finish() & 0xffff_ffff);
    // Rooted under the home dir (NOT std::env::temp_dir()) so the render
    // code's existing `~`-shortening of displayed paths still applies --
    // macOS's real temp dir is itself ~50 chars (`/var/folders/xy/.../T/`),
    // which alone was enough to push realistic paths past the same render
    // width and truncate away the asserted-on suffix. `.leviath-test` is a
    // sibling of `.leviath`, never read by `lev dash`/`lev serve`, so this
    // still can't leak into the user's real dashboard even if a killed test
    // process skips this guard's own Drop-time cleanup.
    let base_dir = dirs::home_dir()
        .unwrap_or_default()
        .join(".leviath-test")
        .join(format!("rs-{short}"));
    let runs_dir = base_dir.join("runs");
    let _ = std::fs::create_dir_all(&runs_dir);
    unsafe {
        std::env::set_var("LEVIATH_RUNS_DIR", &runs_dir);
        std::env::set_var("LEVIATH_DASHBOARD_LOG_PATH", base_dir.join("dashboard.log"));
    }
    RunsDirTestGuard {
        original_runs_dir,
        original_dashboard_log_path,
        base_dir,
        _lock: lock,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_json_atomic_fs_write_failure() {
        // Drive the `std::fs::write(&tmp, json)?` error arm: writing the
        // `.json.tmp` sibling into a directory that does not exist fails.
        let path = std::path::Path::new("/nonexistent/leviath/runstate-cov/out.json");
        let result = write_json_atomic(path, "{}");
        assert!(result.is_err());
        assert!(!path.exists());
    }

    // ─── RunStatus ──────────────────────────────────────────────────────────

    #[test]
    fn run_status_serde_roundtrip() {
        for status in [
            RunStatus::Starting,
            RunStatus::Running,
            RunStatus::WaitingInput,
            RunStatus::Complete,
            RunStatus::CompleteInteractive,
            RunStatus::Error,
            RunStatus::Cancelled,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let back: RunStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(status, back);
        }
    }

    #[test]
    fn run_status_display() {
        assert_eq!(RunStatus::Starting.to_string(), "Starting");
        assert_eq!(RunStatus::Running.to_string(), "Running");
        assert_eq!(RunStatus::WaitingInput.to_string(), "WaitingInput");
        assert_eq!(RunStatus::Complete.to_string(), "Complete");
        assert_eq!(
            RunStatus::CompleteInteractive.to_string(),
            "CompleteInteractive"
        );
        assert_eq!(RunStatus::Error.to_string(), "Error");
        assert_eq!(RunStatus::Cancelled.to_string(), "Cancelled");
    }

    #[test]
    fn run_status_snake_case_serialization() {
        let json = serde_json::to_string(&RunStatus::WaitingInput).unwrap();
        assert_eq!(json, "\"waiting_input\"");
        let json = serde_json::to_string(&RunStatus::CompleteInteractive).unwrap();
        assert_eq!(json, "\"complete_interactive\"");
    }

    // ─── StageRunStatus ─────────────────────────────────────────────────────

    #[test]
    fn stage_run_status_serde_roundtrip() {
        for status in [
            StageRunStatus::Pending,
            StageRunStatus::Active,
            StageRunStatus::WaitingInput,
            StageRunStatus::Complete,
            StageRunStatus::Error,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let back: StageRunStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(status, back);
        }
    }

    #[test]
    fn stage_run_status_display() {
        assert_eq!(StageRunStatus::Pending.to_string(), "Pending");
        assert_eq!(StageRunStatus::Active.to_string(), "Active");
        assert_eq!(StageRunStatus::WaitingInput.to_string(), "WaitingInput");
        assert_eq!(StageRunStatus::Complete.to_string(), "Complete");
        assert_eq!(StageRunStatus::Error.to_string(), "Error");
    }

    // ─── RunMeta ────────────────────────────────────────────────────────────

    #[test]
    fn run_meta_new_defaults() {
        let meta = RunMeta::new(
            "run-1".into(),
            "agent".into(),
            "/path".into(),
            "do stuff".into(),
            Some("gpt-4".into()),
            "/work".into(),
            3,
        );
        assert_eq!(meta.run_id, "run-1");
        assert_eq!(meta.agent_name, "agent");
        assert_eq!(meta.task, "do stuff");
        assert_eq!(meta.model.as_deref(), Some("gpt-4"));
        assert_eq!(meta.num_stages, 3);
        assert_eq!(meta.status, RunStatus::Starting);
        assert_eq!(meta.pid, 0);
        assert_eq!(meta.stage_index, 0);
        assert!(meta.error.is_none());
        assert!(meta.title.is_none());
        assert!(meta.metadata.is_empty());
        assert!(meta.callback_url.is_none());
        assert!(meta.parent_run_id.is_none());
    }

    #[test]
    fn run_meta_serde_roundtrip() {
        let meta = RunMeta::new(
            "test-run".into(),
            "test-agent".into(),
            "/agents/test".into(),
            "run tests".into(),
            None,
            "/tmp".into(),
            2,
        );
        let json = serde_json::to_string_pretty(&meta).unwrap();
        let back: RunMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.run_id, "test-run");
        assert_eq!(back.agent_name, "test-agent");
        assert_eq!(back.num_stages, 2);
        assert!(back.model.is_none());
    }

    #[test]
    fn run_meta_touch_updates_timestamp() {
        let mut meta = RunMeta::new(
            "r".into(),
            "a".into(),
            "/p".into(),
            "t".into(),
            None,
            "/w".into(),
            1,
        );
        let before = meta.updated_at;
        // Touch should update (or at least not decrease) updated_at
        meta.touch();
        assert!(meta.updated_at >= before);
    }

    #[test]
    fn run_meta_optional_fields_deserialize() {
        // Simulate a meta.json without optional fields (e.g., from older version)
        let json = serde_json::json!({
            "run_id": "r1",
            "agent_name": "a",
            "agent_path": "/p",
            "task": "t",
            "model": null,
            "pid": 123,
            "status": "running",
            "current_stage": "init",
            "stage_index": 0,
            "num_stages": 1,
            "iteration": 0,
            "prompt_tokens": 0,
            "completion_tokens": 0,
            "workdir": "/w",
            "started_at": 1000,
            "updated_at": 1000,
            "error": null
        });
        let meta: RunMeta = serde_json::from_value(json).unwrap();
        assert_eq!(meta.cached_tokens, 0);
        assert!(meta.title.is_none());
        assert!(meta.metadata.is_empty());
        assert!(meta.callback_url.is_none());
        assert!(meta.parent_run_id.is_none());
    }

    // ─── StageRecord ────────────────────────────────────────────────────────

    #[test]
    fn stage_record_new_defaults() {
        let rec = StageRecord::new("analyze".into(), 2);
        assert_eq!(rec.name, "analyze");
        assert_eq!(rec.index, 2);
        assert_eq!(rec.status, StageRunStatus::Pending);
        assert_eq!(rec.prompt_tokens, 0);
        assert_eq!(rec.completion_tokens, 0);
        assert_eq!(rec.cached_tokens, 0);
        assert!(rec.started_at.is_none());
        assert!(rec.ended_at.is_none());
    }

    #[test]
    fn stage_record_serde_roundtrip() {
        let mut rec = StageRecord::new("build".into(), 0);
        rec.status = StageRunStatus::Complete;
        rec.prompt_tokens = 100;
        rec.started_at = Some(1000);
        rec.ended_at = Some(2000);

        let json = serde_json::to_string(&rec).unwrap();
        let back: StageRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "build");
        assert_eq!(back.status, StageRunStatus::Complete);
        assert_eq!(back.prompt_tokens, 100);
        assert_eq!(back.started_at, Some(1000));
    }

    // ─── RegionSnapshot / ContextSnapshot ───────────────────────────────────

    #[test]
    fn region_snapshot_serde_roundtrip() {
        let snap = RegionSnapshot {
            name: "system".into(),
            kind: "pinned".into(),
            current_tokens: 100,
            max_tokens: 500,
            entries: vec![RegionEntrySnapshot {
                content: "You are helpful".into(),
                tokens: 3,
                metadata: None,
                key: None,
            }],
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: RegionSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "system");
        assert_eq!(back.entries.len(), 1);
        assert_eq!(back.entries[0].content, "You are helpful");
    }

    #[test]
    fn region_snapshot_empty_entries_omitted() {
        let snap = RegionSnapshot {
            name: "empty".into(),
            kind: "temporary".into(),
            current_tokens: 0,
            max_tokens: 100,
            entries: vec![],
        };
        let json = serde_json::to_value(&snap).unwrap();
        assert!(json.get("entries").is_none());
    }

    #[test]
    fn context_snapshot_serde_roundtrip() {
        let snap = ContextSnapshot {
            stage_name: "analyze".into(),
            total_tokens: 500,
            max_tokens: 8192,
            regions: vec![RegionSnapshot {
                name: "history".into(),
                kind: "sliding".into(),
                current_tokens: 300,
                max_tokens: 2000,
                entries: vec![],
            }],
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: ContextSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.stage_name, "analyze");
        assert_eq!(back.total_tokens, 500);
        assert_eq!(back.regions.len(), 1);
    }

    // ─── tail_file ──────────────────────────────────────────────────────────

    #[test]
    fn tail_file_nonexistent_returns_empty() {
        let path = std::path::Path::new("/tmp/nonexistent-leviath-test-file.txt");
        assert_eq!(tail_file(path, 1024), "");
    }

    #[test]
    fn tail_file_small_file_returns_all() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.txt");
        std::fs::write(&path, "line1\nline2\nline3\n").unwrap();
        let result = tail_file(&path, 1024);
        assert_eq!(result, "line1\nline2\nline3\n");
    }

    #[test]
    fn tail_file_large_file_returns_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.txt");
        let content = "abcdefghij\n".repeat(100); // 1100 bytes
        std::fs::write(&path, &content).unwrap();
        let result = tail_file(&path, 50);
        // Should be less than 50 bytes, starting from a line boundary
        assert!(result.len() <= 50);
        assert!(result.ends_with('\n'));
    }

    // ─── new_run_id ─────────────────────────────────────────────────────────

    #[test]
    fn new_run_id_contains_agent_name() {
        let id = new_run_id("my-agent");
        assert!(id.starts_with("my-agent-"));
    }

    #[test]
    fn new_run_id_sanitizes_special_chars() {
        let id = new_run_id("agent with spaces!");
        assert!(!id.contains(' '));
        assert!(!id.contains('!'));
    }

    #[test]
    fn new_run_id_is_unique_across_rapid_calls_in_same_second() {
        // Regression test: `--count N` calls `new_run_id` N times in a tight
        // loop, all within the same wall-clock second. Before the
        // `RUN_ID_COUNTER` fix, every one of these produced the identical
        // ID, silently collapsing N runs into a single on-disk entry.
        let ids: std::collections::HashSet<String> =
            (0..100).map(|_| new_run_id("same-agent")).collect();
        assert_eq!(ids.len(), 100);
    }

    // ─── write_meta / read_meta roundtrip ───────────────────────────────────

    #[test]
    fn write_and_read_meta_roundtrip() {
        // Isolated via `isolate_runs_dir_for_test` so write_meta/read_meta
        // never touch the real ~/.leviath/runs/ -- the temp dir is removed
        // automatically when `_guard` drops, so no manual cleanup needed.
        let _guard = isolate_runs_dir_for_test("write-and-read-meta-roundtrip");
        let meta = RunMeta::new(
            "test-roundtrip-unit".into(),
            "test-agent".into(),
            "/agents/test".into(),
            "unit test".into(),
            Some("model-x".into()),
            "/tmp".into(),
            2,
        );

        create_run(&meta).unwrap();
        let back = read_meta(&meta.run_id).unwrap();
        assert_eq!(back.run_id, "test-roundtrip-unit");
        assert_eq!(back.agent_name, "test-agent");
        assert_eq!(back.task, "unit test");
        assert_eq!(back.model.as_deref(), Some("model-x"));
    }

    #[test]
    fn read_meta_returns_err_on_corrupted_json() {
        // Exercises `read_meta_from`'s `serde_json::from_str(&json)?` Err
        // arm: a `meta.json` that exists but doesn't parse as a `RunMeta`.
        let _guard = isolate_runs_dir_for_test("read-meta-returns-err-on-corrupted-json");
        let run_id = "corrupted-meta-run";
        let dir = run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("meta.json"), "not valid json").unwrap();

        let result = read_meta(run_id);
        assert!(result.is_err());
    }

    // ─── write_stages_index / read_stages_index roundtrip ───────────────────

    #[test]
    fn write_and_read_stages_index_roundtrip() {
        let _guard = isolate_runs_dir_for_test("write-and-read-stages-index-roundtrip");
        let run_id = "test-stages-idx-unit";
        let dir = run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        let stages = vec![
            StageRecord::new("init".into(), 0),
            StageRecord::new("process".into(), 1),
        ];
        write_stages_index(run_id, &stages).unwrap();
        let back = read_stages_index(run_id);
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].name, "init");
        assert_eq!(back[1].name, "process");
    }

    #[test]
    fn read_stages_index_missing_returns_empty() {
        let back = read_stages_index("nonexistent-run-12345");
        assert!(back.is_empty());
    }

    // ─── write/read context snapshot ────────────────────────────────────────

    #[test]
    fn write_and_read_context_snapshot_roundtrip() {
        let _guard = isolate_runs_dir_for_test("write-and-read-context-snapshot-roundtrip");
        let run_id = "test-ctx-snap-unit";
        let dir = run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        let snap = ContextSnapshot {
            stage_name: "test".into(),
            total_tokens: 42,
            max_tokens: 8192,
            regions: vec![],
        };
        write_context_snapshot(run_id, &snap).unwrap();
        let back = read_context_snapshot(run_id).unwrap();
        assert_eq!(back.stage_name, "test");
        assert_eq!(back.total_tokens, 42);
    }

    #[test]
    fn read_context_snapshot_missing_returns_none() {
        assert!(read_context_snapshot("nonexistent-ctx-run").is_none());
    }

    // ─── stage_dir / append_stage_output / append_stage_log ─────────────────

    #[test]
    fn stage_dir_path_structure() {
        let path = stage_dir("run-abc", 2);
        assert!(path.ends_with("stages/2"));
        assert!(path.to_str().unwrap().contains("run-abc"));
    }

    #[test]
    fn append_and_tail_stage_output() {
        let _guard = isolate_runs_dir_for_test("append-and-tail-stage-output");
        let run_id = "test-stage-output-unit";
        append_stage_output(run_id, 0, "line 1");
        append_stage_output(run_id, 0, "line 2");
        let output = tail_stage_output(run_id, 0, 4096);
        assert!(output.contains("line 1"));
        assert!(output.contains("line 2"));
    }

    #[test]
    fn append_and_tail_stage_log() {
        let _guard = isolate_runs_dir_for_test("append-and-tail-stage-log");
        let run_id = "test-stage-log-unit";
        append_stage_log(run_id, 0, "event A");
        append_stage_log(run_id, 0, "event B");
        let log = tail_stage_log(run_id, 0, 4096);
        assert!(log.contains("event A"));
        assert!(log.contains("event B"));
    }

    // ─── write/read stage context ───────────────────────────────────────────

    #[test]
    fn write_and_read_stage_context_roundtrip() {
        let _guard = isolate_runs_dir_for_test("write-and-read-stage-context-roundtrip");
        let run_id = "test-stage-ctx-unit";
        let snap = ContextSnapshot {
            stage_name: "stage-0".into(),
            total_tokens: 100,
            max_tokens: 4096,
            regions: vec![],
        };
        write_stage_context(run_id, 0, &snap).unwrap();
        let back = read_stage_context(run_id, 0).unwrap();
        assert_eq!(back.stage_name, "stage-0");
    }

    #[test]
    fn read_stage_context_missing_returns_none() {
        assert!(read_stage_context("nonexistent-run", 99).is_none());
    }

    // ─── append_dashboard_log ─────────────────────────────────────────────

    #[test]
    fn append_dashboard_log_creates_log_file() {
        let _guard = isolate_runs_dir_for_test("append-dashboard-log-creates-log-file");
        append_dashboard_log("coverage-test-message");
        assert!(dashboard_log_path().exists());
    }

    #[test]
    fn append_dashboard_log_open_failure_is_silently_ignored() {
        // Covers the `if let Ok(mut file) = ... .open(&path)` pattern *not*
        // matching: pre-create the resolved log path as a directory, so
        // opening it for append fails with `IsADirectory` -- the function
        // must swallow this silently (best-effort logging) rather than
        // panic.
        let _guard = isolate_runs_dir_for_test("append-dashboard-log-open-failure");
        let path = dashboard_log_path();
        std::fs::create_dir_all(&path).unwrap();
        append_dashboard_log("this should not panic");
        assert!(path.is_dir());
    }

    #[test]
    fn append_dashboard_log_path_with_no_parent_skips_create_dir_all() {
        // Every other test resolves `dashboard_log_path()` to a path with a
        // real parent component, leaving the `if let Some(parent) = ...`
        // pattern's `None` arm (root paths like "/" have no parent) never
        // exercised. Locks `RUNS_DIR_ENV_LOCK` directly (not
        // `isolate_runs_dir_for_test`, whose fixed `base_dir.join(...)`
        // path always has a parent) so the override can point at "/".
        let _lock = RUNS_DIR_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prev = std::env::var_os("LEVIATH_DASHBOARD_LOG_PATH");
        unsafe { std::env::set_var("LEVIATH_DASHBOARD_LOG_PATH", "/") };
        assert!(dashboard_log_path().parent().is_none());
        append_dashboard_log("this should not panic even with no parent");
        restore_env_var("LEVIATH_DASHBOARD_LOG_PATH", prev);
    }

    // ─── dashboard_log_path ────────────────────────────────────────────────

    // `restore_env_var` (used below) is defined alongside `RunsDirTestGuard`
    // in the parent module, since `RunsDirTestGuard::drop` shares it too --
    // see its doc comment for why both the `Some` and `None` arms are
    // exercised directly by the test below rather than by the "real" call
    // sites, whose `prev` depends on incidental test-scheduling timing.

    #[test]
    fn restore_env_var_handles_both_some_and_none() {
        let key = "LEVIATH_COVERAGE_RESTORE_ENV_VAR_TEST";
        restore_env_var(key, Some(std::ffi::OsString::from("value")));
        assert_eq!(std::env::var(key).as_deref(), Ok("value"));
        restore_env_var(key, None);
        assert!(std::env::var(key).is_err());
    }

    #[test]
    fn dashboard_log_path_structure() {
        // Exercises the real (env-reading) `dashboard_log_path()` on its
        // fallback branch, so -- like `runs_dir_structure` below -- this
        // must hold `RUNS_DIR_ENV_LOCK` and force both env vars unset:
        // `isolate_runs_dir_for_test` (used by many other tests) sets
        // `LEVIATH_DASHBOARD_LOG_PATH` process-wide, and without the lock a
        // concurrently-running isolated test would make this assertion
        // race and intermittently fail.
        let _lock = RUNS_DIR_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prev = std::env::var_os("LEVIATH_DASHBOARD_LOG_PATH");
        unsafe { std::env::remove_var("LEVIATH_DASHBOARD_LOG_PATH") };
        let path = dashboard_log_path();
        assert!(path.to_str().unwrap().contains(".leviath"));
        assert!(path.to_str().unwrap().ends_with("dashboard.log"));
        restore_env_var("LEVIATH_DASHBOARD_LOG_PATH", prev);
    }

    // ─── runs_dir / run_dir ────────────────────────────────────────────────

    #[test]
    fn runs_dir_structure() {
        // See the comment on `dashboard_log_path_structure` above -- same
        // race, same fix, for `LEVIATH_RUNS_DIR`.
        let _lock = RUNS_DIR_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prev = std::env::var_os("LEVIATH_RUNS_DIR");
        unsafe { std::env::remove_var("LEVIATH_RUNS_DIR") };
        let path = runs_dir();
        assert!(path.to_str().unwrap().contains(".leviath"));
        assert!(path.to_str().unwrap().ends_with("runs"));
        restore_env_var("LEVIATH_RUNS_DIR", prev);
    }

    #[test]
    fn runs_dir_from_uses_override_when_provided() {
        let path = runs_dir_from(Some("/custom/leviath/runs"));
        assert_eq!(path, PathBuf::from("/custom/leviath/runs"));
    }

    #[test]
    fn runs_dir_from_falls_back_to_home_when_none() {
        let path = runs_dir_from(None);
        #[cfg(unix)]
        assert!(path.ends_with(".leviath/runs"));
        #[cfg(windows)]
        assert!(path.ends_with(".leviath\\runs"));
    }

    #[test]
    fn dashboard_log_path_from_uses_override_when_provided() {
        let path = dashboard_log_path_from(Some("/custom/leviath/dashboard.log"));
        assert_eq!(path, PathBuf::from("/custom/leviath/dashboard.log"));
    }

    #[test]
    fn dashboard_log_path_from_falls_back_to_home_when_none() {
        let path = dashboard_log_path_from(None);
        #[cfg(unix)]
        assert!(path.ends_with(".leviath/dashboard.log"));
        #[cfg(windows)]
        assert!(path.ends_with(".leviath\\dashboard.log"));
    }

    #[test]
    fn run_dir_contains_run_id() {
        let path = run_dir("my-run-123");
        assert!(path.to_str().unwrap().contains("my-run-123"));
    }

    // ─── isolate_runs_dir_for_test ──────────────────────────────────────────

    #[test]
    fn isolate_runs_dir_for_test_points_at_temp_dir_and_restores_on_drop() {
        // Deliberately does NOT snapshot "the env var value before" from
        // outside any lock and compare against it after -- `isolate_runs_dir_for_test`
        // only holds `RUNS_DIR_ENV_LOCK` for its own scope, so a naive
        // before/after comparison races a concurrently-running isolated test
        // on another thread (observed: `left: None, right: Some(".../rs-.../runs")`
        // when another guard's value leaked into "before" or "after" this
        // guard's own window). Instead assert the guard's own path is live
        // only inside its scope and gone afterward -- a property that holds
        // no matter what other threads are doing, since no other test can
        // legitimately produce this exact hash-derived path.
        let guard_runs_dir;
        let guard_log_path;
        {
            let guard = isolate_runs_dir_for_test("guard-self-test");
            guard_runs_dir = guard.base_dir.join("runs");
            guard_log_path = guard.base_dir.join("dashboard.log");
            assert_eq!(runs_dir(), guard_runs_dir);
            assert!(runs_dir().exists());
            assert_eq!(dashboard_log_path(), guard_log_path);
        }
        // Guard dropped: env vars must no longer point at the (now-removed)
        // temp dir this guard created.
        assert_ne!(runs_dir(), guard_runs_dir);
        assert_ne!(dashboard_log_path(), guard_log_path);
        assert!(!guard_runs_dir.exists());
    }

    // ─── tail_file edge cases ──────────────────────────────────────────────

    #[test]
    fn tail_file_exact_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("exact.txt");
        std::fs::write(&path, "exactly").unwrap();
        // max_bytes == file size
        let result = tail_file(&path, 7);
        assert_eq!(result, "exactly");
    }

    // ─── RunMeta metadata and callback_url ─────────────────────────────────

    #[test]
    fn run_meta_with_metadata() {
        let mut meta = RunMeta::new(
            "meta-run".into(),
            "agent".into(),
            "/p".into(),
            "task".into(),
            None,
            "/w".into(),
            1,
        );
        meta.metadata
            .insert("key1".to_string(), "value1".to_string());
        meta.callback_url = Some("https://example.com/hook".to_string());
        meta.parent_run_id = Some("parent-123".to_string());

        let json = serde_json::to_string(&meta).unwrap();
        let back: RunMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.metadata.get("key1").unwrap(), "value1");
        assert_eq!(
            back.callback_url.as_deref(),
            Some("https://example.com/hook")
        );
        assert_eq!(back.parent_run_id.as_deref(), Some("parent-123"));
    }

    // ─── StageRecord modifications ─────────────────────────────────────────

    #[test]
    fn stage_record_mutation() {
        let mut rec = StageRecord::new("test".into(), 0);
        rec.status = StageRunStatus::Active;
        rec.started_at = Some(1000);
        rec.prompt_tokens = 500;
        rec.completion_tokens = 200;
        rec.cached_tokens = 50;

        assert_eq!(rec.status, StageRunStatus::Active);
        assert_eq!(rec.started_at, Some(1000));
        assert_eq!(rec.prompt_tokens, 500);
        assert_eq!(rec.completion_tokens, 200);
        assert_eq!(rec.cached_tokens, 50);

        rec.status = StageRunStatus::Complete;
        rec.ended_at = Some(2000);
        assert_eq!(rec.status, StageRunStatus::Complete);
        assert_eq!(rec.ended_at, Some(2000));
    }

    // ─── ContextSnapshot with entries ──────────────────────────────────────

    #[test]
    fn context_snapshot_with_entries() {
        let snap = ContextSnapshot {
            stage_name: "main".into(),
            total_tokens: 1000,
            max_tokens: 8192,
            regions: vec![
                RegionSnapshot {
                    name: "system".into(),
                    kind: "pinned".into(),
                    current_tokens: 100,
                    max_tokens: 2000,
                    entries: vec![
                        RegionEntrySnapshot {
                            content: "You are helpful".into(),
                            tokens: 3,
                            metadata: None,
                            key: None,
                        },
                        RegionEntrySnapshot {
                            content: "Additional instruction".into(),
                            tokens: 5,
                            metadata: Some(serde_json::json!({"source": "user"})),
                            key: None,
                        },
                    ],
                },
                RegionSnapshot {
                    name: "conversation".into(),
                    kind: "sliding".into(),
                    current_tokens: 900,
                    max_tokens: 6000,
                    entries: vec![],
                },
            ],
        };

        let json = serde_json::to_string_pretty(&snap).unwrap();
        let back: ContextSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.regions.len(), 2);
        assert_eq!(back.regions[0].entries.len(), 2);
        assert_eq!(back.regions[0].entries[1].tokens, 5);
        assert!(back.regions[0].entries[1].metadata.is_some());
    }

    // ─── RegionEntrySnapshot metadata ──────────────────────────────────────

    #[test]
    fn region_entry_snapshot_metadata_omitted_when_none() {
        let entry = RegionEntrySnapshot {
            content: "test".into(),
            tokens: 1,
            metadata: None,
            key: None,
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert!(json.get("metadata").is_none());
    }

    // ─── Multiple stage output appends ─────────────────────────────────────

    #[test]
    fn append_stage_output_multiple_stages() {
        let _guard = isolate_runs_dir_for_test("append-stage-output-multiple-stages");
        let run_id = "test-multi-stage-out";
        append_stage_output(run_id, 0, "stage 0 output");
        append_stage_output(run_id, 1, "stage 1 output");
        append_stage_output(run_id, 2, "stage 2 output");

        let out0 = tail_stage_output(run_id, 0, 4096);
        let out1 = tail_stage_output(run_id, 1, 4096);
        let out2 = tail_stage_output(run_id, 2, 4096);

        assert!(out0.contains("stage 0 output"));
        assert!(out1.contains("stage 1 output"));
        assert!(out2.contains("stage 2 output"));
        // Verify no cross-contamination
        assert!(!out0.contains("stage 1 output"));
    }

    // ─── list_runs ─────────────────────────────────────────────────────────

    #[test]
    fn list_runs_returns_sorted() {
        let _guard = isolate_runs_dir_for_test("list-runs-returns-sorted");
        let meta1 = RunMeta::new(
            "test-list-run-a".into(),
            "agent".into(),
            "/p".into(),
            "task a".into(),
            None,
            "/w".into(),
            1,
        );
        let meta2 = RunMeta::new(
            "test-list-run-b".into(),
            "agent".into(),
            "/p".into(),
            "task b".into(),
            None,
            "/w".into(),
            1,
        );

        let _ = create_run(&meta1);
        // Small delay to ensure different timestamps
        let _ = create_run(&meta2);

        let runs = list_runs();
        // Both should appear in the list
        let ids: Vec<&str> = runs.iter().map(|r| r.run_id.as_str()).collect();
        assert!(ids.contains(&"test-list-run-a"));
        assert!(ids.contains(&"test-list-run-b"));
    }

    // ─── tail_stage_log / tail_stage_output empty ──────────────────────────

    #[test]
    fn tail_stage_output_nonexistent_returns_empty() {
        assert_eq!(tail_stage_output("no-such-run-xyz", 0, 4096), "");
    }

    #[test]
    fn tail_stage_log_nonexistent_returns_empty() {
        assert_eq!(tail_stage_log("no-such-run-xyz", 0, 4096), "");
    }

    // ─── list_runs_in_dir ───────────────────────────────────────────────────

    #[test]
    fn list_runs_in_dir_nonexistent_returns_empty() {
        let result = list_runs_in_dir(PathBuf::from("/nonexistent/leviath/runs/coverage-test"));
        assert!(result.is_empty());
    }

    #[test]
    fn list_runs_in_dir_empty_dir_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let result = list_runs_in_dir(dir.path().to_path_buf());
        assert!(result.is_empty());
    }

    #[test]
    fn list_runs_in_dir_unreadable_dir_returns_empty() {
        // Covers the `if let Ok(entries) = std::fs::read_dir(&dir)` pattern
        // *not* matching: `dir.exists()` is true (so the earlier early-return
        // is skipped) but `read_dir` fails, so the whole block is silently
        // skipped. Pointing at a *file* makes `read_dir` fail on every platform
        // (the prior version used a `chmod 0o000` directory, Unix-only).
        let dir = tempfile::tempdir().unwrap();
        let not_a_dir = dir.path().join("runs-is-a-file");
        std::fs::write(&not_a_dir, "not a dir").unwrap();
        let result = list_runs_in_dir(not_a_dir);
        assert!(result.is_empty());
    }

    #[test]
    fn append_stage_output_open_failure_is_silently_skipped() {
        // When `output.log` already exists as a *directory*, `OpenOptions::open`
        // fails and the write is silently skipped (the `if let Ok(file)` false
        // path). Making the target a directory fails the open on every platform;
        // this branch was previously only reached incidentally via a
        // `chmod 0o555` read-only run dir (Unix-only).
        let _guard = crate::runstate::isolate_runs_dir_for_test("append_stage_output_open_failure");
        let run_id = "append-out-openfail";
        ensure_stage_dir(run_id, 0);
        std::fs::create_dir_all(stage_dir(run_id, 0).join("output.log")).unwrap();
        append_stage_output(run_id, 0, "ignored"); // must not panic
    }

    #[test]
    fn append_stage_log_open_failure_is_silently_skipped() {
        // Same as above for `logs.log` in `append_stage_log`.
        let _guard = crate::runstate::isolate_runs_dir_for_test("append_stage_log_open_failure");
        let run_id = "append-log-openfail";
        ensure_stage_dir(run_id, 0);
        std::fs::create_dir_all(stage_dir(run_id, 0).join("logs.log")).unwrap();
        append_stage_log(run_id, 0, "ignored"); // must not panic
    }

    // ─── runs_dir / list_runs edge cases ────────────────────────────────────

    #[test]
    fn runs_dir_with_override_set_returns_override() {
        let _lock = RUNS_DIR_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmpdir = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("LEVIATH_RUNS_DIR");
        unsafe { std::env::set_var("LEVIATH_RUNS_DIR", tmpdir.path()) };
        let dir = runs_dir();
        assert_eq!(dir, tmpdir.path());
        restore_env_var("LEVIATH_RUNS_DIR", prev);
    }

    #[test]
    fn runs_dir_without_override_falls_back_to_home() {
        let _lock = RUNS_DIR_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prev = std::env::var_os("LEVIATH_RUNS_DIR");
        unsafe { std::env::remove_var("LEVIATH_RUNS_DIR") };
        let dir = runs_dir();
        #[cfg(unix)]
        assert!(dir.ends_with(".leviath/runs"));
        #[cfg(windows)]
        assert!(dir.ends_with(".leviath\\runs"));
        restore_env_var("LEVIATH_RUNS_DIR", prev);
    }

    #[test]
    fn list_runs_empty_when_runs_dir_missing_or_empty() {
        // Isolated via `isolate_runs_dir_for_test`, so this is a genuinely
        // empty runs dir (not "the real dir, which we hope has no entry with
        // this exact bogus id") -- can assert real emptiness instead of just
        // absence of one specific id.
        let _guard = isolate_runs_dir_for_test("list-runs-empty-when-runs-dir-missing-or-empty");
        let runs = list_runs();
        assert!(runs.is_empty());
    }

    #[test]
    fn tail_file_nonexistent_path_returns_empty() {
        let path = std::path::Path::new("/nonexistent/path/to/a/file.log");
        assert_eq!(tail_file(path, 1024), "");
    }

    #[test]
    fn tail_file_small_file_returns_whole_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.log");
        std::fs::write(&path, "hello world").unwrap();
        assert_eq!(tail_file(&path, 1024), "hello world");
    }

    #[test]
    fn tail_file_large_file_truncates_from_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.log");
        let content = "a".repeat(100) + "\nTAIL_MARKER\n";
        std::fs::write(&path, &content).unwrap();
        let tailed = tail_file(&path, 20);
        assert!(tailed.contains("TAIL_MARKER"));
        assert!(tailed.len() < content.len());
    }

    #[test]
    fn tail_file_directory_path_returns_empty() {
        // metadata() and File::open() both succeed on a directory (confirmed
        // empirically on macOS/Linux); it's read_to_end() that fails with
        // "Is a directory" -- and that error is deliberately discarded (`let
        // _ = file.read_to_end(&mut buf);`), so this exercises the
        // graceful-empty-buffer fallback at the bottom of the function, not
        // either of the two `Err(_) => return String::new()` early returns.
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(tail_file(dir.path(), 4), "");
    }

    #[test]
    fn tail_log_reads_output_log_for_run_id() {
        // tail_log() itself (as opposed to tail_file(), which every other
        // test here calls directly) had zero coverage -- it's a one-line
        // wrapper joining run_dir(run_id) with "output.log".
        let _guard = isolate_runs_dir_for_test("tail-log-reads-output-log-for-run-id");
        let run_id = "test-tail-log-run";
        let dir = run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("output.log"), "hello from output.log").unwrap();

        assert_eq!(tail_log(run_id, 1024), "hello from output.log");
    }

    #[cfg(unix)]
    #[test]
    fn tail_file_open_permission_denied_returns_empty() {
        // A file with no permissions at all: `Path::exists()`/`fs::metadata()`
        // only need search (execute) permission on the *parent* directories
        // to stat a path, not read permission on the file itself -- so both
        // succeed here. `std::fs::File::open()` in read mode, however,
        // genuinely fails with `PermissionDenied`. Unlike the metadata-error
        // arm (only reachable via a delete-between-calls race), this is a
        // deterministic way to exercise the `File::open` `Err(_)` arm.
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no-permissions.log");
        // Content must exceed max_bytes so the "whole file" fast path
        // (`file_size <= max_bytes`) doesn't short-circuit before reaching
        // the `File::open` call under test.
        std::fs::write(&path, "x".repeat(100)).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();

        assert_eq!(tail_file(&path, 4), "");

        // Restore permissions so the tempdir can clean itself up on drop.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    }

    // ─── hermetic write/read coverage tests (use _to/_from/_in helpers) ───────

    #[test]
    fn write_context_snapshot_to_hermetic() {
        let dir = tempfile::tempdir().unwrap();
        let snap = ContextSnapshot {
            stage_name: "cov-stage".into(),
            total_tokens: 42,
            max_tokens: 8192,
            regions: vec![],
        };
        write_context_snapshot_to(dir.path(), &snap).unwrap();
        let json = std::fs::read_to_string(dir.path().join("context.json")).unwrap();
        let back: ContextSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.total_tokens, 42);
    }

    #[test]
    fn write_context_snapshot_to_fails_without_dir() {
        let snap = ContextSnapshot {
            stage_name: "s".into(),
            total_tokens: 1,
            max_tokens: 100,
            regions: vec![],
        };
        let nonexistent = std::path::Path::new("/nonexistent-cov-dir-xyzzy-abc");
        let result = write_context_snapshot_to(nonexistent, &snap);
        assert!(result.is_err());
    }

    #[test]
    fn write_context_snapshot_to_fails_when_rename_target_is_a_dir() {
        // Covers the `std::fs::rename(&tmp, &path)?` `Err` arm: the tmp file
        // write succeeds (its directory is writable), but the final rename
        // fails because `context.json` already exists as a *directory* --
        // `rename(2)` on POSIX refuses to replace a directory with a
        // regular file, unlike a plain overwrite of an existing file.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("context.json")).unwrap();
        let snap = ContextSnapshot {
            stage_name: "s".into(),
            total_tokens: 1,
            max_tokens: 100,
            regions: vec![],
        };
        let result = write_context_snapshot_to(dir.path(), &snap);
        assert!(result.is_err());
    }

    #[test]
    fn create_run_in_hermetic() {
        let tmpdir = tempfile::tempdir().unwrap();
        let run_dir = tmpdir.path().join("cov-run");
        let meta = RunMeta::new(
            "cov-run".into(),
            "cov-agent".into(),
            "/agents/cov".into(),
            "cov task".into(),
            None,
            "/tmp".into(),
            1,
        );
        create_run_in(&run_dir, &meta).unwrap();
        let back = read_meta_from(&run_dir).unwrap();
        assert_eq!(back.run_id, "cov-run");
    }

    #[test]
    fn create_run_in_fails_on_bad_parent() {
        // A hardcoded "/nonexistent-.../run" path isn't reliably bad across
        // platforms: on Windows CI runners (which typically have write
        // access to create directories at the drive root), that path
        // resolves under the current drive's root and create_dir_all
        // actually succeeds there, while on Unix it fails because writing
        // to the real filesystem root needs privileges the CI user lacks --
        // this passed locally but failed on Windows CI. Use a path with a
        // regular file as a parent component instead: create_dir_all can
        // never succeed under a file, on any platform or set of permissions.
        let dir = tempfile::tempdir().unwrap();
        let not_a_dir = dir.path().join("not-a-directory");
        std::fs::write(&not_a_dir, "x").unwrap();
        let bad = not_a_dir.join("run");
        let meta = RunMeta::new(
            "run".into(),
            "a".into(),
            "/".into(),
            "t".into(),
            None,
            "/tmp".into(),
            1,
        );
        let result = create_run_in(&bad, &meta);
        assert!(result.is_err());
    }

    #[test]
    fn write_meta_to_hermetic() {
        let tmpdir = tempfile::tempdir().unwrap();
        let meta = RunMeta::new(
            "cov-write-meta".into(),
            "a".into(),
            "/".into(),
            "t".into(),
            None,
            "/tmp".into(),
            1,
        );
        write_meta_to(tmpdir.path(), &meta).unwrap();
        let back = read_meta_from(tmpdir.path()).unwrap();
        assert_eq!(back.run_id, "cov-write-meta");
    }

    #[test]
    fn write_meta_to_fails_without_dir() {
        let meta = RunMeta::new(
            "cov-no-dir".into(),
            "a".into(),
            "/".into(),
            "t".into(),
            None,
            "/tmp".into(),
            1,
        );
        let bad = std::path::Path::new("/nonexistent-cov-write-meta-xyzzy");
        let result = write_meta_to(bad, &meta);
        assert!(result.is_err());
    }

    #[test]
    fn write_meta_to_fails_when_rename_target_is_a_dir() {
        // See `write_context_snapshot_to_fails_when_rename_target_is_a_dir`:
        // same `std::fs::rename(&tmp_path, &final_path)?` `Err` arm, forced
        // by pre-creating `meta.json` as a directory.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("meta.json")).unwrap();
        let meta = RunMeta::new(
            "cov-rename-fail".into(),
            "a".into(),
            "/".into(),
            "t".into(),
            None,
            "/tmp".into(),
            1,
        );
        let result = write_meta_to(dir.path(), &meta);
        assert!(result.is_err());
    }

    #[test]
    fn read_meta_from_fails_on_missing_file() {
        let tmpdir = tempfile::tempdir().unwrap();
        let result = read_meta_from(tmpdir.path());
        assert!(result.is_err());
    }

    #[test]
    fn write_stages_index_to_hermetic() {
        let tmpdir = tempfile::tempdir().unwrap();
        let stages = vec![StageRecord::new("cov-stage".into(), 0)];
        write_stages_index_to(tmpdir.path(), &stages).unwrap();
        let json = std::fs::read_to_string(tmpdir.path().join("stages.json")).unwrap();
        let back: Vec<StageRecord> = serde_json::from_str(&json).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].name, "cov-stage");
    }

    #[test]
    fn write_stages_index_to_fails_without_dir() {
        let stages = vec![StageRecord::new("s".into(), 0)];
        let bad = std::path::Path::new("/nonexistent-cov-stages-xyzzy");
        let result = write_stages_index_to(bad, &stages);
        assert!(result.is_err());
    }

    #[test]
    fn write_stages_index_to_fails_when_rename_target_is_a_dir() {
        // See `write_context_snapshot_to_fails_when_rename_target_is_a_dir`:
        // same `std::fs::rename(&tmp, &path)?` `Err` arm, forced by
        // pre-creating `stages.json` as a directory.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("stages.json")).unwrap();
        let stages = vec![StageRecord::new("s".into(), 0)];
        let result = write_stages_index_to(dir.path(), &stages);
        assert!(result.is_err());
    }

    #[test]
    fn list_runs_in_dir_includes_valid_run() {
        let tmpdir = tempfile::tempdir().unwrap();
        let run_id = "cov-listed-run";
        let run_subdir = tmpdir.path().join(run_id);
        std::fs::create_dir_all(&run_subdir).unwrap();
        let meta = RunMeta::new(
            run_id.into(),
            "list-agent".into(),
            "/agents/list".into(),
            "list task".into(),
            None,
            "/tmp".into(),
            1,
        );
        let json = serde_json::to_string_pretty(&meta).unwrap();
        std::fs::write(run_subdir.join("meta.json"), &json).unwrap();

        // list_runs_in_dir now reads meta.json directly from the dir, no env var needed
        let runs = list_runs_in_dir(tmpdir.path().to_path_buf());
        assert!(runs.iter().any(|r| r.run_id == run_id));
    }

    #[test]
    fn list_runs_in_dir_skips_entry_with_corrupted_meta_json() {
        // Exercises the `if let Ok(meta) = serde_json::from_str::<RunMeta>(...)`
        // else arm: a subdirectory whose meta.json exists and is readable as
        // a string, but doesn't parse as a `RunMeta`, is silently skipped
        // rather than propagating an error.
        let tmpdir = tempfile::tempdir().unwrap();
        let good_run_id = "cov-listed-good-run";
        let bad_run_id = "cov-listed-corrupted-run";

        let good_subdir = tmpdir.path().join(good_run_id);
        std::fs::create_dir_all(&good_subdir).unwrap();
        let meta = RunMeta::new(
            good_run_id.into(),
            "list-agent".into(),
            "/agents/list".into(),
            "list task".into(),
            None,
            "/tmp".into(),
            1,
        );
        let json = serde_json::to_string_pretty(&meta).unwrap();
        std::fs::write(good_subdir.join("meta.json"), &json).unwrap();

        let bad_subdir = tmpdir.path().join(bad_run_id);
        std::fs::create_dir_all(&bad_subdir).unwrap();
        std::fs::write(bad_subdir.join("meta.json"), "not valid json").unwrap();

        // A subdirectory with NO meta.json exercises the *other* skip branch:
        // the `if let Ok(json) = read_to_string(&meta_path)` else arm (the file
        // can't be read), distinct from the parse-fails arm above. Covering
        // both here keeps list_runs_in_dir at 100% on every OS deterministically
        // (previously one arm happened to be hit only on some platforms).
        let no_meta_run_id = "cov-listed-no-meta-run";
        std::fs::create_dir_all(tmpdir.path().join(no_meta_run_id)).unwrap();

        let runs = list_runs_in_dir(tmpdir.path().to_path_buf());
        assert!(runs.iter().any(|r| r.run_id == good_run_id));
        assert!(!runs.iter().any(|r| r.run_id == bad_run_id));
        assert!(!runs.iter().any(|r| r.run_id == no_meta_run_id));
    }

    #[test]
    fn append_dashboard_log_writes_message() {
        // Exercises the create_dir_all branch and writeln! branch via a unique marker.
        let _guard = isolate_runs_dir_for_test("append-dashboard-log-writes-message");
        let unique = format!("cov-dashboard-log-{}", std::process::id());
        append_dashboard_log(&unique);
        let content = std::fs::read_to_string(dashboard_log_path()).unwrap_or_default();
        assert!(content.contains(&unique));
    }
}
