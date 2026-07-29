//! On-disk run state for background agent executions.
//!
//! Each run lives under `~/.leviath/runs/<run-id>/` with:
//! - `meta.json`    — run metadata, updated atomically (tmp + rename)
//! - `output.log`  — append-only combined worker stdout (legacy/fallback)
//! - `stages.json` — index of per-stage records
//! - `stages/<idx>/output.log` — readable agent output for that stage
//! - `stages/<idx>/logs.log`   — operational events + tool activity
//! - `stages/<idx>/context.json` — context snapshot for that stage
//!
//! The dashboard's activity log is persisted separately at:
//! - `~/.leviath/dashboard.log` — never cleared, appended across sessions

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

// The plain run-state data types (RunMeta, RunStatus, the snapshot structs, and
// the per-stage records) live in `leviath_core::run_meta`. Re-exported here so
// `crate::runstate::RunMeta` / `runstate::RunMeta` call sites across the cli
// resolve. All on-disk IO for these types remains in this module.
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
    // `write_private`: these files carry the run's full task prompt,
    // conversation and tool output — and `meta.json` carries the webhook
    // signing secret. They were written with a plain `fs::write` at the umask
    // default (typically 0644), protected only by the 0700 on the enclosing run
    // directory. That is one `chmod` away from being readable, and defence in
    // depth is the whole point of a mode on the file itself.
    leviath_sys::write_private(&tmp, json.as_bytes())?;
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

/// Read + parse a run's portable archive (`<run_dir>/run.lvr`), returning its
/// records, or `None` if the archive is missing or unreadable.
pub fn read_run_archive(run_id: &str) -> Option<Vec<leviath_core::run_archive::RunRecord>> {
    let path = run_dir(run_id).join("run.lvr");
    let bytes = std::fs::read(&path).ok()?;
    leviath_core::run_archive::read_archive(&mut bytes.as_slice())
        .ok()
        .map(|(_version, records)| records)
}

/// A run's context-window history: the full window (+ metadata) at each recorded
/// point over time, oldest first. Empty when there's no readable archive.
pub fn context_history(run_id: &str) -> Vec<leviath_core::run_archive::RunPoint> {
    read_run_archive(run_id)
        .map(|records| leviath_core::run_archive::replay_points(&records))
        .unwrap_or_default()
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Inner implementation of `runs_dir`, parameterised so it can be tested
/// without touching the process-global env. All callers go through `runs_dir`.
///
/// The fallback resolves through [`crate::config::leviath_home_dir`], not
/// `dirs::home_dir` directly, so `LEVIATH_HOME` redirects the runs dir like it
/// redirects the config, the control socket and the agents dir. It used to use
/// `dirs::home_dir` alone, which meant a test that set `LEVIATH_HOME` was
/// isolated everywhere *except* here and still wrote runs into the developer's
/// real `~/.leviath/runs`. `LEVIATH_RUNS_DIR` still wins over both.
fn runs_dir_from(env_override: Option<&str>) -> PathBuf {
    if let Some(dir) = env_override {
        return PathBuf::from(dir);
    }
    leviath_core::paths::data_dir()
        .unwrap_or_default()
        .join("runs")
}

/// Directory where all run state is stored.
pub fn runs_dir() -> PathBuf {
    runs_dir_from(std::env::var("LEVIATH_RUNS_DIR").ok().as_deref())
}

/// Directory for a specific run.
///
/// A `run_id` that is not a single safe path component resolves to
/// `<runs_dir>/<invalid>`, a name that cannot exist — so a caller that passes an
/// attacker-supplied id gets a miss rather than a traversal. `run_id` reaches
/// this from URL segments on `GET /api/agents/{id}/logs` and friends, where
/// `Path::join` would otherwise happily accept `../../` or an absolute path.
///
/// Returning a definitely-missing path rather than an `Option` keeps every
/// caller's "no such run" branch as the single failure path, instead of adding a
/// second one that all of them would have to handle identically.
pub fn run_dir(run_id: &str) -> PathBuf {
    if !leviath_core::is_safe_path_component(run_id) {
        tracing::warn!(run_id = %run_id, "rejected an unsafe run id");
        return runs_dir().join("<invalid>");
    }
    runs_dir().join(run_id)
}

/// Inner implementation of `dashboard_log_path`, parameterised so it can be
/// tested without touching the process-global env. All callers go through
/// `dashboard_log_path`.
fn dashboard_log_path_from(env_override: Option<&str>) -> PathBuf {
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
/// path. This function only *computes* a `PathBuf` — it never writes — so both
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
/// default [`dashboard_log_path`]. Silently ignores I/O errors — best-effort.
pub fn append_dashboard_log(msg: &str) {
    append_dashboard_log_to(&dashboard_log_path(), msg);
}

/// Append a timestamped line to the dashboard activity log at an explicit
/// `path`. Silently ignores I/O errors — the dashboard log is best-effort.
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
fn append_dashboard_log_capped(path: &Path, msg: &str, max_bytes: u64) {
    use std::io::Write;
    // Ensure the parent directory exists (first-run case).
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    roll_log_if_over_cap(path, max_bytes);
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        let _ = writeln!(file, "{} {}", timestamp, msg);
    }
}

/// The path the rolled (previous-generation) log is moved to: `<name>.1`.
fn rolled_log_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".1");
    PathBuf::from(name)
}

