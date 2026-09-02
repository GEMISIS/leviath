//! On-disk run state for background agent executions.
//!
//! Each run lives under `~/.leviath/runs/<run-id>/` with:
//! - `meta.json`    - run metadata, updated atomically (tmp + rename)
//! - `output.log`  - append-only combined worker stdout (legacy/fallback)
//! - `stages.json` - index of per-stage records
//! - `stages/<idx>/output.log` - readable agent output for that stage
//! - `stages/<idx>/logs.log`   - operational events + tool activity
//! - `stages/<idx>/context.json` - context snapshot for that stage
//!
//! The dashboard's activity log is persisted separately at:
//! - `~/.leviath/dashboard.log` - never cleared, appended across sessions
//!
//! # Who writes, and which copy is authoritative
//!
//! There are two answers to "what runs exist", and that is deliberate. The ECS
//! world is the live one: it knows wait reasons and tick-fresh progress for the
//! runs the daemon is holding right now, and `host.rs`'s `list()` reads it.
//! The runs directory is the durable one: it survives a crash or a daemon that
//! is not running, and `list_runs` below reads it. Disk lags the world by at
//! most one persistence tick, so the two disagreeing is expected rather than a
//! bug, and every reconciliation of that gap goes through `looks_abandoned`.
//!
//! The runtime's `persistence_bridge` is the only thing that writes a live
//! run's state. The writers in this module are `#[cfg(test)]` so that stays
//! true by compilation rather than by convention: a test can lay down a run
//! directory to read back, and production has no second path to the same files.

use std::path::{Path, PathBuf};
use std::sync::Arc;

mod dashboard_log;
mod force;
#[cfg(test)]
pub(crate) use dashboard_log::append_dashboard_log;
#[cfg(test)]
use dashboard_log::*;
pub(crate) use dashboard_log::{append_dashboard_log_to, dashboard_log_path};
pub(crate) use force::{ForceCancelOutcome, force_cancel, force_cancel_in, force_error_in};

// The plain run-state data types (RunMeta, RunStatus, the snapshot structs, and
// the per-stage records) live in `leviath_core::run_meta`. Re-exported here so
// `crate::runstate::RunMeta` / `runstate::RunMeta` call sites across the cli
// resolve. All on-disk IO for these types remains in this module.
pub(crate) use leviath_core::run_meta::{
    ContextSnapshot, RunMeta, RunStatus, StageRecord, StageRunStatus,
};
#[cfg(test)]
pub(crate) use leviath_core::run_meta::{RegionEntrySnapshot, RegionSnapshot};

/// Atomically write a context snapshot for the run.
///
/// Test-only. Production writes go through the runtime's `persistence_bridge`,
/// which is the sole writer of a live run's on-disk state; this exists so a
/// test can lay down a run directory to read back. See the module doc.
#[cfg(test)]
pub(crate) fn write_context_snapshot(run_id: &str, snap: &ContextSnapshot) -> anyhow::Result<()> {
    write_context_snapshot_to(&run_dir(run_id), snap)
}

/// Atomically write pre-serialized `json` to `path` (via a `.json.tmp`
/// sibling + rename).
///
/// Non-generic (takes an already-serialized string) so it has a single
/// monomorphization and every region - including the `std::fs` error `?`
/// arms - is exercised by real tests. Serialization is performed by the
/// callers, whose concrete production types
/// (`ContextSnapshot`/`RunMeta`/`&[StageRecord]`) are provably infallible to
/// serialize (see the `.expect` sites).
/// Write `body` to `path` atomically, readable only by this user.
///
/// Not JSON-specific despite where it started: the final-output sidecar is raw
/// content, and wants the same private-then-rename treatment for the same
/// reason.
fn write_private_atomic(path: &std::path::Path, body: &str) -> anyhow::Result<()> {
    let tmp = path.with_extension("tmp");
    // `write_private`: these files carry the run's full task prompt,
    // conversation and tool output - and `meta.json` carries the webhook
    // signing secret. They were written with a plain `fs::write` at the umask
    // default (typically 0644), protected only by the 0700 on the enclosing run
    // directory. That is one `chmod` away from being readable, and defence in
    // depth is the whole point of a mode on the file itself.
    leviath_sys::write_private(&tmp, body.as_bytes())?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
fn write_context_snapshot_to(dir: &std::path::Path, snap: &ContextSnapshot) -> anyhow::Result<()> {
    let json = serde_json::to_string_pretty(snap)
        .expect("infallible: ContextSnapshot always serializes to JSON");
    write_private_atomic(&dir.join(leviath_core::files::CONTEXT_FILE), &json)
}

/// Read the context snapshot for a run, if present.
pub(crate) fn read_context_snapshot(run_id: &str) -> Option<ContextSnapshot> {
    let path = run_dir(run_id).join(leviath_core::files::CONTEXT_FILE);
    let json = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&json).ok()
}

/// A parse cache keyed by a file's `(mtime, len)`: the file is re-read and
/// re-parsed only when its stat changes.
///
/// For pollers reading run state on a tick. The dashboard synced at 10Hz by
/// re-parsing every run's `meta.json`, `stages.json`, and whole
/// `context.json`; with 50 runs on disk that was on the order of 100 MB/s of
/// allocate-and-parse-and-free for files that change at most once per persist
/// tick. A `stat` costs microseconds; this turns the steady-state tick into
/// stats plus clones of shared `Arc`s.
///
/// `(mtime, len)` rather than mtime alone: the persistence lane's atomic
/// rename gives every update a fresh temp inode and mtime, but coarse mtime
/// granularity on some filesystems can miss two updates in the same instant -
/// the length check catches most of those, and a same-length same-instant
/// rewrite is indistinguishable anyway one tick later.
pub(crate) struct StatCache<T> {
    entries: std::collections::HashMap<PathBuf, CacheEntry<T>>,
}

/// One cached file: the stat it was last read at, when that stat was taken,
/// and what it parsed to.
struct CacheEntry<T> {
    mtime: std::time::SystemTime,
    len: u64,
    checked: std::time::Instant,
    value: Option<Arc<T>>,
}

impl<T> Default for StatCache<T> {
    fn default() -> Self {
        Self {
            entries: std::collections::HashMap::new(),
        }
    }
}

impl<T> StatCache<T> {
    /// The value parsed from `path`, re-reading only when the file's stat
    /// changed since the last call. `None` when the file is missing,
    /// unreadable, or `parse` rejects it - negative results are cached too, so
    /// a persistently-bad file costs one stat per tick, not one parse.
    pub(crate) fn get_with(
        &mut self,
        path: &Path,
        parse: impl FnOnce(&str) -> Option<T>,
    ) -> Option<Arc<T>> {
        self.get_with_recheck(path, parse, std::time::Duration::ZERO)
    }

    /// [`get_with`](Self::get_with), skipping the stat when the entry was
    /// checked less than `recheck_after` ago.
    ///
    /// The stat is the cost. A dashboard over 750 runs stat'ed 1,500 files
    /// ten times a second to learn that 1,490 of them, belonging to runs that
    /// finished days ago, had not changed; two thirds of its idle CPU was that
    /// question. A caller that knows a file has settled (a finished run's
    /// record) asks it once a second instead, and a file it knows is live
    /// passes `Duration::ZERO` and is stat'ed every time, as before.
    pub(crate) fn get_with_recheck(
        &mut self,
        path: &Path,
        parse: impl FnOnce(&str) -> Option<T>,
        recheck_after: std::time::Duration,
    ) -> Option<Arc<T>> {
        if let Some(entry) = self.entries.get(path)
            && entry.checked.elapsed() < recheck_after
        {
            return entry.value.clone();
        }
        let Ok(meta) = std::fs::metadata(path) else {
            self.entries.remove(path);
            return None;
        };
        // A filesystem with no mtimes degrades to epoch (so length changes
        // still refresh) rather than growing an unreachable error arm.
        let stamp = (meta.modified().unwrap_or(std::time::UNIX_EPOCH), meta.len());
        let checked = std::time::Instant::now();
        if let Some(entry) = self.entries.get_mut(path)
            && (entry.mtime, entry.len) == stamp
        {
            entry.checked = checked;
            return entry.value.clone();
        }
        let value = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| parse(&text))
            .map(Arc::new);
        self.entries.insert(
            path.to_path_buf(),
            CacheEntry {
                mtime: stamp.0,
                len: stamp.1,
                checked,
                value: value.clone(),
            },
        );
        value
    }

    /// The cached value for `path`, without asking the filesystem anything.
    pub(crate) fn peek(&self, path: &Path) -> Option<Arc<T>> {
        self.entries.get(path).and_then(|entry| entry.value.clone())
    }

    /// Drop entries for files under runs that no longer exist, so a
    /// long-lived poller's cache stays bounded by the live run set.
    pub(crate) fn retain_under(&mut self, keep: &std::collections::HashSet<PathBuf>) {
        self.entries.retain(|path, _| {
            path.parent()
                .is_some_and(|dir| keep.contains(&dir.to_path_buf()))
        });
    }
}

/// Read + parse a run's portable archive (`<run_dir>/run.lvr`), returning its
/// records, or `None` if the archive is missing or unreadable.
///
/// Materializes the whole journal. For anything that only walks the timeline
/// (the history API, journal search highlights), prefer [`visit_run_archive`]:
/// a mature run's journal is tens of MB, and parsing it whole per request was
/// the API's single largest transient allocation.
pub(crate) fn read_run_archive(run_id: &str) -> Option<Vec<leviath_core::run_archive::RunRecord>> {
    let path = run_dir(run_id).join(leviath_core::files::ARCHIVE_FILE);
    let bytes = std::fs::read(&path).ok()?;
    // Lenient, not strict: this reads an archive some other build may have
    // written, so a record kind added later must be stepped over rather than
    // rejecting the file (or, worse, truncating it silently).
    leviath_core::run_archive::read_archive_lenient(&mut bytes.as_slice())
        .ok()
        .map(|(_version, records)| records)
}

/// Stream a run's raw journal records through `visit`, one at a time, without
/// materializing the archive. Same lenient tail handling as
/// [`visit_run_archive`]. For consumers that inspect records rather than
/// replayed points (journal search).
pub(crate) fn visit_run_records(
    run_id: &str,
    visit: &mut dyn FnMut(&leviath_core::run_archive::RunRecord) -> std::ops::ControlFlow<()>,
) -> Option<()> {
    let path = run_dir(run_id).join(leviath_core::files::ARCHIVE_FILE);
    let file = std::fs::File::open(&path).ok()?;
    let mut reader = std::io::BufReader::with_capacity(64 * 1024, file);
    leviath_core::run_archive::read_archive_start(&mut reader).ok()?;
    // A frame this build cannot parse is skipped, not treated as the end: it
    // was written by a later version and the records after it are still ours.
    while let Ok(Some(frame)) = leviath_core::run_archive::read_frame(&mut reader) {
        let leviath_core::run_archive::Frame::Record(record) = frame else {
            continue;
        };
        if visit(&record).is_break() {
            break;
        }
    }
    Some(())
}

/// Stream a run's archive through a [`visit_points`] visitor without ever
/// materializing the journal: one buffered pass over `run.lvr`, one record and
/// one running window in memory. Returns `None` if the archive is missing or
/// its preamble is invalid; a torn tail (a live run mid-append) just ends the
/// walk with the points already visited.
///
/// [`visit_points`]: leviath_core::run_archive::visit_points
pub(crate) fn visit_run_archive(
    run_id: &str,
    visit: &mut dyn FnMut(leviath_core::run_archive::PointRef<'_>) -> std::ops::ControlFlow<()>,
) -> Option<()> {
    let path = run_dir(run_id).join(leviath_core::files::ARCHIVE_FILE);
    let file = std::fs::File::open(&path).ok()?;
    let mut reader = std::io::BufReader::with_capacity(64 * 1024, file);
    leviath_core::run_archive::visit_archive_points(&mut reader, visit).ok()
}

/// A run's context-window history: the full window (+ metadata) at each recorded
/// point over time, oldest first. Empty when there's no readable archive.
///
/// Every point's `meta` is [`RunMeta::redacted`]. The journal stores `RunMeta`
/// whole - including `callback_secret`, which the daemon needs to keep signing
/// webhooks for a run it reloads - so a replayed point carries the secret unless
/// it is stripped here. `GET /api/agents/{id}/context/history` serialized these
/// points directly, which handed the webhook signing key to any holder of the
/// API token: the same disclosure `redacted()` was introduced for on
/// `/api/agents`, re-opened through the archive.
///
/// Redacted in this shared reader rather than in that one handler so the next
/// consumer of a run's history inherits the fix instead of having to remember
/// it. No caller needs the secret: the CLI printer, the dashboard, and the API
/// all only display these points.
pub(crate) fn context_history(run_id: &str) -> Vec<leviath_core::run_archive::RunPoint> {
    read_run_archive(run_id)
        .map(|records| leviath_core::run_archive::replay_points(&records))
        .unwrap_or_default()
        .into_iter()
        .map(|point| leviath_core::run_archive::RunPoint {
            meta: point.meta.redacted(),
            ..point
        })
        .collect()
}

/// Inner implementation of `runs_dir`, parameterised so it can be tested
/// without touching the process-global env. All callers go through `runs_dir`.
///
/// The fallback resolves through [`crate::config::leviath_home_dir`], not
/// `dirs::home_dir` directly, so `LEVIATH_HOME` redirects the runs dir like it
/// redirects the config, the control socket and the agents dir. With the raw
/// OS home instead, a test that sets `LEVIATH_HOME` would be isolated
/// everywhere *except* here and still write runs into the developer's real
/// `~/.leviath/runs`. `LEVIATH_RUNS_DIR` wins over both.
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
/// `<runs_dir>/<invalid>`, a name that cannot exist - so a caller that passes an
/// attacker-supplied id gets a miss rather than a traversal. `run_id` reaches
/// this from URL segments on `GET /api/agents/{id}/logs` and friends, where
/// `Path::join` would otherwise happily accept `../../` or an absolute path.
///
/// Returning a definitely-missing path rather than an `Option` keeps every
/// caller's "no such run" branch as the single failure path, instead of adding a
/// second one that all of them would have to handle identically.
pub(crate) fn run_dir(run_id: &str) -> PathBuf {
    run_dir_in(&runs_dir(), run_id)
}

/// [`run_dir`] under an explicit runs directory, for a caller that was handed
/// one (the MCP server, whose tests isolate with a temp dir) rather than
/// resolving it from the environment. Same unsafe-id mapping.
pub(crate) fn run_dir_in(runs_dir: &std::path::Path, run_id: &str) -> PathBuf {
    if !leviath_core::is_safe_path_component(run_id) {
        tracing::warn!(run_id = %run_id, "rejected an unsafe run id");
        return runs_dir.join("<invalid>");
    }
    runs_dir.join(run_id)
}

/// How many random bits go in a run ID's suffix, rendered as 12 hex digits.
/// Collisions only matter within one wall-clock second for one agent name, so 48
/// bits is many orders of magnitude more than needed while staying short enough
/// to read in `lev ps` and the dashboard.
const RUN_ID_ENTROPY_BITS: u32 = 48;

/// Generate a unique run ID: `<agent_name>-<timestamp>-<random>`.
///
/// The suffix is **random**, not derived. A derived suffix like
/// `(now ^ (now >> 16) ^ counter)` over a process-local counter defends a
/// `lev run --count N` batch inside one process but degenerates to a pure
/// function of the current second across separate processes: three concurrent
/// `lev run` invocations all mint `fetcher-1785127214-8b48` and silently share
/// one run directory. Nothing downstream detects that - `create_dir_all` is a
/// no-op on an existing directory and the persistence worker then
/// last-writer-wins over `meta.json` / `context.json` / `run.lvr`, interleaving
/// two runs' state irrecoverably.
///
/// The `<name>-<secs>-<hex>` shape is preserved: the timestamp keeps IDs sorting
/// and reading chronologically, and the dashboard's short-ID display
/// (`split('-').next_back()`) still lands on the unique component.
///
/// The name is folded to **ASCII** alphanumerics, which is stricter than it
/// looks necessary. The id becomes a directory name, and [`run_dir`] resolves an
/// id that is not a safe path component to `<invalid>`. A Unicode fold let an
/// agent named `café` mint `café-...`: the daemon created that directory
/// happily, and then every CLI read of the run looked in `<invalid>` and found
/// nothing. The minter has to satisfy the rule the readers enforce.
pub(crate) fn new_run_id(agent_name: &str) -> String {
    use rand::RngExt as _;
    let entropy: u64 = rand::rng().random::<u64>() >> (u64::BITS - RUN_ID_ENTROPY_BITS);
    let safe_name = agent_name.replace(|c: char| !c.is_ascii_alphanumeric() && c != '-', "-");
    format!(
        "{}-{}-{:012x}",
        safe_name,
        leviath_core::duration::now_secs(),
        entropy
    )
}

/// Create the run directory and write initial metadata.
#[cfg(test)]
pub(crate) fn create_run(meta: &RunMeta) -> anyhow::Result<()> {
    create_run_in(&run_dir(&meta.run_id), meta)
}

/// Create an explicit run directory and write initial metadata into it.
///
/// Callers that already know the directory should prefer this over
/// [`create_run`], which resolves it from the home directory - the daemon's
/// spawner stakes out the run dir under its own configured `runs_dir`.
pub(crate) fn create_run_in(dir: &std::path::Path, meta: &RunMeta) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;

    // Restrict the run directory to owner-only (no-op on non-Unix).
    let _ = leviath_sys::secure_dir_perms(dir);

    write_meta_to(dir, meta)
}

/// Atomically write run metadata (write to tmp, then rename).
#[cfg(test)]
pub(crate) fn write_meta(meta: &RunMeta) -> anyhow::Result<()> {
    write_meta_to(&run_dir(&meta.run_id), meta)
}

/// Atomically write `meta.json` into an explicit run directory.
///
/// Callers that already know the directory should prefer this over
/// [`write_meta`], which resolves it from the home directory - the daemon's
/// recovery pass works from its configured `runs_dir` instead.
pub(crate) fn write_meta_to(dir: &std::path::Path, meta: &RunMeta) -> anyhow::Result<()> {
    let json =
        serde_json::to_string_pretty(meta).expect("infallible: RunMeta always serializes to JSON");
    write_private_atomic(&dir.join(leviath_core::files::META_FILE), &json)
}

/// Read run metadata for a given run ID.
pub(crate) fn read_meta(run_id: &str) -> anyhow::Result<RunMeta> {
    read_meta_from(&run_dir(run_id))
}

/// Read a run's final output, content included.
///
/// The descriptor in `meta.json` says whether there is one and how big it is;
/// this fetches the bytes from the sidecar beside it. Returns `None` when the
/// run produced no answer, or when the sidecar is missing (a run written by a
/// build that stored the answer inline, or one whose directory was pruned).
pub(crate) fn read_final_output(run_id: &str) -> Option<leviath_core::FinalOutput> {
    let meta = read_meta(run_id).ok()?;
    read_final_output_in(&run_dir(run_id), &meta)
}