/// Roll the live log to `<name>.1` once it reaches `max_bytes`, replacing any
/// existing rolled file, so the live file restarts empty and at most one
/// previous generation is retained (bounded ~2×cap on disk). Best-effort — a
/// failed rename just leaves the log to keep growing rather than erroring.
fn roll_log_if_over_cap(path: &Path, max_bytes: u64) {
    let over = std::fs::metadata(path)
        .map(|m| m.len() >= max_bytes)
        .unwrap_or(false);
    if over {
        let _ = std::fs::rename(path, rolled_log_path(path));
    }
}

/// How many random bits go in a run ID's suffix, rendered as 12 hex digits.
/// Collisions only matter within one wall-clock second for one agent name, so 48
/// bits is many orders of magnitude more than needed while staying short enough
/// to read in `lev ps` and the dashboard.
const RUN_ID_ENTROPY_BITS: u32 = 48;

/// Generate a unique run ID: `<agent_name>-<timestamp>-<random>`.
///
/// The suffix is **random**, not derived. It used to be
/// `(now ^ (now >> 16) ^ counter)` over a process-local counter, which defended
/// a `lev run --count N` batch inside one process but degenerated to a pure
/// function of the current second across separate processes: three concurrent
/// `lev run` invocations all minted `fetcher-1785127214-8b48` and silently shared
/// one run directory. Nothing downstream detects that — `create_dir_all` is a
/// no-op on an existing directory and the persistence worker then last-writer-wins
/// over `meta.json` / `context.json` / `run.lvr`, interleaving two runs'
/// state irrecoverably.
///
/// The `<name>-<secs>-<hex>` shape is preserved: the timestamp keeps IDs sorting
/// and reading chronologically, and the dashboard's short-ID display
/// (`split('-').next_back()`) still lands on the unique component.
pub fn new_run_id(agent_name: &str) -> String {
    use rand::RngExt as _;
    let entropy: u64 = rand::rng().random::<u64>() >> (u64::BITS - RUN_ID_ENTROPY_BITS);
    let safe_name = agent_name.replace(|c: char| !c.is_alphanumeric() && c != '-', "-");
    format!("{}-{}-{:012x}", safe_name, now_secs(), entropy)
}

/// Create the run directory and write initial metadata.
pub fn create_run(meta: &RunMeta) -> anyhow::Result<()> {
    create_run_in(&run_dir(&meta.run_id), meta)
}

/// Create an explicit run directory and write initial metadata into it.
///
/// Callers that already know the directory should prefer this over
/// [`create_run`], which resolves it from the home directory — the daemon's
/// spawner stakes out the run dir under its own configured `runs_dir`.
pub(crate) fn create_run_in(dir: &std::path::Path, meta: &RunMeta) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;

    // Restrict the run directory to owner-only (no-op on non-Unix).
    let _ = leviath_sys::secure_dir_perms(dir);

    write_meta_to(dir, meta)
}

/// Atomically write run metadata (write to tmp, then rename).
pub fn write_meta(meta: &RunMeta) -> anyhow::Result<()> {
    write_meta_to(&run_dir(&meta.run_id), meta)
}

/// Atomically write `meta.json` into an explicit run directory.
///
/// Callers that already know the directory should prefer this over
/// [`write_meta`], which resolves it from the home directory — the daemon's
/// recovery pass works from its configured `runs_dir` instead.
pub(crate) fn write_meta_to(dir: &std::path::Path, meta: &RunMeta) -> anyhow::Result<()> {
    let json =
        serde_json::to_string_pretty(meta).expect("infallible: RunMeta always serializes to JSON");
    write_json_atomic(&dir.join("meta.json"), &json)
}

/// Read run metadata for a given run ID.
pub fn read_meta(run_id: &str) -> anyhow::Result<RunMeta> {
    read_meta_from(&run_dir(run_id))
}

/// Whether an on-disk run status means the run has finished and should be left
/// alone. `Starting`/`Running`/`WaitingInput` are all "still going" as far as
/// anything reading the runs dir is concerned.
pub fn is_terminal_status(status: &RunStatus) -> bool {
    matches!(
        status,
        RunStatus::Complete
            | RunStatus::CompleteInteractive
            | RunStatus::Error
            | RunStatus::Cancelled
    )
}

/// The outcome of forcing a run to a terminal state on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForceCancelOutcome {
    /// The run was live on disk and is now recorded `Cancelled`.
    Cancelled,
    /// The run was already finished; nothing was written.
    AlreadyTerminal,
    /// No run directory with that id exists.
    NoSuchRun,
    /// The directory exists but its metadata could not be rewritten.
    WriteFailed,
}

impl ForceCancelOutcome {
    /// Whether the id named a run at all — i.e. whether the cancel had a target,
    /// regardless of whether it needed to write anything.
    pub fn found_run(&self) -> bool {
        !matches!(self, Self::NoSuchRun)
    }
}

/// Force a run's on-disk metadata to `Cancelled`, in the runs dir resolved from
/// the environment. See [`force_cancel_in`].
pub fn force_cancel(run_id: &str) -> ForceCancelOutcome {
    force_cancel_in(&run_dir(run_id), now_secs())
}

/// Force the run in `run_dir` to `Cancelled`, stamping `updated_at` with `now`.
///
/// This is the floor under every kill path: it needs nothing but the filesystem,
/// so it works for a run the daemon can't rebuild (blueprint deleted, metadata
/// corrupt, died mid-spawn) and for a run whose daemon is gone entirely. Both
/// the daemon's force-terminator seam and `lev cancel --force` route here so
/// there is one definition of "terminated on disk".
///
/// A directory whose `meta.json` is missing or unparseable still gets a minimal
/// `Cancelled` record written: such a run is otherwise skipped by `list_runs`,
/// which makes it invisible *and* permanent.
pub fn force_cancel_in(run_dir: &Path, now: i64) -> ForceCancelOutcome {
    if !run_dir.is_dir() {
        return ForceCancelOutcome::NoSuchRun;
    }
    let run_id = run_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let cancelled = match read_meta_from(run_dir) {
        Ok(meta) if is_terminal_status(&meta.status) => return ForceCancelOutcome::AlreadyTerminal,
        Ok(meta) => RunMeta {
            status: RunStatus::Cancelled,
            updated_at: now,
            ..meta
        },
        // Unreadable metadata: synthesize just enough to record the outcome. The
        // run id is the directory name, which is the one field always recoverable.
        Err(_) => RunMeta {
            status: RunStatus::Cancelled,
            updated_at: now,
            error: Some("run metadata was unreadable; cancelled".to_string()),
            ..RunMeta::new(
                run_id.clone(),
                run_id,
                String::new(),
                String::new(),
                None,
                String::new(),
                0,
            )
        },
    };
    match write_meta_to(run_dir, &cancelled) {
        Ok(()) => ForceCancelOutcome::Cancelled,
        Err(e) => {
            // Formatted outside the macro: a method call inside a `%field` is
            // only evaluated when a subscriber visits the value, so it would go
            // unexercised under the tests' no-op subscriber.
            let path = run_dir.display().to_string();
            tracing::warn!(
                run_dir = %path,
                error = %e,
                "could not force a run to cancelled on disk"
            );
            ForceCancelOutcome::WriteFailed
        }
    }
}