/// [`read_final_output`] for a run directory the caller already resolved,
/// with the metadata it already read. The daemon's recovery works from its
/// configured runs directory rather than the home one, which is what this
/// entry point is for.
pub(crate) fn read_final_output_in(
    dir: &std::path::Path,
    meta: &RunMeta,
) -> Option<leviath_core::FinalOutput> {
    let descriptor = meta.final_output.clone()?;
    let content = std::fs::read_to_string(final_output_path(dir)).ok()?;
    Some(leviath_core::FinalOutput {
        content,
        format: descriptor.format,
        stage: descriptor.stage,
        submitted_at: descriptor.submitted_at,
        truncated: descriptor.truncated,
        artifacts: descriptor.artifacts,
    })
}

/// Where a run's answer lives, beside its `meta.json`.
pub(crate) fn final_output_path(dir: &std::path::Path) -> PathBuf {
    dir.join(leviath_core::FINAL_OUTPUT_FILE)
}

/// Write a run's answer to its sidecar, atomically.
///
/// Raw content with no wrapper: serving it is a read, and `lev result --raw` is
/// a copy. The descriptor in `meta.json` is what says it exists.
///
/// Test-only; see [`write_context_snapshot`].
#[cfg(test)]
pub(crate) fn write_final_output(dir: &std::path::Path, content: &str) -> anyhow::Result<()> {
    write_private_atomic(&final_output_path(dir), content)
}

/// Whether an on-disk run status means the run has finished and should be left
/// alone. `Starting`/`Running`/`WaitingInput` are all "still going" as far as
/// anything reading the runs dir is concerned.
pub(crate) fn is_terminal_status(status: &RunStatus) -> bool {
    matches!(
        status,
        RunStatus::Complete
            | RunStatus::CompleteInteractive
            | RunStatus::Error
            | RunStatus::Cancelled
    )
}

/// How long a run may claim to be live on disk, while the daemon is not holding
/// it, before anything treats it as abandoned.
///
/// Comfortably longer than the persistence heartbeat, so a live-but-slow run (a
/// long inference writes nothing else) is never mistaken for a dead one.
pub const STALE_AFTER_SECS: i64 = 300;

/// Whether a run that claims to be live on disk has nothing driving it: the
/// daemon is not holding it *and* it has not moved in [`STALE_AFTER_SECS`].
///
/// `live` is the set of run ids the daemon reports hosting, or `None` when it
/// gave no answer this poll. Both halves are needed and each is wrong on its
/// own. An unreachable daemon reports an empty set, so the id check alone would
/// condemn every healthy run the moment the daemon restarted. And a run parked
/// on a long inference legitimately does not move for minutes, so the clock
/// alone would condemn a run that is working. `None` therefore answers `false`
/// for everything: no answer is not evidence.
///
/// Ages against `last_progress_at`, falling back to `updated_at` for runs
/// written before that field existed. The fallback preserves the older, weaker
/// behavior for old runs rather than declaring them all stale at once.
///
/// One definition, shared by the dashboard's STALE badge and by `lev ps --all`,
/// so what an operator sees and what a harness reconciles against cannot drift.
pub(crate) fn looks_abandoned(
    meta: &RunMeta,
    live: Option<&std::collections::HashSet<String>>,
    now: i64,
) -> bool {
    let Some(live) = live else {
        return false; // no answer from the daemon; assume nothing
    };
    if is_terminal_status(&meta.status) || live.contains(&meta.run_id) {
        return false;
    }
    let moved_at = meta.last_progress_at.unwrap_or(meta.updated_at);
    now.saturating_sub(moved_at) > STALE_AFTER_SECS
}

/// Read run metadata out of an explicit run directory (the daemon works from its
/// own configured `runs_dir` rather than the home-resolved one).
pub(crate) fn read_meta_from(dir: &std::path::Path) -> anyhow::Result<RunMeta> {
    let path = dir.join(leviath_core::files::META_FILE);
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
            let meta_path = entry.path().join(leviath_core::files::META_FILE);
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
pub(crate) fn list_runs() -> Vec<RunMeta> {
    list_runs_in_dir(runs_dir())
}

/// [`list_runs`] under an explicit runs directory; see [`run_dir_in`] for why
/// a caller would hold one.
pub(crate) fn list_runs_in(runs_dir: &std::path::Path) -> Vec<RunMeta> {
    list_runs_in_dir(runs_dir.to_path_buf())
}

/// Every run below `root_id` in the sub-agent tree - its children, their
/// children, and so on - deepest first, `root_id` itself excluded.
///
/// This is the unit a delete has to act on. A sub-agent run is not a separate
/// thing a user started: it exists because its parent spawned it, it is drawn
/// nested under its parent, and once the parent's directory is gone nothing
/// left on disk explains why it is there. The dashboard treats a run whose
/// parent is absent as a root, so deleting a parent alone did not remove its
/// sub-agents - it *promoted* them to the top of the list.
///
/// Deepest first so a caller removing the family takes a child's directory
/// before its parent's. A partial failure then leaves a parent whose children
/// are gone, which reads as an ordinary finished run, rather than the orphan
/// this exists to prevent.
///
/// Two sources, unioned, because neither sees the whole tree on its own: the
/// scan over every run's `parent_run_id` misses a child whose `meta.json` will
/// not parse (`list_runs` skips it), and a parent's own `children` list misses
/// one spawned by a build that did not persist that field. An id named only by
/// `children` counts only if its directory is really there, so a pruned or
/// never-created child is not reported as something to delete.
///
/// Cycle-safe: no run is queued twice, so metadata claiming an ancestor as a
/// child ends the walk rather than looping forever.
pub(crate) fn descendant_run_ids(root_id: &str) -> Vec<String> {
    use std::collections::{HashMap, HashSet};

    let all = list_runs();
    let mut by_parent: HashMap<&str, Vec<&str>> = HashMap::new();
    for meta in &all {
        if let Some(parent) = meta.parent_run_id.as_deref() {
            by_parent.entry(parent).or_default().push(&meta.run_id);
        }
    }
    for meta in &all {
        let known = by_parent.entry(&meta.run_id).or_default();
        for child in &meta.children {
            if !known.contains(&child.as_str()) {
                known.push(child);
            }
        }
    }

    let mut seen: HashSet<&str> = HashSet::from([root_id]);
    let mut frontier: Vec<&str> = vec![root_id];
    let mut levels: Vec<Vec<String>> = Vec::new();
    while !frontier.is_empty() {
        let mut level = Vec::new();
        let mut next = Vec::new();
        for id in frontier {
            for child in by_parent.get(id).map(Vec::as_slice).unwrap_or_default() {
                if !seen.insert(child) || !run_dir(child).is_dir() {
                    continue;
                }
                level.push((*child).to_string());
                next.push(*child);
            }
        }
        levels.push(level);
        frontier = next;
    }
    levels.into_iter().rev().flatten().collect()
}

/// `root_id` and everything spawned beneath it, deepest first: the set that
/// "delete this run" acts on, wherever it is asked for.
///
/// A sub-agent run has no life of its own - it is drawn nested under its
/// parent and exists only because that parent spawned it - so forgetting the
/// parent has to forget the children too. The relationship is one-way: this
/// never reaches upwards, so deleting a child leaves its parent and its
/// siblings exactly where they were.
///
/// One definition for the API route and the dashboard, so the two cannot
/// disagree about what a delete covers. See [`descendant_run_ids`] for the
/// ordering and for how the tree is read off disk.
pub(crate) fn family_of(root_id: &str) -> Vec<String> {
    let mut ids = descendant_run_ids(root_id);
    ids.push(root_id.to_string());
    ids
}

/// [`list_runs`] through a [`StatCache`], for pollers: each `meta.json` is
/// re-parsed only when its stat changes, and cache entries for deleted runs
/// are dropped. Same ordering and skip-unreadable behavior as `list_runs`.
pub(crate) fn list_runs_cached(cache: &mut StatCache<RunMeta>) -> Vec<Arc<RunMeta>> {
    let dir = runs_dir();
    let mut runs = Vec::new();
    let mut live_dirs = std::collections::HashSet::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            live_dirs.insert(entry.path());
            let meta_path = entry.path().join(leviath_core::files::META_FILE);
            // A run this poller already knows to be finished is asked about
            // once a second; a live one (or one never seen) every time.
            let recheck = cache
                .peek(&meta_path)
                .map_or(std::time::Duration::ZERO, |meta| settle_window(&meta));
            if let Some(meta) = cache.get_with_recheck(
                &meta_path,
                |json| serde_json::from_str::<RunMeta>(json).ok(),
                recheck,
            ) {
                runs.push(meta);
            }
        }
    }
    cache.retain_under(&live_dirs);
    runs.sort_by_key(|r| std::cmp::Reverse(r.started_at));
    runs
}

/// How long a poller may go without re-stat'ing a run's files once the run
/// has finished: `ZERO` (every tick) while it is live, a second once it is
/// not. A finished run's record changes only when someone renames or deletes
/// it, and a second's lag on that is what buys a 750-run dashboard back two
/// thirds of its idle CPU.
pub(crate) fn settle_window(meta: &RunMeta) -> std::time::Duration {
    match meta.status {
        RunStatus::Complete
        | RunStatus::CompleteInteractive
        | RunStatus::Error
        | RunStatus::Cancelled => SETTLED_RECHECK,
        RunStatus::Starting | RunStatus::Running | RunStatus::WaitingInput | RunStatus::Paused => {
            std::time::Duration::ZERO
        }
    }
}

/// See [`settle_window`].
const SETTLED_RECHECK: std::time::Duration = std::time::Duration::from_secs(1);

/// [`read_stages_index`] through a [`StatCache`], for pollers.
#[cfg(test)]
pub(crate) fn read_stages_index_cached(
    run_id: &str,
    cache: &mut StatCache<Vec<StageRecord>>,
) -> Vec<StageRecord> {
    read_stages_index_settled(run_id, cache, std::time::Duration::ZERO)
}

/// [`read_stages_index_cached`] with the poller's [`settle_window`] for the
/// run, so a finished run's stage ledger is not stat'ed every tick either.
pub(crate) fn read_stages_index_settled(
    run_id: &str,
    cache: &mut StatCache<Vec<StageRecord>>,
    recheck_after: std::time::Duration,
) -> Vec<StageRecord> {
    let path = run_dir(run_id).join(leviath_core::files::STAGES_FILE);
    cache
        .get_with_recheck(&path, |json| serde_json::from_str(json).ok(), recheck_after)
        .map(|records| records.as_ref().clone())
        .unwrap_or_default()
}

/// [`read_context_snapshot`] through a [`StatCache`], for pollers. The
/// snapshot is shared, not cloned: a context window is the largest thing in a
/// run dir, and handing out copies per tick is the churn this cache removes.
pub(crate) fn read_context_snapshot_cached(
    run_id: &str,
    cache: &mut StatCache<ContextSnapshot>,
) -> Option<Arc<ContextSnapshot>> {
    let path = run_dir(run_id).join(leviath_core::files::CONTEXT_FILE);
    cache.get_with(&path, |json| serde_json::from_str(json).ok())
}

/// Read the last `max_bytes` of any file on disk, returning UTF-8 text.
/// If the file is smaller than `max_bytes` the whole file is returned.
/// Partial UTF-8 at the truncation boundary is handled by skipping to the
/// first newline.  Returns an empty string on any I/O error.
pub(crate) fn tail_file(path: &std::path::Path, max_bytes: u64) -> String {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return String::new(),
    };

    // Use fstat on the open fd rather than a separate stat() call - avoids the
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
pub(crate) fn stage_dir(run_id: &str, stage_idx: usize) -> PathBuf {
    run_dir(run_id).join("stages").join(stage_idx.to_string())
}

/// Atomically write the stages index for a run.
///
/// Test-only; see [`write_context_snapshot`].
#[cfg(test)]
pub(crate) fn write_stages_index(run_id: &str, stages: &[StageRecord]) -> anyhow::Result<()> {
    write_stages_index_to(&run_dir(run_id), stages)
}

#[cfg(test)]
fn write_stages_index_to(dir: &std::path::Path, stages: &[StageRecord]) -> anyhow::Result<()> {
    let json = serde_json::to_string_pretty(&stages)
        .expect("infallible: StageRecord slice always serializes to JSON");
    write_private_atomic(&dir.join(leviath_core::files::STAGES_FILE), &json)
}

/// Read the stages index for a run, or return an empty vec on any error.
pub(crate) fn read_stages_index(run_id: &str) -> Vec<StageRecord> {
    read_stages_index_from(&run_dir(run_id))
}

/// [`read_stages_index`] for a run directory the caller already holds.
///
/// Restart recovery works from its configured runs directory rather than the
/// home one, so it cannot resolve the path itself.
pub(crate) fn read_stages_index_from(dir: &std::path::Path) -> Vec<StageRecord> {
    let json = match std::fs::read_to_string(dir.join(leviath_core::files::STAGES_FILE)) {
        Ok(j) => j,
        Err(_) => return Vec::new(),
    };
    serde_json::from_str(&json).unwrap_or_default()
}

/// Ensure the per-stage directory exists (called before first write).
#[cfg(test)]
fn ensure_stage_dir(run_id: &str, stage_idx: usize) {
    let dir = stage_dir(run_id, stage_idx);
    let _ = leviath_sys::create_private_dir_all(&dir);
}

/// Append a line of readable agent output to the per-stage output log.
///
/// Test-only; see [`write_context_snapshot`].
#[cfg(test)]
pub(crate) fn append_stage_output(run_id: &str, stage_idx: usize, text: &str) {
    use std::io::Write;
    ensure_stage_dir(run_id, stage_idx);
    let path = stage_dir(run_id, stage_idx).join("output.log");
    if let Ok(mut file) = leviath_sys::open_private_append(&path) {
        let _ = writeln!(file, "{}", text);
    }
}

/// Append a line of operational/tool-activity log to the per-stage logs file.
///
/// Test-only; see [`write_context_snapshot`].
#[cfg(test)]
pub(crate) fn append_stage_log(run_id: &str, stage_idx: usize, text: &str) {
    use std::io::Write;
    ensure_stage_dir(run_id, stage_idx);
    let path = stage_dir(run_id, stage_idx).join("logs.log");
    if let Ok(mut file) = leviath_sys::open_private_append(&path) {
        let _ = writeln!(file, "{}", text);
    }
}

/// Atomically write a context snapshot for a specific stage.
///
/// Test-only; see [`write_context_snapshot`].
#[cfg(test)]
pub(crate) fn write_stage_context(
    run_id: &str,
    stage_idx: usize,
    snap: &ContextSnapshot,
) -> anyhow::Result<()> {
    ensure_stage_dir(run_id, stage_idx);
    write_context_snapshot_to(&stage_dir(run_id, stage_idx), snap)
}

/// Read the context snapshot for a specific stage, if present.
pub(crate) fn read_stage_context(run_id: &str, stage_idx: usize) -> Option<ContextSnapshot> {
    let path = stage_dir(run_id, stage_idx).join(leviath_core::files::CONTEXT_FILE);
    let json = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&json).ok()
}

/// Read the last `max_bytes` of the readable output log for a specific stage.
pub(crate) fn tail_stage_output(run_id: &str, stage_idx: usize, max_bytes: u64) -> String {
    tail_file(&stage_dir(run_id, stage_idx).join("output.log"), max_bytes)
}

/// Read the last `max_bytes` of the operational log for a specific stage.
pub(crate) fn tail_stage_log(run_id: &str, stage_idx: usize, max_bytes: u64) -> String {
    tail_file(&stage_dir(run_id, stage_idx).join("logs.log"), max_bytes)
}

/// Which stage's logs to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StageSelector {
    /// The stage the run is on now - the last entry in `stages.json`. What a
    /// caller tailing a live run wants, and what `agent_result` already picked.
    Current,
    /// One specific stage by index.
    Index(usize),
    /// Every stage, oldest first, with a separator between them.
    All,
}

/// Which of a stage's two logs to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LogStream {
    /// `output.log` - the assistant's readable output.
    Output,
    /// `logs.log` - operational lines: `[tool] …`, `[Tokens: …]`, `[error] …`.
    Operational,
}