/// Read run metadata out of an explicit run directory (the daemon works from its
/// own configured `runs_dir` rather than the home-resolved one).
pub(crate) fn read_meta_from(dir: &std::path::Path) -> anyhow::Result<RunMeta> {
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
            if let Ok(json) = std::fs::read_to_string(&meta_path)
                && let Ok(meta) = serde_json::from_str::<RunMeta>(&json)
            {
                runs.push(meta);
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

/// Build the isolated base directory for a run-state test and create its
/// `runs/` subdir. Returned so the caller's closure can plant fixtures under it.
///
/// Rooted under `~/.leviath-test/rs-<hash>` rather than `std::env::temp_dir()`:
/// some dashboard render tests display a real on-disk path inside a fixed-width
/// terminal area and assert on a substring near its *end*, and macOS's real
/// temp dir (`/var/folders/xy/.../T/`) is long enough to push realistic paths
/// past the render width and truncate the asserted suffix. `unique` is hashed
/// short for the same reason (test names run 60+ chars). `.leviath-test` is a
/// sibling of `.leviath`, never read by `lev dash`/`lev serve`, so even if a
/// killed test process skips cleanup it can't leak into the real dashboard.
#[cfg(test)]
fn make_runs_base_dir(unique: &str) -> std::path::PathBuf {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    unique.hash(&mut hasher);
    let short = format!("{:x}", hasher.finish() & 0xffff_ffff);
    let base_dir = dirs::home_dir()
        .unwrap_or_default()
        .join(".leviath-test")
        .join(format!("rs-{short}"));
    let _ = std::fs::create_dir_all(base_dir.join("runs"));
    base_dir
}

/// The env overrides that point run-state I/O at `base_dir` instead of the
/// real `~/.leviath/`. Handed to `temp_env` for scoped set-and-restore.
#[cfg(test)]
fn runs_dir_isolation_vars(
    base_dir: &std::path::Path,
) -> [(&'static str, Option<std::ffi::OsString>); 2] {
    [
        (
            "LEVIATH_RUNS_DIR",
            Some(base_dir.join("runs").into_os_string()),
        ),
        (
            "LEVIATH_DASHBOARD_LOG_PATH",
            Some(base_dir.join("dashboard.log").into_os_string()),
        ),
    ]
}

/// Runs `f` with `LEVIATH_RUNS_DIR`/`LEVIATH_DASHBOARD_LOG_PATH` pointed at a
/// fresh isolated temp directory (passed to `f`), restoring them afterwards.
/// Closure-scoped (not an RAII guard) because edition 2024 makes `set_var`
/// `unsafe`, which the crate forbids; `temp_env` serializes it process-wide.
#[cfg(test)]
pub(crate) fn with_isolated_runs_dir<R>(unique: &str, f: impl FnOnce(&std::path::Path) -> R) -> R {
    let base_dir = make_runs_base_dir(unique);
    let result = temp_env::with_vars(runs_dir_isolation_vars(&base_dir), || f(&base_dir));
    let _ = std::fs::remove_dir_all(&base_dir);
    result
}

/// Async counterpart of [`with_isolated_runs_dir`] for `#[tokio::test]`s.
#[cfg(test)]
pub(crate) async fn with_isolated_runs_dir_async<R, Fut>(
    unique: &str,
    f: impl FnOnce(std::path::PathBuf) -> Fut,
) -> R
where
    Fut: std::future::Future<Output = R>,
{
    let base_dir = make_runs_base_dir(unique);
    let result =
        temp_env::async_with_vars(runs_dir_isolation_vars(&base_dir), f(base_dir.clone())).await;
    let _ = std::fs::remove_dir_all(&base_dir);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `run_id` arrives from URL segments on `GET /api/agents/{id}/logs` and
    /// friends. `Path::join` neither normalizes `..` nor resists an absolute
    /// path, so an unvalidated id read files anywhere. An unsafe one resolves to
    /// a name that cannot exist, giving the caller a plain miss.
    #[test]
    fn run_dir_refuses_an_unsafe_run_id() {
        crate::test_support::with_tracing(|| {
            for bad in ["../../etc", "/etc/passwd", "..", "a/b"] {
                let dir = run_dir(bad);
                let shown = dir.display().to_string();
                assert!(dir.ends_with("<invalid>"), "{bad} resolved to {shown}");
                assert!(!dir.exists(), "{bad} must not resolve to a real path");
            }
            // An ordinary id is untouched.
            assert!(run_dir("run-abc123").ends_with("run-abc123"));
        });
    }

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
                kind: Default::default(),
                metadata: None,
                key: None,
                taint: Default::default(),
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
        // `--count N` calls `new_run_id` N times in a tight loop, all within the
        // same wall-clock second.
        let ids: std::collections::HashSet<String> =
            (0..100).map(|_| new_run_id("same-agent")).collect();
        assert_eq!(ids.len(), 100);
    }

    /// Split `<name>-<secs>-<hex>` from the right — the agent name itself may
    /// contain dashes.
    fn split_run_id(id: &str) -> (&str, &str) {
        let mut parts = id.rsplitn(3, '-');
        let suffix = parts.next().expect("run id has a suffix");
        let secs = parts.next().expect("run id has a timestamp");
        (secs, suffix)
    }

    #[test]
    fn new_run_id_suffix_is_random_not_derived_from_the_clock() {
        // The collision this guards against was *across processes*: the suffix
        // used to be `(now ^ (now >> 16) ^ counter)` over a process-local
        // counter that every new process starts at 0, so it degenerated to a
        // pure function of the current second. Three concurrent `lev run`
        // invocations all minted `fetcher-1785127214-8b48` and silently shared
        // one run directory. A fresh process has no state to vary, so the
        // property that has to hold is: IDs that share a timestamp still differ.
        let ids: Vec<String> = (0..200).map(|_| new_run_id("same-agent")).collect();
        let mut by_second: std::collections::HashMap<&str, Vec<&str>> =
            std::collections::HashMap::new();
        for id in &ids {
            let (secs, suffix) = split_run_id(id);
            by_second.entry(secs).or_default().push(suffix);
        }
        let mut largest = 0;
        for (secs, suffixes) in &by_second {
            let distinct: std::collections::HashSet<&&str> = suffixes.iter().collect();
            assert_eq!(
                distinct.len(),
                suffixes.len(),
                "two runs in second {secs} share a suffix: {suffixes:?}"
            );
            largest = largest.max(suffixes.len());
        }
        // 200 calls take microseconds, so they cannot all land in distinct
        // seconds — without this the assertion above would be vacuous.
        assert!(
            largest > 1,
            "expected IDs sharing a second, got {by_second:?}"
        );
    }

    // ─── write_meta / read_meta roundtrip ───────────────────────────────────

    #[test]
    fn write_and_read_meta_roundtrip() {
        // Isolated via `isolate_runs_dir_for_test` so write_meta/read_meta
        // never touch the real ~/.leviath/runs/ -- the temp dir is removed
        // automatically when `_guard` drops, so no manual cleanup needed.
        with_isolated_runs_dir("write-and-read-meta-roundtrip", |_d| {
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
        });
    }

    #[test]
    fn read_meta_returns_err_on_corrupted_json() {
        // Exercises `read_meta_from`'s `serde_json::from_str(&json)?` Err
        // arm: a `meta.json` that exists but doesn't parse as a `RunMeta`.
        with_isolated_runs_dir("read-meta-returns-err-on-corrupted-json", |_d| {
            let run_id = "corrupted-meta-run";
            let dir = run_dir(run_id);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("meta.json"), "not valid json").unwrap();

            let result = read_meta(run_id);
            assert!(result.is_err());
        });
    }

    // ─── write_stages_index / read_stages_index roundtrip ───────────────────

    #[test]
    fn write_and_read_stages_index_roundtrip() {
        with_isolated_runs_dir("write-and-read-stages-index-roundtrip", |_d| {
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
        });
    }

    #[test]
    fn read_stages_index_missing_returns_empty() {
        let back = read_stages_index("nonexistent-run-12345");
        assert!(back.is_empty());
    }

    // ─── write/read context snapshot ────────────────────────────────────────

    #[test]
    fn write_and_read_context_snapshot_roundtrip() {
        with_isolated_runs_dir("write-and-read-context-snapshot-roundtrip", |_d| {
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
        });
    }

    #[test]
    fn read_context_snapshot_missing_returns_none() {
        assert!(read_context_snapshot("nonexistent-ctx-run").is_none());
    }

    #[test]
    fn read_run_archive_roundtrips_and_context_history_replays() {
        with_isolated_runs_dir("read-run-archive-roundtrip", |_d| {
            use leviath_core::run_archive::{self, RunIdentity, RunRecord};
            let run_id = "archive-unit";
            std::fs::create_dir_all(run_dir(run_id)).unwrap();
            let mut buf = Vec::new();
            run_archive::write_archive_start(&mut buf, run_archive::RUN_ARCHIVE_VERSION).unwrap();
            let meta = RunMeta::new(
                run_id.to_string(),
                "a".to_string(),
                "/p".to_string(),
                "t".to_string(),
                None,
                "/w".to_string(),
                1,
            );
            run_archive::write_record(
                &mut buf,
                &RunRecord::Header {
                    identity: RunIdentity {
                        run_id: run_id.to_string(),
                        machine_id: "m".to_string(),
                        world_id: "w".to_string(),
                        created_at: 0,
                    },
                    meta: Box::new(meta),
                },
            )
            .unwrap();
            run_archive::write_record(
                &mut buf,
                &RunRecord::ContextCheckpoint {
                    snapshot: ContextSnapshot {
                        stage_name: "plan".to_string(),
                        total_tokens: 3,
                        max_tokens: 100,
                        regions: vec![],
                    },
                    at: 1,
                },
            )
            .unwrap();
            std::fs::write(run_dir(run_id).join("run.lvr"), &buf).unwrap();

            let records = read_run_archive(run_id).expect("archive read");
            assert_eq!(records.len(), 2);
            let history = context_history(run_id);
            assert_eq!(history.len(), 1);
            assert_eq!(history[0].context.stage_name, "plan");
        });
    }

    #[test]
    fn read_run_archive_missing_or_corrupt_returns_none() {
        with_isolated_runs_dir("read-run-archive-corrupt", |_d| {
            // Missing archive.
            assert!(read_run_archive("no-such-archive-run").is_none());
            assert!(context_history("no-such-archive-run").is_empty());
            // Corrupt archive (bad magic) → None, not a panic.
            let run_id = "corrupt-archive-unit";
            std::fs::create_dir_all(run_dir(run_id)).unwrap();
            std::fs::write(run_dir(run_id).join("run.lvr"), b"not an archive").unwrap();
            assert!(read_run_archive(run_id).is_none());
            assert!(context_history(run_id).is_empty());
        });
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
        with_isolated_runs_dir("append-and-tail-stage-output", |_d| {
            let run_id = "test-stage-output-unit";
            append_stage_output(run_id, 0, "line 1");
            append_stage_output(run_id, 0, "line 2");
            let output = tail_stage_output(run_id, 0, 4096);
            assert!(output.contains("line 1"));
            assert!(output.contains("line 2"));
        });
    }

    #[test]
    fn append_and_tail_stage_log() {
        with_isolated_runs_dir("append-and-tail-stage-log", |_d| {
            let run_id = "test-stage-log-unit";
            append_stage_log(run_id, 0, "event A");
            append_stage_log(run_id, 0, "event B");
            let log = tail_stage_log(run_id, 0, 4096);
            assert!(log.contains("event A"));
            assert!(log.contains("event B"));
        });
    }

    // ─── write/read stage context ───────────────────────────────────────────

    #[test]
    fn write_and_read_stage_context_roundtrip() {
        with_isolated_runs_dir("write-and-read-stage-context-roundtrip", |_d| {
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
        });
    }

    #[test]
    fn read_stage_context_missing_returns_none() {
        assert!(read_stage_context("nonexistent-run", 99).is_none());
    }

    // ─── append_dashboard_log ─────────────────────────────────────────────

    #[test]
    fn append_dashboard_log_creates_log_file() {
        with_isolated_runs_dir("append-dashboard-log-creates-log-file", |_d| {
            append_dashboard_log("coverage-test-message");
            assert!(dashboard_log_path().exists());
        });
    }

    #[test]
    fn append_dashboard_log_open_failure_is_silently_ignored() {
        // Covers the `if let Ok(mut file) = ... .open(&path)` pattern *not*
        // matching: pre-create the resolved log path as a directory, so
        // opening it for append fails with `IsADirectory` -- the function
        // must swallow this silently (best-effort logging) rather than
        // panic.
        with_isolated_runs_dir("append-dashboard-log-open-failure", |_d| {
            let path = dashboard_log_path();
            std::fs::create_dir_all(&path).unwrap();
            append_dashboard_log("this should not panic");
            assert!(path.is_dir());
        });
    }

    #[test]
    fn append_dashboard_log_path_with_no_parent_skips_create_dir_all() {
        // Every other test resolves `dashboard_log_path()` to a path with a
        // real parent component, leaving the `if let Some(parent) = ...`
        // pattern's `None` arm (root paths like "/" have no parent) never
        // exercised. `temp_env::with_var` points the override at "/" for the
        // closure's duration (serialized process-wide, then restored).
        temp_env::with_var("LEVIATH_DASHBOARD_LOG_PATH", Some("/"), || {
            assert!(dashboard_log_path().parent().is_none());
            append_dashboard_log("this should not panic even with no parent");
        });
    }

    #[test]
    fn dashboard_log_rolls_once_over_cap() {
        // A tiny cap so a couple of lines trips the roll. The over-cap live file
        // is moved to `<name>.1` and a fresh live file is started.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dashboard.log");
        append_dashboard_log_capped(&path, "first line well over the tiny cap", 8);
        // First write created the file; it now exceeds the 8-byte cap.
        assert!(path.exists());
        assert!(!rolled_log_path(&path).exists());
        // Second write sees the file over cap → rolls it and restarts.
        append_dashboard_log_capped(&path, "second", 8);
        let rolled = rolled_log_path(&path);
        assert!(rolled.exists(), "previous generation rolled to <name>.1");
        assert!(
            std::fs::read_to_string(&rolled)
                .unwrap()
                .contains("first line")
        );
        // The live file was restarted with only the newest line.
        let live = std::fs::read_to_string(&path).unwrap();
        assert!(live.contains("second"));
        assert!(!live.contains("first line"));
    }

    #[test]
    fn dashboard_log_does_not_roll_under_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dashboard.log");
        append_dashboard_log_capped(&path, "a", 1_000_000);
        append_dashboard_log_capped(&path, "b", 1_000_000);
        // Both lines are in the single live file; nothing was rolled.
        assert!(!rolled_log_path(&path).exists());
        let live = std::fs::read_to_string(&path).unwrap();
        assert!(live.contains("a") && live.contains("b"));
    }

    // ─── dashboard_log_path ────────────────────────────────────────────────

    #[test]
    fn dashboard_log_path_structure() {
        // Exercises the real (env-reading) `dashboard_log_path()` on its
        // fallback branch, so -- like `runs_dir_structure` below -- it forces
        // `LEVIATH_DASHBOARD_LOG_PATH` unset via `temp_env::with_var_unset`,
        // which also serializes against every other temp-env test so a
        // concurrently-isolated test can't race this assertion.
        temp_env::with_var_unset("LEVIATH_DASHBOARD_LOG_PATH", || {
            let path = dashboard_log_path();
            assert!(path.to_str().unwrap().contains(".leviath"));
            assert!(path.to_str().unwrap().ends_with("dashboard.log"));
        });
    }

    /// With no `LEVIATH_DASHBOARD_LOG_PATH`, the dashboard log must follow
    /// `LEVIATH_HOME` like every other data path. It resolved through the raw
    /// OS home before, so a fully isolated test session still appended to the
    /// developer's real `~/.leviath/dashboard.log`.
    #[test]
    fn dashboard_log_path_honors_leviath_home() {
        temp_env::with_vars(
            [
                ("LEVIATH_DASHBOARD_LOG_PATH", None),
                ("LEVIATH_HOME", Some("/custom/home")),
            ],
            || {
                assert_eq!(
                    dashboard_log_path(),
                    PathBuf::from("/custom/home/.leviath/dashboard.log")
                );
            },
        );
    }

    // ─── runs_dir / run_dir ────────────────────────────────────────────────

    #[test]
    fn runs_dir_structure() {
        // See the comment on `dashboard_log_path_structure` above -- same
        // race, same fix, for `LEVIATH_RUNS_DIR`.
        temp_env::with_var_unset("LEVIATH_RUNS_DIR", || {
            let path = runs_dir();
            assert!(path.to_str().unwrap().contains(".leviath"));
            assert!(path.to_str().unwrap().ends_with("runs"));
        });
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

    /// With no `LEVIATH_RUNS_DIR`, the runs dir must follow `LEVIATH_HOME` — the
    /// same home every other leviath path resolves through. Without this, setting
    /// `LEVIATH_HOME` isolates a test's config/socket/agents dir while its runs
    /// still land in the real `~/.leviath/runs`.
    #[test]
    fn runs_dir_follows_leviath_home() {
        temp_env::with_vars(
            [
                ("LEVIATH_RUNS_DIR", None::<&str>),
                ("LEVIATH_HOME", Some("/tmp/leviath-home-runs-test")),
            ],
            || {
                assert_eq!(
                    runs_dir(),
                    PathBuf::from("/tmp/leviath-home-runs-test")
                        .join(".leviath")
                        .join("runs")
                );
            },
        );
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

    // ─── with_isolated_runs_dir ─────────────────────────────────────────────

    #[test]
    fn with_isolated_runs_dir_points_at_temp_dir_and_cleans_up_after() {
        // Deliberately avoids a racy before/after ambient comparison (a
        // concurrently-isolated test could own `LEVIATH_RUNS_DIR` just before
        // or after this closure's temp-env window): instead assert the helper's
        // own hash-derived path is live *inside* the closure and removed
        // afterward -- a property no other test can perturb, since none
        // produces this exact path.
        let inside = with_isolated_runs_dir("helper-self-test", |base_dir| {
            let expected = base_dir.join("runs");
            assert_eq!(runs_dir(), expected);
            assert!(runs_dir().exists());
            assert_eq!(dashboard_log_path(), base_dir.join("dashboard.log"));
            expected
        });
        // Closure returned: the temp dir the helper created is gone.
        assert!(!inside.exists());
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

    #[test]
    fn tail_file_tail_without_newline_returns_whole_window() {
        // When the last `max_bytes` window of a larger file contains no '\n'
        // at all (a single long line with no line breaks), `tail_file` cannot
        // skip to a newline boundary, so it falls through to the `else` arm and
        // returns the whole (newline-free) tail window verbatim. Bytes are
        // written raw (never via `writeln!`, which would append '\n') so that
        // on *every* OS the tail slice is guaranteed newline-free -- on Windows
        // ordinary text output is `\r\n`-terminated, which would otherwise keep
        // a '\n' in the window and take the `if` arm instead.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no_newline.txt");
        // 100 raw bytes, no newline anywhere.
        let content = "a".repeat(100);
        std::fs::write(&path, content.as_bytes()).unwrap();
        // A 10-byte window is smaller than the file (100) and contains no '\n'.
        let result = tail_file(&path, 10);
        assert_eq!(result, "aaaaaaaaaa");
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
                            kind: Default::default(),
                            metadata: None,
                            key: None,
                            taint: Default::default(),
                        },
                        RegionEntrySnapshot {
                            content: "Additional instruction".into(),
                            tokens: 5,
                            kind: Default::default(),
                            metadata: Some(serde_json::json!({"source": "user"})),
                            key: None,
                            taint: Default::default(),
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
            kind: Default::default(),
            metadata: None,
            key: None,
            taint: Default::default(),
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert!(json.get("metadata").is_none());
    }

    // ─── Multiple stage output appends ─────────────────────────────────────

    #[test]
    fn append_stage_output_multiple_stages() {
        with_isolated_runs_dir("append-stage-output-multiple-stages", |_d| {
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
        });
    }

    // ─── list_runs ─────────────────────────────────────────────────────────

    #[test]
    fn list_runs_returns_sorted() {
        with_isolated_runs_dir("list-runs-returns-sorted", |_d| {
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
        });
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
        // skipped. Pointing at a *file* makes `read_dir` fail on every platform.
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
        // path). Making the target a directory fails the open on every platform.
        crate::runstate::with_isolated_runs_dir("append_stage_output_open_failure", |_d| {
            let run_id = "append-out-openfail";
            ensure_stage_dir(run_id, 0);
            std::fs::create_dir_all(stage_dir(run_id, 0).join("output.log")).unwrap();
            append_stage_output(run_id, 0, "ignored"); // must not panic
        });
    }

    #[test]
    fn append_stage_log_open_failure_is_silently_skipped() {
        // Same as above for `logs.log` in `append_stage_log`.
        crate::runstate::with_isolated_runs_dir("append_stage_log_open_failure", |_d| {
            let run_id = "append-log-openfail";
            ensure_stage_dir(run_id, 0);
            std::fs::create_dir_all(stage_dir(run_id, 0).join("logs.log")).unwrap();
            append_stage_log(run_id, 0, "ignored"); // must not panic
        });
    }

    // ─── runs_dir / list_runs edge cases ────────────────────────────────────

    #[test]
    fn runs_dir_with_override_set_returns_override() {
        let tmpdir = tempfile::tempdir().unwrap();
        temp_env::with_var("LEVIATH_RUNS_DIR", Some(tmpdir.path()), || {
            assert_eq!(runs_dir(), tmpdir.path());
        });
    }

    #[test]
    fn runs_dir_without_override_falls_back_to_home() {
        temp_env::with_var_unset("LEVIATH_RUNS_DIR", || {
            let dir = runs_dir();
            #[cfg(unix)]
            assert!(dir.ends_with(".leviath/runs"));
            #[cfg(windows)]
            assert!(dir.ends_with(".leviath\\runs"));
        });
    }

    #[test]
    fn list_runs_empty_when_runs_dir_missing_or_empty() {
        // Isolated via `isolate_runs_dir_for_test`, so this is a genuinely
        // empty runs dir (not "the real dir, which we hope has no entry with
        // this exact bogus id") -- can assert real emptiness instead of just
        // absence of one specific id.
        with_isolated_runs_dir("list-runs-empty-when-runs-dir-missing-or-empty", |_d| {
            let runs = list_runs();
            assert!(runs.is_empty());
        });
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
        // both here keeps list_runs_in_dir at 100% on every OS deterministically.
        let no_meta_run_id = "cov-listed-no-meta-run";
        std::fs::create_dir_all(tmpdir.path().join(no_meta_run_id)).unwrap();

        let runs = list_runs_in_dir(tmpdir.path().to_path_buf());
        assert!(runs.iter().any(|r| r.run_id == good_run_id));
        assert!(!runs.iter().any(|r| r.run_id == bad_run_id));
        assert!(!runs.iter().any(|r| r.run_id == no_meta_run_id));
    }

    // ─── force_cancel_in: the floor under every kill path ───

    /// Write a run dir with `status` and return its path.
    fn run_dir_with(base: &std::path::Path, run_id: &str, status: RunStatus) -> PathBuf {
        let dir = base.join(run_id);
        let meta = RunMeta {
            status,
            ..RunMeta::new(
                run_id.into(),
                "a".into(),
                "/p".into(),
                "t".into(),
                None,
                "/w".into(),
                1,
            )
        };
        create_run_in(&dir, &meta).unwrap();
        dir
    }

    #[test]
    fn force_cancel_terminates_every_non_terminal_status() {
        let base = tempfile::tempdir().unwrap();
        for status in [
            RunStatus::Starting,
            RunStatus::Running,
            RunStatus::WaitingInput,
        ] {
            let dir = run_dir_with(base.path(), &format!("live-{status}"), status.clone());
            assert_eq!(force_cancel_in(&dir, 99), ForceCancelOutcome::Cancelled);
            let meta = read_meta_from(&dir).unwrap();
            assert_eq!(meta.status, RunStatus::Cancelled, "{status} is killable");
            assert_eq!(meta.updated_at, 99, "the cancel is stamped");
        }
    }

    #[test]
    fn force_cancel_leaves_a_finished_run_alone() {
        let base = tempfile::tempdir().unwrap();
        for status in [
            RunStatus::Complete,
            RunStatus::CompleteInteractive,
            RunStatus::Error,
            RunStatus::Cancelled,
        ] {
            let dir = run_dir_with(base.path(), &format!("done-{status}"), status.clone());
            assert_eq!(
                force_cancel_in(&dir, 99),
                ForceCancelOutcome::AlreadyTerminal,
                "{status} is already finished"
            );
            assert_eq!(read_meta_from(&dir).unwrap().status, status);
        }
    }

    #[test]
    fn force_cancel_reports_no_such_run_for_a_missing_directory() {
        let base = tempfile::tempdir().unwrap();
        let outcome = force_cancel_in(&base.path().join("ghost"), 99);
        assert_eq!(outcome, ForceCancelOutcome::NoSuchRun);
        assert!(!outcome.found_run(), "nothing to cancel");
    }

    /// A run dir whose metadata can't be parsed still gets terminated. Such a run
    /// is skipped by `list_runs`, so leaving it alone makes it both invisible and
    /// permanent — the one state from which there is no way back.
    #[test]
    fn force_cancel_writes_a_record_over_unreadable_metadata() {
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("corrupt-run");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("meta.json"), "{ not json").unwrap();

        assert_eq!(force_cancel_in(&dir, 99), ForceCancelOutcome::Cancelled);
        let meta = read_meta_from(&dir).expect("now parses");
        assert_eq!(meta.status, RunStatus::Cancelled);
        assert_eq!(meta.run_id, "corrupt-run", "recovered from the dir name");
        assert!(meta.error.is_some(), "records why it was synthesized");
    }

    /// A directory that exists but can't be written still counts as "found" — the
    /// caller must not report "no such run" for a run that plainly exists.
    #[test]
    fn force_cancel_reports_a_write_failure_but_still_found_the_run() {
        crate::test_support::with_tracing(|| {
            let base = tempfile::tempdir().unwrap();
            let dir = base.path().join("blocked-run");
            std::fs::create_dir_all(&dir).unwrap();
            // A directory where `meta.json` must go: the rename can't succeed.
            std::fs::create_dir_all(dir.join("meta.json")).unwrap();

            let outcome = force_cancel_in(&dir, 99);
            assert_eq!(outcome, ForceCancelOutcome::WriteFailed);
            assert!(outcome.found_run());
        });
    }

    #[test]
    fn append_dashboard_log_writes_message() {
        // Exercises the create_dir_all branch and writeln! branch via a unique marker.
        with_isolated_runs_dir("append-dashboard-log-writes-message", |_d| {
            let unique = format!("cov-dashboard-log-{}", std::process::id());
            append_dashboard_log(&unique);
            let content = std::fs::read_to_string(dashboard_log_path()).unwrap_or_default();
            assert!(content.contains(&unique));
        });
    }
}