/// Read a run's logs, choosing the stage and the stream.
///
/// Exists because there were two answers in the codebase to "where is a run's
/// output", and one of them was wrong: `GET /api/agents/{id}/logs` read
/// `<run_dir>/output.log`, which nothing has ever written, so it returned an
/// empty string for every run there has ever been. The real logs are per-stage,
/// under `stages/<idx>/`. Routing both that handler and `agent_result` through
/// here leaves one answer.
///
/// Stages come from `stages.json` rather than a `read_dir` of `stages/`, because
/// that index is the record of which stages exist and in what order - the
/// directory is just where their bytes landed.
///
/// `max_bytes` applies to what is returned, so for [`StageSelector::All`] it
/// bounds the joined text rather than each stage separately: "the last N bytes
/// of what you asked for" holds whatever the selector was.
pub(crate) fn tail_run_logs(
    run_id: &str,
    selector: StageSelector,
    stream: LogStream,
    max_bytes: u64,
) -> String {
    let read = |idx: usize| match stream {
        LogStream::Output => tail_stage_output(run_id, idx, max_bytes),
        LogStream::Operational => tail_stage_log(run_id, idx, max_bytes),
    };
    let stages = read_stages_index(run_id);
    match selector {
        StageSelector::Index(idx) => read(idx),
        StageSelector::Current => match stages.len().checked_sub(1) {
            Some(last) => read(last),
            // No stages recorded yet. Fall back to the legacy run-level file:
            // nothing writes it today, but a run whose stage dirs were pruned
            // still reads honestly instead of claiming it produced nothing.
            None => tail_file(&run_dir(run_id).join("output.log"), max_bytes),
        },
        StageSelector::All => {
            let joined = stages
                .iter()
                .map(|stage| {
                    format!(
                        "===== stage {}: {} =====\n{}",
                        stage.index,
                        stage.name,
                        read(stage.index)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            // Re-bound the join: each part was capped individually, so their
            // concatenation can exceed the cap the caller asked for.
            let start = leviath_core::text::floor_char_boundary(
                &joined,
                joined.len().saturating_sub(max_bytes as usize),
            );
            joined.split_at(start).1.to_string()
        }
    }
}

/// Build the isolated base directory for a run-state test and create its
/// `runs/` subdir. Returned as a [`tempfile::TempDir`] so the tree lives
/// exactly as long as the closure that uses it and is removed on drop even
/// when that closure panics.
///
/// `unique` (the test name, typically) is hashed into the directory prefix so
/// a stray tree in the temp dir can still be traced back to the test that
/// left it, without pushing the path length up by the 60+ characters a test
/// name runs to.
#[cfg(test)]
fn make_runs_base_dir(unique: &str) -> tempfile::TempDir {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    unique.hash(&mut hasher);
    let short = format!("{:x}", hasher.finish() & 0xffff_ffff);
    let base_dir = tempfile::Builder::new()
        .prefix(&format!("rs-{short}-"))
        .tempdir()
        .expect("create an isolated runs dir");
    let _ = std::fs::create_dir_all(base_dir.path().join("runs"));
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
    temp_env::with_vars(runs_dir_isolation_vars(base_dir.path()), || {
        f(base_dir.path())
    })
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
    temp_env::async_with_vars(
        runs_dir_isolation_vars(base_dir.path()),
        f(base_dir.path().to_path_buf()),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::fixtures;

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

    // ─── looks_abandoned ────────────────────────────────────────────────────

    /// A run claiming to be live on disk, last moved at 1000.
    fn live_on_disk(run_id: &str) -> RunMeta {
        let mut meta = RunMeta::new(
            run_id.to_string(),
            "coder".to_string(),
            "/agents/coder".to_string(),
            "t".to_string(),
            None,
            "/w".to_string(),
            1,
        );
        meta.status = RunStatus::Running;
        meta.updated_at = 1_000;
        meta.last_progress_at = Some(1_000);
        meta
    }

    fn held(ids: &[&str]) -> std::collections::HashSet<String> {
        ids.iter().map(|s| (*s).to_string()).collect()
    }

    /// The abandoned shape: disk says running, the daemon is not hosting it,
    /// and it has not moved in a long time.
    #[test]
    fn a_run_nothing_is_driving_looks_abandoned() {
        let meta = live_on_disk("r1");
        assert!(looks_abandoned(
            &meta,
            Some(&held(&["other"])),
            1_000 + STALE_AFTER_SECS + 1
        ));
    }

    /// The arm that decides whether a reconciler is safe to run at all. A daemon
    /// that is restarting gives no answer, which looks exactly like every run
    /// dying at once; anything that acted on it would cancel a whole factory.
    #[test]
    fn no_answer_from_the_daemon_condemns_nothing() {
        let meta = live_on_disk("r1");
        assert!(!looks_abandoned(
            &meta,
            None,
            1_000 + STALE_AFTER_SECS * 100
        ));
    }

    #[test]
    fn a_run_the_daemon_is_hosting_is_never_abandoned() {
        let meta = live_on_disk("r1");
        assert!(!looks_abandoned(
            &meta,
            Some(&held(&["r1"])),
            1_000 + STALE_AFTER_SECS * 100
        ));
    }

    /// A run parked on a long inference has not moved and is still working, so
    /// the window has to be wider than the persistence heartbeat.
    #[test]
    fn a_slow_run_inside_the_window_is_left_alone() {
        let meta = live_on_disk("r1");
        assert!(!looks_abandoned(
            &meta,
            Some(&held(&[])),
            1_000 + STALE_AFTER_SECS - 1
        ));
    }

    /// A finished run is not abandoned, it is done. The daemon unloads it within
    /// seconds of it going terminal, so it is absent from the live set for the
    /// rest of time and would otherwise trip every other check here.
    #[test]
    fn a_finished_run_is_not_abandoned() {
        for status in [
            RunStatus::Complete,
            RunStatus::CompleteInteractive,
            RunStatus::Error,
            RunStatus::Cancelled,
        ] {
            let mut meta = live_on_disk("r1");
            meta.status = status.clone();
            assert!(
                !looks_abandoned(&meta, Some(&held(&[])), 1_000 + STALE_AFTER_SECS * 100),
                "{status} is finished, not abandoned"
            );
        }
    }

    /// The progress stamp wins over the heartbeat. A wedged run keeps rewriting
    /// `updated_at` every 30 seconds, so judging on it would never age anything
    /// out; the stamp is the only field on meta.json that separates a run that
    /// is working from one that is only ticking.
    #[test]
    fn a_fresh_heartbeat_does_not_rescue_a_run_that_stopped_moving() {
        let mut meta = live_on_disk("r1");
        let now = 1_000 + STALE_AFTER_SECS * 10;
        meta.updated_at = now; // the heartbeat, still beating
        meta.last_progress_at = Some(1_000); // but nothing has moved since 1000
        assert!(looks_abandoned(&meta, Some(&held(&[])), now));
    }

    /// A run written before the stamp existed falls back to `updated_at`, so old
    /// runs keep the older, weaker behavior instead of all reading as stale.
    #[test]
    fn a_run_without_the_stamp_falls_back_to_updated_at() {
        let mut meta = live_on_disk("r1");
        meta.last_progress_at = None;
        meta.updated_at = 1_000;
        assert!(looks_abandoned(
            &meta,
            Some(&held(&[])),
            1_000 + STALE_AFTER_SECS + 1
        ));
        meta.updated_at = 1_000 + STALE_AFTER_SECS;
        assert!(!looks_abandoned(
            &meta,
            Some(&held(&[])),
            1_000 + STALE_AFTER_SECS + 1
        ));
    }

    #[test]
    fn write_json_atomic_fs_write_failure() {
        // Drive the `std::fs::write(&tmp, json)?` error arm: writing the
        // `.json.tmp` sibling into a directory that does not exist fails.
        let path = std::path::Path::new("/nonexistent/leviath/runstate-cov/out.json");
        let result = write_private_atomic(path, "{}");
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
            RunStatus::Paused,
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
        assert_eq!(RunStatus::Paused.to_string(), "Paused");
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
        let mut meta = fixtures::run_meta("r");
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
        // A run written before the progress stamp existed has no answer, which is
        // why the field is an Option: `Some(0)` would read as "last moved in 1970"
        // and invite a reconciler to declare it abandoned.
        assert!(meta.last_progress_at.is_none());
    }

    /// `pid` is written by every daemon there has ever been, and is always 0 in
    /// the shared world. A file that omits it entirely must still load, so the
    /// field can be dropped in a future major without stranding old runs.
    #[test]
    fn run_meta_without_a_pid_still_loads() {
        let json = serde_json::json!({
            "run_id": "r1",
            "agent_name": "a",
            "agent_path": "/p",
            "task": "t",
            "model": null,
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
        assert_eq!(meta.pid, 0);
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
                reasoning: None,
            }],
            description: None,
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
            description: None,
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
                description: None,
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

    // ─── read_final_output ──────────────────────────────────────────────────

    /// The descriptor in `meta.json` and the sidecar beside it have to agree.
    /// Each way they can disagree reads as "no answer", which is the only safe
    /// reading: half an answer is worse than none.
    #[test]
    fn read_final_output_needs_both_the_descriptor_and_the_sidecar() {
        with_isolated_runs_dir("read-final-output", |_| {
            // No run at all.
            assert!(read_final_output("no-such-run").is_none());

            // A run with no answer recorded.
            let meta = fixtures::run_meta("run-silent");
            create_run(&meta).expect("run dir");
            assert!(read_final_output("run-silent").is_none());

            // A descriptor saying there is one, with the sidecar missing: a run
            // written by a build that stored the answer inline, or one whose
            // directory was pruned.
            let answer = leviath_core::output::FinalOutput::new(
                "the answer",
                Some("markdown".to_string()),
                "present".to_string(),
                42,
            );
            let mut claimed = fixtures::run_meta("run-claimed");
            claimed.final_output = Some(answer.descriptor());
            create_run(&claimed).expect("run dir");
            assert!(read_final_output("run-claimed").is_none());

            // And both together: the answer comes back whole.
            write_final_output(&run_dir("run-claimed"), &answer.content).expect("sidecar");
            let read = read_final_output("run-claimed").expect("both halves are there");
            assert_eq!(read.content, "the answer");
            assert_eq!(read.format.as_deref(), Some("markdown"));
            assert_eq!(read.stage, "present");
        });
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

    /// The id becomes a directory name, and every reader resolves it through
    /// `is_safe_path_component`. A minted id that fails that check spawns a run
    /// the CLI can never read back, so the two rules have to agree whatever the
    /// blueprint calls itself.
    #[test]
    fn every_minted_run_id_is_a_safe_path_component() {
        for name in [
            "café",
            "日本語",
            "agent with spaces!",
            "../escape",
            "a/b",
            "..",
            "",
            "emoji-🚀-agent",
            "Ünïcödé",
        ] {
            let id = new_run_id(name);
            assert!(
                leviath_core::is_safe_path_component(&id),
                "agent {name:?} minted {id:?}, which run_dir resolves to <invalid>"
            );
        }
    }

    #[test]
    fn new_run_id_is_unique_across_rapid_calls_in_same_second() {
        // `--count N` calls `new_run_id` N times in a tight loop, all within the
        // same wall-clock second.
        let ids: std::collections::HashSet<String> =
            (0..100).map(|_| new_run_id("same-agent")).collect();
        assert_eq!(ids.len(), 100);
    }

    /// Split `<name>-<secs>-<hex>` from the right - the agent name itself may
    /// contain dashes.
    fn split_run_id(id: &str) -> (&str, &str) {
        let mut parts = id.rsplitn(3, '-');
        let suffix = parts.next().expect("run id has a suffix");
        let secs = parts.next().expect("run id has a timestamp");
        (secs, suffix)
    }

    #[test]
    fn new_run_id_suffix_is_random_not_derived_from_the_clock() {
        // The collision this guards against is *across processes*: a suffix
        // derived as `(now ^ (now >> 16) ^ counter)` over a process-local
        // counter that every new process starts at 0 degenerates to a pure
        // function of the current second. Three concurrent `lev run`
        // invocations all mint `fetcher-1785127214-8b48` and silently share
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
        // seconds - without this the assertion above would be vacuous.
        assert!(
            largest > 1,
            "expected IDs sharing a second, got {by_second:?}"
        );
    }

    // ─── write_meta / read_meta roundtrip ───────────────────────────────────

    #[test]
    fn write_and_read_meta_roundtrip() {
        // Isolated via `isolate_runs_dir_for_test` so write_meta/read_meta
        // never touch the real ~/.leviath/runs/ - the temp dir is removed
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
            let meta = fixtures::run_meta(run_id);
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

            // The streaming visitors see the same journal without ever
            // materializing it.
            let mut streamed_points = Vec::new();
            visit_run_archive(run_id, &mut |p| {
                streamed_points.push((p.index, p.context.stage_name.to_string()));
                std::ops::ControlFlow::Continue(())
            })
            .expect("streamed replay");
            assert_eq!(streamed_points, vec![(0, "plan".to_string())]);

            let mut streamed_records = 0usize;
            visit_run_records(run_id, &mut |_| {
                streamed_records += 1;
                std::ops::ControlFlow::Continue(())
            })
            .expect("streamed records");
            assert_eq!(streamed_records, 2);

            // And a visitor can stop early.
            let mut first_only = 0usize;
            visit_run_records(run_id, &mut |_| {
                first_only += 1;
                std::ops::ControlFlow::Break(())
            })
            .expect("streamed records with break");
            assert_eq!(first_only, 1);
        });
    }

    /// The stat cache's contract: parse once, serve from cache while the stat
    /// is unchanged, re-parse on change, cache negative results, and forget
    /// files that disappear.
    #[test]
    fn stat_cache_parses_once_per_stat_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("value.json");
        std::fs::write(&path, "41").unwrap();
        let mut cache: StatCache<i64> = StatCache::default();
        let mut parses = 0;
        let get = |cache: &mut StatCache<i64>, path: &std::path::Path, parses: &mut usize| {
            cache
                .get_with(path, |text| {
                    *parses += 1;
                    text.trim().parse().ok()
                })
                .map(|v| *v)
        };

        assert_eq!(get(&mut cache, &path, &mut parses), Some(41));
        assert_eq!(get(&mut cache, &path, &mut parses), Some(41));
        assert_eq!(parses, 1, "the second read came from the cache");

        // A same-length rewrite with a fresh mtime re-parses (the atomic-rename
        // writer always produces a new inode+mtime; simulate with a bumped
        // mtime via a rewrite of different content and length).
        std::fs::write(&path, "1234").unwrap();
        assert_eq!(get(&mut cache, &path, &mut parses), Some(1234));
        assert_eq!(parses, 2);

        // Unparseable content is cached as a miss - one parse attempt, then
        // stat-only until the file changes again.
        std::fs::write(&path, "not a number").unwrap();
        assert_eq!(get(&mut cache, &path, &mut parses), None);
        assert_eq!(get(&mut cache, &path, &mut parses), None);
        assert_eq!(parses, 3, "the bad file was parsed once, not per tick");

        // A deleted file is a miss and its entry is dropped.
        std::fs::remove_file(&path).unwrap();
        assert_eq!(get(&mut cache, &path, &mut parses), None);
        assert_eq!(parses, 3);
    }

    #[test]
    fn stat_cache_retain_under_drops_dead_runs() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("live");
        let dead = dir.path().join("dead");
        std::fs::create_dir_all(&live).unwrap();
        std::fs::create_dir_all(&dead).unwrap();
        std::fs::write(live.join("meta.json"), "1").unwrap();
        std::fs::write(dead.join("meta.json"), "2").unwrap();
        let mut cache: StatCache<i64> = StatCache::default();
        cache.get_with(&live.join("meta.json"), |t| t.trim().parse().ok());
        cache.get_with(&dead.join("meta.json"), |t| t.trim().parse().ok());
        assert_eq!(cache.entries.len(), 2);

        let keep: std::collections::HashSet<PathBuf> = [live.clone()].into_iter().collect();
        cache.retain_under(&keep);
        assert_eq!(cache.entries.len(), 1);
        assert!(cache.entries.contains_key(&live.join("meta.json")));
    }

    /// The cached listing and per-run readers agree with their uncached
    /// counterparts, and serve repeat calls without re-parsing.
    #[test]
    fn cached_run_readers_match_the_uncached_ones() {
        with_isolated_runs_dir("cached-run-readers", |_d| {
            let meta = RunMeta::new(
                "cached-run".to_string(),
                "agent".to_string(),
                "/p".to_string(),
                "t".to_string(),
                None,
                "/w".to_string(),
                2,
            );
            create_run(&meta).unwrap();
            write_stages_index(
                "cached-run",
                &[leviath_core::run_meta::StageRecord::new(
                    "plan".to_string(),
                    0,
                )],
            )
            .unwrap();
            write_context_snapshot(
                "cached-run",
                &ContextSnapshot {
                    stage_name: "plan".to_string(),
                    total_tokens: 3,
                    max_tokens: 100,
                    regions: vec![],
                },
            )
            .unwrap();

            let mut metas = StatCache::default();
            let mut stages = StatCache::default();
            let mut contexts = StatCache::default();

            let listed = list_runs_cached(&mut metas);
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].run_id, list_runs()[0].run_id);

            let cached_stages = read_stages_index_cached("cached-run", &mut stages);
            let plain_stages = read_stages_index("cached-run");
            assert_eq!(cached_stages.len(), plain_stages.len());
            assert_eq!(cached_stages[0].name, plain_stages[0].name);
            let cached_ctx =
                read_context_snapshot_cached("cached-run", &mut contexts).expect("snapshot cached");
            assert_eq!(
                *cached_ctx,
                read_context_snapshot("cached-run").expect("snapshot read")
            );
            // A repeat serves the SAME Arc - the whole point of the cache.
            let again = read_context_snapshot_cached("cached-run", &mut contexts).unwrap();
            assert!(Arc::ptr_eq(&cached_ctx, &again));

            // A second run makes the listing's ordering real: newest first,
            // same as the uncached listing.
            let mut second = RunMeta::new(
                "cached-run-2".to_string(),
                "agent".to_string(),
                "/p".to_string(),
                "t".to_string(),
                None,
                "/w".to_string(),
                1,
            );
            second.started_at += 100;
            create_run(&second).unwrap();
            let listed = list_runs_cached(&mut metas);
            assert_eq!(listed.len(), 2);
            assert_eq!(listed[0].run_id, "cached-run-2", "newest first");

            // A run dir with a garbled meta.json is skipped, not fatal - and
            // skipped cheaply on every later tick (the negative result is
            // cached until the file changes).
            std::fs::create_dir_all(run_dir("garbled-run")).unwrap();
            std::fs::write(run_dir("garbled-run").join("meta.json"), "not json {{").unwrap();
            assert_eq!(list_runs_cached(&mut metas).len(), 2);

            // A run whose dir disappears falls out of the cached listing.
            std::fs::remove_dir_all(run_dir("garbled-run")).unwrap();
            std::fs::remove_dir_all(run_dir("cached-run")).unwrap();
            std::fs::remove_dir_all(run_dir("cached-run-2")).unwrap();
            assert!(list_runs_cached(&mut metas).is_empty());
            assert!(read_stages_index_cached("cached-run", &mut stages).is_empty());
            assert!(read_context_snapshot_cached("cached-run", &mut contexts).is_none());

            // And a missing runs DIRECTORY altogether lists nothing (the
            // read_dir-failed arm).
            std::fs::remove_dir_all(runs_dir()).unwrap();
            assert!(list_runs_cached(&mut metas).is_empty());
        });
    }

    /// A settled entry is answered from memory inside its window and from the
    /// filesystem outside it; `peek` never asks the filesystem at all.
    #[test]
    fn a_stat_cache_honours_the_recheck_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meta.json");
        std::fs::write(&path, "1").unwrap();
        let mut cache: StatCache<String> = StatCache::default();
        let parse = |s: &str| Some(s.to_string());
        assert!(cache.peek(&path).is_none(), "nothing cached yet");
        assert_eq!(*cache.get_with(&path, parse).unwrap(), "1");
        assert_eq!(*cache.peek(&path).unwrap(), "1");

        // The file changes. Inside the window the old value stands, because
        // the point is not to stat; outside it the change is seen. The new
        // content is a different LENGTH: the stamp is (mtime, len), and a
        // same-length rewrite inside one mtime tick (Windows CI has coarse
        // ticks) is invisible to it by design.
        std::fs::write(&path, "22").unwrap();
        let hour = std::time::Duration::from_secs(3600);
        assert_eq!(*cache.get_with_recheck(&path, parse, hour).unwrap(), "1");
        assert_eq!(*cache.get_with(&path, parse).unwrap(), "22");
        // A stat that finds the same stamp refreshes the check time without a
        // parse, so the next windowed read is answered from memory too.
        assert_eq!(*cache.get_with(&path, parse).unwrap(), "22");
        assert_eq!(*cache.get_with_recheck(&path, parse, hour).unwrap(), "22");

        // A missing file is forgotten, and a window does not resurrect it.
        std::fs::remove_file(&path).unwrap();
        assert!(cache.get_with(&path, parse).is_none());
        assert!(cache.peek(&path).is_none());
        assert!(cache.get_with_recheck(&path, parse, hour).is_none());
    }

    /// A finished run settles; a live, waiting or paused one does not.
    #[test]
    fn only_a_finished_run_settles() {
        let mut meta = RunMeta::new(
            "settle".to_string(),
            "agent".to_string(),
            "/p".to_string(),
            "t".to_string(),
            None,
            "/w".to_string(),
            1,
        );
        for status in [
            RunStatus::Starting,
            RunStatus::Running,
            RunStatus::WaitingInput,
            RunStatus::Paused,
        ] {
            meta.status = status;
            assert_eq!(settle_window(&meta), std::time::Duration::ZERO);
        }
        for status in [
            RunStatus::Complete,
            RunStatus::CompleteInteractive,
            RunStatus::Error,
            RunStatus::Cancelled,
        ] {
            meta.status = status;
            assert_eq!(settle_window(&meta), SETTLED_RECHECK);
        }
    }

    /// The listing asks a finished run once a second and a live one every
    /// time: a rename of a finished run shows up within the window, a live
    /// run's progress immediately.
    #[test]
    fn a_cached_listing_settles_finished_runs() {
        with_isolated_runs_dir("cached-listing-settles", |_d| {
            let mut done = RunMeta::new(
                "done".to_string(),
                "agent".to_string(),
                "/p".to_string(),
                "t".to_string(),
                None,
                "/w".to_string(),
                1,
            );
            done.status = RunStatus::Complete;
            create_run(&done).unwrap();
            let mut live = RunMeta::new(
                "live".to_string(),
                "agent".to_string(),
                "/p".to_string(),
                "t".to_string(),
                None,
                "/w".to_string(),
                1,
            );
            live.status = RunStatus::Running;
            create_run(&live).unwrap();
            let mut metas = StatCache::default();
            let mut stages = StatCache::default();
            assert_eq!(list_runs_cached(&mut metas).len(), 2);

            // Both records change on disk.
            done.title = Some("renamed".to_string());
            write_meta(&done).unwrap();
            live.iteration = 7;
            write_meta(&live).unwrap();
            let listed = list_runs_cached(&mut metas);
            let by_id = |id: &str| listed.iter().find(|m| m.run_id == id).unwrap().clone();
            assert_eq!(by_id("live").iteration, 7, "a live run is read every tick");
            assert_eq!(
                by_id("done").title,
                None,
                "a finished run waits for its window"
            );
            // The same for the stage ledger: the first read populates, a
            // windowed read after a change answers from memory, an unwindowed
            // one sees the change.
            write_stages_index("done", &[StageRecord::new("a".to_string(), 0)]).unwrap();
            let window = settle_window(&done);
            assert_eq!(
                read_stages_index_settled("done", &mut stages, window).len(),
                1
            );
            write_stages_index(
                "done",
                &[
                    StageRecord::new("a".to_string(), 0),
                    StageRecord::new("b".to_string(), 1),
                ],
            )
            .unwrap();
            assert_eq!(
                read_stages_index_settled("done", &mut stages, window).len(),
                1
            );
            assert_eq!(read_stages_index_cached("done", &mut stages).len(), 2);
            // Outside the window (forced here by asking with no window) the
            // rename is seen.
            let fresh = metas
                .get_with(&run_dir("done").join("meta.json"), |json| {
                    serde_json::from_str::<RunMeta>(json).ok()
                })
                .unwrap();
            assert_eq!(fresh.title.as_deref(), Some("renamed"));
        });
    }

    #[test]
    fn streaming_visitors_return_none_when_the_archive_is_missing() {
        with_isolated_runs_dir("streaming-visitors-missing", |_d| {
            // One visitor closure of each kind, shared across every call in
            // this test - the last pair of calls (on a real archive) executes
            // them, so a missing/invalid archive is proven by the counters
            // staying put, not by never-run closures.
            let points_seen = std::cell::Cell::new(0usize);
            let mut on_point = |_: leviath_core::run_archive::PointRef<'_>| {
                points_seen.set(points_seen.get() + 1);
                std::ops::ControlFlow::Continue(())
            };
            let records_seen = std::cell::Cell::new(0usize);
            let mut on_record = |_: &leviath_core::run_archive::RunRecord| {
                records_seen.set(records_seen.get() + 1);
                std::ops::ControlFlow::Continue(())
            };

            assert!(visit_run_archive("no-such-run", &mut on_point).is_none());
            assert!(visit_run_records("no-such-run", &mut on_record).is_none());
            // A file that is not an archive fails the preamble check.
            let run_id = "bad-preamble";
            std::fs::create_dir_all(run_dir(run_id)).unwrap();
            std::fs::write(run_dir(run_id).join("run.lvr"), b"junk").unwrap();
            assert!(visit_run_archive(run_id, &mut on_point).is_none());
            assert!(visit_run_records(run_id, &mut on_record).is_none());
            assert_eq!((points_seen.get(), records_seen.get()), (0, 0));

            // The same closures over a real archive do run.
            let real = "streaming-visitors-real";
            std::fs::create_dir_all(run_dir(real)).unwrap();
            write_minimal_archive(real);
            assert!(visit_run_archive(real, &mut on_point).is_some());
            assert!(visit_run_records(real, &mut on_record).is_some());
            assert_eq!(points_seen.get(), 1);
            assert_eq!(records_seen.get(), 2);
        });
    }

    /// A record kind written by a later build is stepped over, so the records
    /// after it still reach the caller.
    ///
    /// This is the CLI half of the guarantee. The reader here has its own loop
    /// over frames, separate from the ones in `leviath-core`, so "every reader
    /// skips" is a claim that has to be checked per reader rather than assumed
    /// from the primitive being right.
    #[test]
    fn streaming_records_steps_over_a_record_kind_from_a_later_build() {
        with_isolated_runs_dir("visit-records-unknown-kind", |_dir| {
            use leviath_core::run_archive::{self, RunRecord};

            let run_id = "unknown-kind";
            std::fs::create_dir_all(run_dir(run_id)).unwrap();
            write_minimal_archive(run_id);

            // Append a well-framed record this build has no variant for, then
            // one it does.
            let mut extra = Vec::new();
            let payload =
                serde_json::to_vec(&serde_json::json!({ "FromTheFuture": { "x": 1 } })).unwrap();
            extra.extend_from_slice(&(payload.len() as u64).to_be_bytes());
            extra.extend_from_slice(&payload);
            run_archive::write_record(
                &mut extra,
                &RunRecord::Message {
                    message: leviath_core::run_archive::MessageRecord {
                        role: "user".to_string(),
                        content: "after the gap".to_string(),
                    },
                    at: 2,
                },
            )
            .unwrap();
            let path = run_dir(run_id).join("run.lvr");
            let mut bytes = std::fs::read(&path).unwrap();
            bytes.extend_from_slice(&extra);
            std::fs::write(&path, bytes).unwrap();

            let seen = std::cell::RefCell::new(Vec::new());
            let visited = visit_run_records(run_id, &mut |record| {
                if let RunRecord::Message { message, .. } = record {
                    seen.borrow_mut().push(message.content.clone());
                }
                std::ops::ControlFlow::Continue(())
            });
            assert!(visited.is_some());
            assert_eq!(
                seen.into_inner(),
                vec!["after the gap".to_string()],
                "the readable record past the unknown one still arrives"
            );
        });
    }

    /// Write a two-record archive (Header + one ContextCheckpoint) for `run_id`.
    fn write_minimal_archive(run_id: &str) {
        use leviath_core::run_archive::{self, RunIdentity, RunRecord};
        let mut buf = Vec::new();
        run_archive::write_archive_start(&mut buf, run_archive::RUN_ARCHIVE_VERSION).unwrap();
        let meta = fixtures::run_meta(run_id);
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
    }

    /// The journal keeps `callback_secret` (the daemon re-signs webhooks for a
    /// run it reloads), so a replayed point carries it unless the reader strips
    /// it. `GET /api/agents/{id}/context/history` serves these points straight
    /// out, so an unstripped one hands the webhook signing key to any API token
    /// holder.
    ///
    /// Asserts against the *archive* as well as the history, so the test still
    /// means something if the journal ever stops storing the secret: were that
    /// to happen, the first assertion fails rather than the second silently
    /// passing on a field that is no longer there to leak.
    #[test]
    fn context_history_redacts_the_webhook_secret_the_journal_keeps() {
        with_isolated_runs_dir("context-history-redacts-secret", |_d| {
            use leviath_core::run_archive::{self, RunIdentity, RunRecord};
            let run_id = "archive-secret-unit";
            std::fs::create_dir_all(run_dir(run_id)).unwrap();
            let mut buf = Vec::new();
            run_archive::write_archive_start(&mut buf, run_archive::RUN_ARCHIVE_VERSION).unwrap();
            let mut meta = fixtures::run_meta(run_id);
            meta.callback_url = Some("https://example.invalid/hook".to_string());
            meta.callback_secret = Some("super-secret-signing-key".to_string());
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

            // The secret really is on disk, so redaction has work to do. Read
            // the raw bytes rather than matching over parsed records: a match
            // that stops at the Header leaves its other arm unreachable, and
            // this says the thing that actually matters anyway.
            let raw = std::fs::read(run_dir(run_id).join("run.lvr")).unwrap();
            assert!(String::from_utf8_lossy(&raw).contains("super-secret-signing-key"));

            // What the reader hands out has it stripped, and keeps the rest.
            let history = context_history(run_id);
            assert_eq!(history.len(), 1);
            assert_eq!(history[0].meta.callback_secret, None);
            assert_eq!(
                history[0].meta.callback_url.as_deref(),
                Some("https://example.invalid/hook")
            );
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
        // opening it for append fails with `IsADirectory` - the function
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
        // fallback branch, so - like `runs_dir_structure` below - it forces
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
    /// `LEVIATH_HOME` like every other data path. Resolving through the raw
    /// OS home would leave a fully isolated test session still appending to
    /// the developer's real `~/.leviath/dashboard.log`.
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
        // See the comment on `dashboard_log_path_structure` above - same
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

    /// With no `LEVIATH_RUNS_DIR`, the runs dir must follow `LEVIATH_HOME` - the
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
        // afterward - a property no other test can perturb, since none
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
        // on *every* OS the tail slice is guaranteed newline-free - on Windows
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
                            reasoning: None,
                        },
                        RegionEntrySnapshot {
                            content: "Additional instruction".into(),
                            tokens: 5,
                            kind: Default::default(),
                            metadata: Some(serde_json::json!({"source": "user"})),
                            key: None,
                            taint: Default::default(),
                            reasoning: None,
                        },
                    ],
                    description: None,
                },
                RegionSnapshot {
                    name: "conversation".into(),
                    kind: "sliding".into(),
                    current_tokens: 900,
                    max_tokens: 6000,
                    entries: vec![],
                    description: None,
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
            reasoning: None,
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
        // this exact bogus id") - can assert real emptiness instead of just
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
        // "Is a directory" - and that error is deliberately discarded (`let
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
        // to stat a path, not read permission on the file itself - so both
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
            ..fixtures::run_meta(run_id)
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
            assert_eq!(force_cancel_in(&dir, 99), ForceCancelOutcome::Terminated);
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
    /// permanent - the one state from which there is no way back.
    #[test]
    fn force_cancel_writes_a_record_over_unreadable_metadata() {
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("corrupt-run");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("meta.json"), "{ not json").unwrap();

        assert_eq!(force_cancel_in(&dir, 99), ForceCancelOutcome::Terminated);
        let meta = read_meta_from(&dir).expect("now parses");
        assert_eq!(meta.status, RunStatus::Cancelled);
        assert_eq!(meta.run_id, "corrupt-run", "recovered from the dir name");
        assert!(meta.error.is_some(), "records why it was synthesized");
    }

    /// A directory that exists but can't be written still counts as "found" - the
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

    /// The spawn that never became a run: the placeholder is `Starting`, which
    /// is not terminal, so it has to be rewritten or it claims to be alive for
    /// ever.
    #[test]
    fn force_error_records_the_failure_over_a_starting_placeholder() {
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("stillborn-run");
        let meta = RunMeta::new(
            "stillborn-run".to_string(),
            "agent".to_string(),
            "/no/such/agent.leviath".to_string(),
            "t".to_string(),
            None,
            "/tmp".to_string(),
            0,
        );
        create_run_in(&dir, &meta).unwrap();
        assert_eq!(read_meta_from(&dir).unwrap().status, RunStatus::Starting);

        assert_eq!(
            force_error_in(&dir, "blueprint not found", 99),
            ForceCancelOutcome::Terminated
        );

        let written = read_meta_from(&dir).unwrap();
        assert_eq!(written.status, RunStatus::Error);
        assert_eq!(written.error.as_deref(), Some("blueprint not found"));
        assert_eq!(written.updated_at, 99);
        // The rest of the placeholder survives, so the run still explains itself.
        assert_eq!(written.task, "t");
    }

    #[test]
    fn force_error_leaves_a_run_that_already_finished_alone() {
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("done-run");
        let mut meta = RunMeta::new(
            "done-run".to_string(),
            "agent".to_string(),
            String::new(),
            "t".to_string(),
            None,
            "/tmp".to_string(),
            0,
        );
        meta.status = RunStatus::Complete;
        create_run_in(&dir, &meta).unwrap();

        assert_eq!(
            force_error_in(&dir, "too late", 99),
            ForceCancelOutcome::AlreadyTerminal
        );
        assert_eq!(read_meta_from(&dir).unwrap().status, RunStatus::Complete);
    }

    #[test]
    fn force_cancel_keeps_an_error_the_run_had_already_recorded() {
        // Cancelling passes no message of its own, so whatever the run managed
        // to say about itself before it was killed must survive.
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("noisy-run");
        let mut meta = RunMeta::new(
            "noisy-run".to_string(),
            "agent".to_string(),
            String::new(),
            "t".to_string(),
            None,
            "/tmp".to_string(),
            0,
        );
        meta.error = Some("a provider hiccup".to_string());
        create_run_in(&dir, &meta).unwrap();

        assert_eq!(force_cancel_in(&dir, 99), ForceCancelOutcome::Terminated);
        let written = read_meta_from(&dir).unwrap();
        assert_eq!(written.status, RunStatus::Cancelled);
        assert_eq!(written.error.as_deref(), Some("a provider hiccup"));
    }

    #[test]
    fn force_error_writes_its_message_over_unreadable_metadata() {
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("corrupt-stillborn");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("meta.json"), "{ not json").unwrap();

        assert_eq!(
            force_error_in(&dir, "blueprint not found", 99),
            ForceCancelOutcome::Terminated
        );
        let written = read_meta_from(&dir).expect("now parses");
        assert_eq!(written.status, RunStatus::Error);
        assert_eq!(written.error.as_deref(), Some("blueprint not found"));
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

    // ─── descendant_run_ids / family_of ────────────────────────────────────

    /// Plant a run directory whose meta names `parent` as its parent and
    /// `children` as its children.
    fn plant_run(id: &str, parent: Option<&str>, children: &[&str]) {
        let mut meta = RunMeta::new(
            id.to_string(),
            "agent".into(),
            "/p".into(),
            "task".into(),
            None,
            "/w".into(),
            1,
        );
        meta.parent_run_id = parent.map(str::to_string);
        meta.children = children.iter().map(|c| (*c).to_string()).collect();
        create_run(&meta).expect("run dir");
    }

    /// The whole tree below a run, and nothing beside it.
    ///
    /// Deepest first is the contract callers rely on to delete a child's
    /// directory before its parent's, so the grandchild has to lead.
    #[test]
    fn descendants_are_the_whole_subtree_deepest_first() {
        with_isolated_runs_dir("descendants-subtree", |_d| {
            // The parent remembers its children *and* the children name their
            // parent, which is what a healthy tree looks like on disk. The two
            // sources agreeing must report each child once.
            plant_run("root", None, &["kid-a", "kid-b"]);
            plant_run("kid-a", Some("root"), &["grandkid"]);
            plant_run("kid-b", Some("root"), &[]);
            plant_run("grandkid", Some("kid-a"), &[]);
            // A run of its own, and a run under it: neither is below `root`.
            plant_run("stranger", None, &[]);
            plant_run("stranger-kid", Some("stranger"), &[]);

            let found = descendant_run_ids("root");
            assert_eq!(found.len(), 3, "root has three runs below it: {found:?}");
            assert_eq!(found[0], "grandkid", "deepest first: {found:?}");
            assert!(found.contains(&"kid-a".to_string()));
            assert!(found.contains(&"kid-b".to_string()));
            assert!(!found.contains(&"stranger".to_string()));
            assert!(!found.contains(&"stranger-kid".to_string()));

            // The family is the same set plus the run itself, last.
            let family = family_of("root");
            assert_eq!(family.len(), 4);
            assert_eq!(family.last().map(String::as_str), Some("root"));
        });
    }

    /// Nothing walks upwards: deleting a child must leave its parent and its
    /// siblings alone, which is only true if they were never named.
    #[test]
    fn descendants_of_a_child_never_reach_its_parent_or_siblings() {
        with_isolated_runs_dir("descendants-no-upwards", |_d| {
            plant_run("root", None, &[]);
            plant_run("kid-a", Some("root"), &[]);
            plant_run("kid-b", Some("root"), &[]);
            plant_run("grandkid", Some("kid-a"), &[]);

            assert_eq!(descendant_run_ids("kid-a"), vec!["grandkid".to_string()]);
            assert!(descendant_run_ids("kid-b").is_empty());
            assert_eq!(family_of("kid-b"), vec!["kid-b".to_string()]);
        });
    }

    /// A child whose `meta.json` will not parse is skipped by `list_runs`, so
    /// the parent-scan cannot see it. The parent's own `children` list can, and
    /// that is the half that keeps a corrupt child from being left behind.
    #[test]
    fn a_child_only_the_parent_remembers_is_still_found() {
        with_isolated_runs_dir("descendants-unparseable-child", |_d| {
            plant_run("root", None, &["broken-kid", "never-existed"]);
            plant_run("broken-kid", Some("root"), &[]);
            std::fs::write(run_dir("broken-kid").join("meta.json"), "{not json")
                .expect("garble the child's record");

            let found = descendant_run_ids("root");
            // Found through `children`, because the scan cannot read it...
            assert_eq!(found, vec!["broken-kid".to_string()]);
            // ...and an id with no directory behind it is not a deletion
            // waiting to happen, so it is not reported at all.
            assert!(!found.contains(&"never-existed".to_string()));
        });
    }

    /// Metadata claiming an ancestor as a child ends the walk instead of
    /// looping forever.
    #[test]
    fn a_cycle_in_the_tree_terminates() {
        with_isolated_runs_dir("descendants-cycle", |_d| {
            plant_run("a", Some("b"), &["b"]);
            plant_run("b", Some("a"), &["a"]);

            assert_eq!(descendant_run_ids("a"), vec!["b".to_string()]);
            assert_eq!(descendant_run_ids("b"), vec!["a".to_string()]);
        });
    }

    /// A run nobody spawned anything under, and a run that is not there at
    /// all, both have nothing below them.
    #[test]
    fn a_lone_run_has_no_descendants() {
        with_isolated_runs_dir("descendants-lone", |_d| {
            plant_run("lonely", None, &[]);
            assert!(descendant_run_ids("lonely").is_empty());
            assert!(descendant_run_ids("no-such-run").is_empty());
        });
    }
}
