//! On-disk run state for background agent executions.
//!
//! Each run lives under `~/.leviath/runs/<run-id>/` with:
//! - `run.lvr` - the run file: its spec, its steps and its state (see
//!   `run_file` for how it is read)
//! - `final_output` - the answer it handed back
//! - `stages/<idx>/output.log` - readable agent output for that stage
//! - `stages/<idx>/logs.log`   - operational events + tool activity
//! - `stages/<idx>/taint_audit.json` - the taint gate's decisions
//! - `blobs/<sha256>` - its stored parts
//!
//! The run file names every other file there, and each is found through
//! what it names (see `beside`), never by a path worked out here.
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
//! run's state. The writers tests lay run directories down with live in
//! `fixtures_tests` and are `#[cfg(test)]`, so that stays true by compilation
//! rather than by convention.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use leviath_runtime::state::StageFile;

mod beside;
mod dashboard_log;
#[cfg(test)]
mod fixtures_tests;
mod force;
pub(crate) mod run_file;
pub(crate) use beside::{final_output_path, stage_file_path};
pub(crate) use run_file::RunHistory;
#[cfg(test)]
mod run_file_tests;
#[cfg(test)]
pub(crate) use dashboard_log::append_dashboard_log;
#[cfg(test)]
use dashboard_log::*;
pub(crate) use dashboard_log::{append_dashboard_log_to, dashboard_log_path};
#[cfg(test)]
pub(crate) use fixtures_tests::{
    create_run, create_run_in, create_signed_run, create_signed_run_in, write_context_snapshot,
    write_meta, write_meta_to, write_stages_index,
};
pub(crate) use force::{ForceCancelOutcome, force_cancel, force_cancel_in};

// The plain run-state data types (RunMeta, RunStatus, the snapshot structs, and
// the per-stage records) live in `leviath_core::run_meta`. Re-exported here so
// `crate::runstate::RunMeta` / `runstate::RunMeta` call sites across the cli
// resolve. All on-disk IO for these types remains in this module.
pub(crate) use leviath_core::run_meta::{
    ContextSnapshot, RunMeta, RunStatus, StageRecord, StageRunStatus,
};
#[cfg(test)]
pub(crate) use leviath_core::run_meta::{RegionEntrySnapshot, RegionSnapshot};

/// Read the context snapshot for a run, if present: its window as of its last
/// step.
pub(crate) fn read_context_snapshot(run_id: &str) -> Option<ContextSnapshot> {
    read_context_in(&run_dir(run_id))
}

/// [`read_context_snapshot`] for a run directory the caller already holds.
fn read_context_in(dir: &Path) -> Option<ContextSnapshot> {
    run_file::context_in(dir)
}

/// A parse cache keyed by a file's `(mtime, len)`: the file is re-read and
/// re-parsed only when its stat changes.
///
/// For pollers reading run state on a tick. A dashboard syncing at 10Hz
/// would otherwise decode every run's file every tick, for files that change
/// at most once per persist tick. A `stat` costs microseconds; this turns the
/// steady-state tick into stats plus clones of shared `Arc`s.
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
    /// The value `read` works out for `path`, worked out again only when the
    /// file's stat changed since the last call. `None` when the file is
    /// missing, or `read` finds nothing; negative results are cached too, so
    /// a persistently-bad file costs one stat per tick, not one read.
    ///
    /// The stat is skipped while the entry was checked less than
    /// `recheck_after` ago, the window worked out from what the cache holds
    /// for `path` (`None` for nothing, or a file that did not read). The stat
    /// is the cost. A dashboard over 750 runs stat'ed 1,500 files ten times a
    /// second to learn that 1,490 of them, belonging to runs that finished
    /// days ago, had not changed; two thirds of its idle CPU was that
    /// question. A caller that knows a file has settled (a finished run's
    /// record) asks it once a second instead, and a file it knows is live
    /// passes `Duration::ZERO` and is stat'ed every time.
    pub(crate) fn get_reading(
        &mut self,
        path: &Path,
        read: impl FnOnce() -> Option<T>,
        recheck_after: impl FnOnce(Option<&T>) -> std::time::Duration,
    ) -> Option<Arc<T>> {
        if let Some(entry) = self.entries.get(path)
            && entry.checked.elapsed() < recheck_after(entry.value.as_deref())
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
        let value = read().map(Arc::new);
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

    /// Drop entries for files under runs that no longer exist, so a
    /// long-lived poller's cache stays bounded by the live run set.
    pub(crate) fn retain_under(&mut self, keep: &std::collections::HashSet<PathBuf>) {
        self.entries.retain(|path, _| {
            path.parent()
                .is_some_and(|dir| keep.contains(&dir.to_path_buf()))
        });
    }
}

/// A file's size and modification time: what tells a reader whether it has
/// changed since it was last read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileStamp {
    pub(crate) mtime: std::time::SystemTime,
    pub(crate) len: u64,
}

/// The stamp of the file at `path`, or `None` when there is none.
fn file_stamp(path: &Path) -> Option<FileStamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some(FileStamp {
        mtime: meta.modified().unwrap_or(std::time::UNIX_EPOCH),
        len: meta.len(),
    })
}

/// The stamp of a run's file (`<run_dir>/run.lvr`), or `None` when it has
/// none.
pub(crate) fn run_file_stamp(run_id: &str) -> Option<FileStamp> {
    file_stamp(&run_file::path_in(&run_dir(run_id)))
}

/// The stamps of the two files a run's answer is read from: its run file,
/// which says whether it has one and names the file that holds it, and that
/// file.
pub(crate) fn answer_stamps(run_id: &str) -> (Option<FileStamp>, Option<FileStamp>) {
    let dir = run_dir(run_id);
    (
        file_stamp(&run_file::path_in(&dir)),
        final_output_path(&dir).and_then(|path| file_stamp(&path)),
    )
}

/// A run's context-window history: the full window (+ metadata) at each recorded
/// point over time, oldest first. Empty when there's no readable run file.
/// See [`run_file::history_in`].
pub(crate) fn context_history(run_id: &str) -> Vec<leviath_runtime::runfile::history::RunPoint> {
    run_history(run_id).points
}

/// [`context_history`] a point at a time, each handed to `each` as it is
/// reached rather than all held at once. False when the run has no history.
pub(crate) fn each_context_point(
    run_id: &str,
    each: impl FnMut(leviath_runtime::runfile::history::RunPoint),
) -> bool {
    run_file::walk_history_in(&run_dir(run_id), each).is_some()
}

/// A run's history: its window over time (see [`context_history`]) and the
/// edges it took, read off its run file. Empty when there is none.
pub(crate) fn run_history(run_id: &str) -> RunHistory {
    run_file::history_in(&run_dir(run_id)).unwrap_or_default()
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
/// this from URL segments on `GET /api/runs/{id}/logs` and friends, where
/// `Path::join` would otherwise happily accept `../../` or an absolute path.
///
/// Returning a definitely-missing path rather than an `Option` keeps every
/// caller's "no such run" branch as the single failure path, instead of adding a
/// second one that all of them would have to handle identically.
pub(crate) fn run_dir(run_id: &str) -> PathBuf {
    if !leviath_core::is_safe_path_component(run_id) {
        tracing::warn!(run_id = %run_id, "rejected an unsafe run id");
        return runs_dir().join("<invalid>");
    }
    runs_dir().join(run_id)
}

/// Forget the secrets the run `run_id` keeps in the secret store, as the run
/// is deleted: nothing signs with them once it is gone.
pub(crate) fn forget_secrets(run_id: &str) {
    leviath_runtime::secret_store::SecretStore::of_runs(&runs_dir()).forget_run(run_id);
}

/// Delete what the run `run_id` put in providers' file storage, reading its
/// ledger now, before the caller removes the run's directory. The deletes run
/// in the background on a runtime that is already going, or here, bounded,
/// when there is none. Best effort: the vendor's expiry is the backstop.
pub(crate) fn forget_provider_files(run_id: &str) {
    let entries = leviath_runtime::provider_files::take_ledger(&run_dir(run_id));
    if entries.is_empty() {
        return;
    }
    // Built where the deletes run: a provider's client wants a runtime to
    // stand in. One that cannot be built is one whose files are left to
    // expire, which the delete says per file.
    let work = async move {
        let config = crate::config::Config::load().unwrap_or_default();
        let registry = crate::commands::run::session::build_provider_registry_from_config(&config)
            .unwrap_or_default();
        leviath_runtime::provider_files::delete_entries(&entries, &registry).await
    };
    match tokio::runtime::Handle::try_current() {
        Ok(runtime) => {
            runtime.spawn(work);
        }
        Err(_) => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a current-thread runtime builds");
            let _ = runtime.block_on(async {
                tokio::time::timeout(std::time::Duration::from_secs(30), work).await
            });
        }
    }
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
/// last-writer-wins over the run file and the files beside it, interleaving
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

/// Read run metadata for a given run ID.
pub(crate) fn read_meta(run_id: &str) -> anyhow::Result<RunMeta> {
    read_meta_from(&run_dir(run_id))
}

/// Read a run's final output, content included.
///
/// The run's file says whether there is one, how big it is, and which file
/// beside it holds it. Returns `None` when the run produced no answer, or
/// when the file it names does not read.
pub(crate) fn read_final_output(run_id: &str) -> Option<leviath_core::FinalOutput> {
    read_meta_and_answer(run_id).ok()?.1
}

/// [`read_final_output`] for a run directory the caller already resolved,
/// with the metadata it already read.
#[cfg(test)]
pub(crate) fn read_final_output_in(
    dir: &std::path::Path,
    meta: &RunMeta,
) -> Option<leviath_core::FinalOutput> {
    answer_in(dir, meta, &beside::named_files(dir))
}

/// A run's record and its answer, with its run file read once for both.
/// See [`read_meta`] and [`read_final_output`].
pub(crate) fn read_meta_and_answer(
    run_id: &str,
) -> anyhow::Result<(RunMeta, Option<leviath_core::FinalOutput>)> {
    let dir = run_dir(run_id);
    let tail = run_file::tail_in(&dir)?;
    let meta = leviath_runtime::runfile::summary_of(&tail.spec, &tail.state, tail.updated_at);
    let answer = answer_in(&dir, &meta, &tail.state.files);
    Ok((meta, answer))
}

/// The answer `meta` says the run in `dir` handed back, from the file
/// `files` names for it.
fn answer_in(
    dir: &std::path::Path,
    meta: &RunMeta,
    files: &leviath_runtime::state::RunFiles,
) -> Option<leviath_core::FinalOutput> {
    let descriptor = meta.final_output.clone()?;
    let bytes = beside::read_named(dir, files.final_output.as_ref()?)?;
    Some(leviath_core::FinalOutput {
        content: String::from_utf8_lossy(&bytes).into_owned(),
        format: descriptor.format,
        stage: descriptor.stage,
        submitted_at: descriptor.submitted_at,
        truncated: descriptor.truncated,
        artifacts: descriptor.artifacts,
    })
}

/// Write a run's answer beside its run file, and name it there, as the
/// runtime's persistence lane does for a live run.
#[cfg(test)]
pub(crate) fn write_final_output(dir: &std::path::Path, content: &str) -> anyhow::Result<()> {
    let name = leviath_core::FINAL_OUTPUT_FILE;
    leviath_sys::write_private(&dir.join(name), content.as_bytes())
        .expect("a test's run directory takes its answer");
    fixtures_tests::name_files(dir, |files| {
        files.final_output = Some(leviath_runtime::state::FileRef::whole(
            name,
            content.as_bytes(),
        ));
    });
    Ok(())
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
/// Ages against `last_progress_at`, falling back to `updated_at` for a record
/// without it, rather than declaring every such run stale at once.
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
/// own configured `runs_dir` rather than the home-resolved one): its run file
/// as of its last step.
pub(crate) fn read_meta_from(dir: &std::path::Path) -> anyhow::Result<RunMeta> {
    let tail = run_file::tail_in(dir)?;
    Ok(leviath_runtime::runfile::summary_of(
        &tail.spec,
        &tail.state,
        tail.updated_at,
    ))
}

/// Inner implementation of `list_runs`, parameterised so a missing runs
/// directory can be exercised in tests without deleting real on-disk state.
fn list_runs_in_dir(dir: PathBuf) -> Vec<RunMeta> {
    crate::run_index::list(&dir)
}

/// List all runs, sorted by started_at descending (most recent first),
/// through the run index (see [`crate::run_index`]), so a run file is read
/// only when it changed since the index last saw it. Silently skips any runs
/// whose metadata cannot be read.
pub(crate) fn list_runs() -> Vec<RunMeta> {
    list_runs_in_dir(runs_dir())
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
/// scan over every run's `parent_run_id` misses a child whose run file will
/// not read (`list_runs` skips it), and a parent's own `children` list misses
/// one whose file says nothing of its parent. An id named only by
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

/// [`list_runs`] through a [`StatCache`], for pollers: each run's file is
/// re-read only when its stat changes, the runs directory is listed again
/// only when it may have gained or lost a run (see [`RunDirListing`]), and
/// cache entries for deleted runs are dropped. Same ordering and
/// skip-unreadable behavior as `list_runs`.
pub(crate) fn list_runs_cached(
    cache: &mut StatCache<RunMeta>,
    listing: &mut RunDirListing,
) -> Vec<Arc<RunMeta>> {
    let root = runs_dir();
    if listing.refresh(&root) {
        cache.retain_under(&listing.dir_set());
    }
    let mut runs = Vec::with_capacity(listing.dirs.len());
    // Loaded the first time a run has to be read, so a warm poll that only
    // stats never opens it.
    let mut index: Option<crate::run_index::RunIndex> = None;
    for dir in &listing.dirs {
        // A run this poller already knows to be finished is asked about
        // once a second; a live one (or one never seen) every time.
        if let Some(meta) = cache.get_reading(
            &run_file::path_in(dir),
            || {
                index
                    .get_or_insert_with(|| crate::run_index::RunIndex::load(&root))
                    .run(dir)
            },
            |meta| meta.map_or(std::time::Duration::ZERO, settle_window),
        ) {
            runs.push(meta);
        }
    }
    if let Some(mut index) = index {
        index.keep_only(&listing.dir_set());
        index.save();
    }
    runs.sort_by_key(|r| std::cmp::Reverse(r.started_at));
    runs
}

/// The run directories under the runs directory, for a poller that asks many
/// times a second.
///
/// Listing a directory of thousands of runs ten times a second, to learn that
/// no run was created or deleted, would be most of what an idle poller does.
/// Either one changes the runs directory's mtime, so the listing is kept
/// until the mtime moves.
///
/// Unless the mtime is recent: a filesystem stamps times more coarsely than a
/// poller asks (a second, on some), so a run created in the same stamp as the
/// last listing would leave the mtime where it was. A listing is only trusted
/// once its mtime is [`LISTING_TRUSTED_AFTER`] older than the moment it was
/// taken; a directory that changed more recently than that is listed every
/// time, as it always was.
#[derive(Default)]
pub(crate) struct RunDirListing {
    dirs: Vec<PathBuf>,
    /// The runs directory's mtime at the last listing, and when it was taken.
    stamp: Option<(std::time::SystemTime, std::time::SystemTime)>,
    /// Whether the last [`refresh`](Self::refresh) listed the directory.
    relisted: bool,
}

#[cfg(test)]
impl RunDirListing {
    /// As if the last listing was taken long after its directory last
    /// changed, so an unchanged directory is not listed again.
    pub(crate) fn age(&mut self) {
        self.stamp = self
            .stamp
            .map(|(mtime, _)| (mtime, mtime + LISTING_TRUSTED_AFTER));
    }
}

/// See [`RunDirListing`]: coarser than any filesystem's time stamps.
const LISTING_TRUSTED_AFTER: std::time::Duration = std::time::Duration::from_secs(2);

impl RunDirListing {
    /// List `dir` again unless it cannot have changed since the last listing.
    /// Returns whether it listed, which is when the set of runs may differ.
    pub(crate) fn refresh(&mut self, dir: &Path) -> bool {
        let mtime = std::fs::metadata(dir).and_then(|meta| meta.modified()).ok();
        let settled = match (mtime, self.stamp) {
            (Some(now), Some((then, listed_at))) => {
                now == then
                    && listed_at
                        .duration_since(then)
                        .is_ok_and(|age| age >= LISTING_TRUSTED_AFTER)
            }
            _ => false,
        };
        self.relisted = !settled;
        if settled {
            return false;
        }
        let listed_at = std::time::SystemTime::now();
        self.dirs = std::fs::read_dir(dir)
            .map(|entries| entries.filter_map(|e| e.ok()).map(|e| e.path()).collect())
            .unwrap_or_default();
        self.stamp = mtime.map(|mtime| (mtime, listed_at));
        true
    }

    /// Whether the last [`refresh`](Self::refresh) listed the directory.
    pub(crate) fn relisted(&self) -> bool {
        self.relisted
    }

    /// The listed run directories, as a set for [`StatCache::retain_under`].
    pub(crate) fn dir_set(&self) -> std::collections::HashSet<PathBuf> {
        self.dirs.iter().cloned().collect()
    }
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
) -> Arc<Vec<StageRecord>> {
    read_stages_index_settled(run_id, cache, std::time::Duration::ZERO)
}

/// [`read_stages_index_cached`] with the poller's [`settle_window`] for the
/// run, so a finished run's stage ledger is not stat'ed every tick either.
/// Shared, not copied: the ledger is handed to the run list every tick.
pub(crate) fn read_stages_index_settled(
    run_id: &str,
    cache: &mut StatCache<Vec<StageRecord>>,
    recheck_after: std::time::Duration,
) -> Arc<Vec<StageRecord>> {
    let dir = run_dir(run_id);
    cache
        .get_reading(
            &run_file::path_in(&dir),
            || read_stages_in(&dir),
            |_| recheck_after,
        )
        .unwrap_or_default()
}

/// [`read_context_snapshot`] through a [`StatCache`], for pollers. The
/// snapshot is shared, not cloned: a context window is the largest thing in a
/// run dir, and handing out copies per tick is the churn this cache removes.
pub(crate) fn read_context_snapshot_cached(
    run_id: &str,
    cache: &mut StatCache<ContextSnapshot>,
) -> Option<Arc<ContextSnapshot>> {
    let dir = run_dir(run_id);
    cache.get_reading(
        &run_file::path_in(&dir),
        || read_context_in(&dir),
        |_| std::time::Duration::ZERO,
    )
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
#[cfg(test)]
pub(crate) fn stage_dir(run_id: &str, stage_idx: usize) -> PathBuf {
    run_dir(run_id).join("stages").join(stage_idx.to_string())
}

/// Read a run's per-stage ledger as of its last step, or return an empty vec
/// on any error.
pub(crate) fn read_stages_index(run_id: &str) -> Vec<StageRecord> {
    read_stages_in(&run_dir(run_id)).unwrap_or_default()
}

/// [`read_stages_index`] for a run directory the caller already holds,
/// `None` when it records no ledger.
fn read_stages_in(dir: &Path) -> Option<Vec<StageRecord>> {
    run_file::stages_in(dir)
}

/// Ensure the per-stage directory exists (called before first write).
#[cfg(test)]
fn ensure_stage_dir(run_id: &str, stage_idx: usize) {
    let dir = stage_dir(run_id, stage_idx);
    let _ = leviath_sys::create_private_dir_all(&dir);
}

/// Append a line of readable agent output to the per-stage output log, and
/// name the log in the run's file when it has one.
///
/// Test-only; the runtime's persistence lane writes a live run's.
#[cfg(test)]
pub(crate) fn append_stage_output(run_id: &str, stage_idx: usize, text: &str) {
    append_stage_line(run_id, stage_idx, StageFile::Output, text);
}

/// Append a line of operational/tool-activity log to the per-stage logs
/// file, and name the log in the run's file when it has one.
///
/// Test-only; the runtime's persistence lane writes a live run's.
#[cfg(test)]
pub(crate) fn append_stage_log(run_id: &str, stage_idx: usize, text: &str) {
    append_stage_line(run_id, stage_idx, StageFile::Logs, text);
}

#[cfg(test)]
fn append_stage_line(run_id: &str, stage_idx: usize, which: StageFile, text: &str) {
    use std::io::Write;
    ensure_stage_dir(run_id, stage_idx);
    let index = stage_idx as u32;
    let path = leviath_runtime::state::under(&run_dir(run_id), &which.path(index));
    if let Ok(mut file) = leviath_sys::open_private_append(&path) {
        let _ = writeln!(file, "{}", text);
    }
    let len = std::fs::metadata(&path).map_or(0, |m| m.len());
    fixtures_tests::name_files(&run_dir(run_id), |files| {
        files.set_stage_file(
            index,
            which,
            leviath_runtime::state::FileRef::log(which.path(index), len),
        );
    });
}

/// Read the last `max_bytes` of the readable output log for a specific stage.
pub(crate) fn tail_stage_output(run_id: &str, stage_idx: usize, max_bytes: u64) -> String {
    beside::tail_stage_file(&run_dir(run_id), stage_idx, StageFile::Output, max_bytes)
}

/// Read the last `max_bytes` of the operational log for a specific stage.
pub(crate) fn tail_stage_log(run_id: &str, stage_idx: usize, max_bytes: u64) -> String {
    beside::tail_stage_file(&run_dir(run_id), stage_idx, StageFile::Logs, max_bytes)
}

/// Which stage's logs to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StageSelector {
    /// The stage the run is on now - the last entry in its ledger. What a
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
/// The one answer to "where is a run's output" for `GET /api/runs/{id}/logs`
/// and `agent_result` alike: logs live per stage under `stages/<idx>/`, and a
/// run with no stage recorded yet has none.
///
/// Stages come from the run's ledger rather than a `read_dir` of `stages/`,
/// because the ledger is the record of which stages exist and in what order -
/// the directory is just where their bytes landed.
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
        StageSelector::Current => stages.len().checked_sub(1).map(read).unwrap_or_default(),
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
///
/// The config path comes along: a daemon host booted in this scope builds its
/// reloader from `Config::config_path()`, and left to the environment that
/// would be whatever file a concurrently isolated test is pointing at.
#[cfg(test)]
fn runs_dir_isolation_vars(
    base_dir: &std::path::Path,
) -> [(&'static str, Option<std::ffi::OsString>); 3] {
    [
        (
            "LEVIATH_RUNS_DIR",
            Some(base_dir.join("runs").into_os_string()),
        ),
        (
            "LEVIATH_DASHBOARD_LOG_PATH",
            Some(base_dir.join("dashboard.log").into_os_string()),
        ),
        (
            "LEVIATH_CONFIG_PATH",
            Some(base_dir.join("config.toml").into_os_string()),
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

    /// `run_id` arrives from URL segments on `GET /api/runs/{id}/logs` and
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
    /// out; the stamp is the only field on the run's record that separates a run that
    /// is working from one that is only ticking.
    #[test]
    fn a_fresh_heartbeat_does_not_rescue_a_run_that_stopped_moving() {
        let mut meta = live_on_disk("r1");
        let now = 1_000 + STALE_AFTER_SECS * 10;
        meta.updated_at = now; // the heartbeat, still beating
        meta.last_progress_at = Some(1_000); // but nothing has moved since 1000
        assert!(looks_abandoned(&meta, Some(&held(&[])), now));
    }

    /// A record without the stamp falls back to `updated_at`, instead of
    /// reading as stale.
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
        // A record without its optional fields.
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
        // A record without the progress stamp has no answer, which is
        // why the field is an Option: `Some(0)` would read as "last moved in 1970"
        // and invite a reconciler to declare it abandoned.
        assert!(meta.last_progress_at.is_none());
    }

    /// `pid` is always 0 in the shared world. A record that omits it entirely
    /// must still load, so the field can be dropped without stranding a run.
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

    /// A run's answer is read from the file its run file names; a run with
    /// none, or whose file is not there, has none.
    #[test]
    fn read_final_output_needs_both_the_descriptor_and_the_named_file() {
        with_isolated_runs_dir("read-final-output", |_| {
            // No run at all.
            assert!(read_final_output("no-such-run").is_none());

            // A run with no answer recorded.
            let meta = fixtures::run_meta("run-silent");
            create_run(&meta).expect("run dir");
            assert!(read_final_output("run-silent").is_none());

            // No file named: no answer, though the record claims one.
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

            // Written and named, it reads.
            write_final_output(&run_dir("run-claimed"), &answer.content).expect("the answer");
            let read = read_final_output("run-claimed").expect("both halves are there");
            assert_eq!(read.content, "the answer");
            assert_eq!(read.format.as_deref(), Some("markdown"));
            assert_eq!(read.stage, "present");
            // The record and the answer read together say the same.
            let (record, together) = read_meta_and_answer("run-claimed").expect("it reads");
            assert_eq!(record.run_id, "run-claimed");
            assert_eq!(together, Some(read));
            assert!(read_final_output_in(&run_dir("run-claimed"), &record).is_some());

            // Named and gone, it does not.
            std::fs::remove_file(run_dir("run-claimed").join(leviath_core::FINAL_OUTPUT_FILE))
                .unwrap();
            assert!(read_final_output("run-claimed").is_none());

            // A record that claims an answer, read against a directory with
            // no run file naming one, has none.
            let empty = tempfile::tempdir().unwrap();
            assert!(read_final_output_in(empty.path(), &claimed).is_none());
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
                Some("mock/model-x".into()),
                "/tmp".into(),
                2,
            );

            create_run(&meta).unwrap();
            let back = read_meta(&meta.run_id).unwrap();
            assert_eq!(back.run_id, "test-roundtrip-unit");
            assert_eq!(back.agent_name, "test-agent");
            assert_eq!(back.task, "unit test");
            assert_eq!(back.model.as_deref(), Some("mock/model-x"));
        });
    }

    #[test]
    fn read_meta_returns_err_on_a_run_file_that_will_not_read() {
        with_isolated_runs_dir("read-meta-returns-err-on-corrupted-json", |_d| {
            let run_id = "corrupted-meta-run";
            let dir = run_dir(run_id);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(run_file::path_in(&dir), "not a run file").unwrap();

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
                regions: vec![crate::test_fixtures::fixtures::region("task")],
            };
            write_context_snapshot(run_id, &snap).unwrap();
            let back = read_context_snapshot(run_id).unwrap();
            assert_eq!(back.stage_name, "stage0", "the stage the run is in");
        });
    }

    #[test]
    fn read_context_snapshot_missing_returns_none() {
        assert!(read_context_snapshot("nonexistent-ctx-run").is_none());
    }

    /// Read `path` as text through `cache`, parsed by `parse`, rechecked
    /// after `window`.
    fn cached_text<T>(
        cache: &mut StatCache<T>,
        path: &Path,
        parse: impl FnOnce(&str) -> Option<T>,
        window: std::time::Duration,
    ) -> Option<Arc<T>> {
        let read = || {
            std::fs::read_to_string(path)
                .ok()
                .and_then(|text| parse(&text))
        };
        cache.get_reading(path, read, |_| window)
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
            cached_text(
                cache,
                path,
                |text| {
                    *parses += 1;
                    text.trim().parse().ok()
                },
                std::time::Duration::ZERO,
            )
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
        let now = std::time::Duration::ZERO;
        cached_text(
            &mut cache,
            &live.join("meta.json"),
            |t| t.trim().parse().ok(),
            now,
        );
        cached_text(
            &mut cache,
            &dead.join("meta.json"),
            |t| t.trim().parse().ok(),
            now,
        );
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
                    regions: vec![crate::test_fixtures::fixtures::region("task")],
                },
            )
            .unwrap();

            let mut metas = StatCache::default();
            let mut stages = StatCache::default();
            let mut contexts = StatCache::default();
            let mut listing = RunDirListing::default();

            let listed = list_runs_cached(&mut metas, &mut listing);
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
            let listed = list_runs_cached(&mut metas, &mut listing);
            assert_eq!(listed.len(), 2);
            assert_eq!(listed[0].run_id, "cached-run-2", "newest first");

            // A run dir with a garbled run file is skipped, not fatal - and
            // skipped cheaply on every later tick (the negative result is
            // cached until the file changes).
            std::fs::create_dir_all(run_dir("garbled-run")).unwrap();
            std::fs::write(run_file::path_in(&run_dir("garbled-run")), "not json {{").unwrap();
            assert_eq!(list_runs_cached(&mut metas, &mut listing).len(), 2);

            // A run whose dir disappears falls out of the cached listing.
            std::fs::remove_dir_all(run_dir("garbled-run")).unwrap();
            std::fs::remove_dir_all(run_dir("cached-run")).unwrap();
            std::fs::remove_dir_all(run_dir("cached-run-2")).unwrap();
            assert!(list_runs_cached(&mut metas, &mut listing).is_empty());
            assert!(read_stages_index_cached("cached-run", &mut stages).is_empty());
            assert!(read_context_snapshot_cached("cached-run", &mut contexts).is_none());

            // And a missing runs DIRECTORY altogether lists nothing (the
            // read_dir-failed arm).
            std::fs::remove_dir_all(runs_dir()).unwrap();
            assert!(list_runs_cached(&mut metas, &mut listing).is_empty());
        });
    }

    /// A listing is kept only once the directory's mtime is older than the
    /// listing by more than a coarse filesystem's stamp; a directory that
    /// changed since, or recently, or is missing, is listed again.
    #[test]
    fn a_run_listing_is_kept_only_once_its_directory_has_settled() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("run-a")).unwrap();
        let mut listing = RunDirListing::default();
        assert!(listing.refresh(dir.path()), "never listed");
        assert!(listing.relisted());
        assert_eq!(listing.dir_set().len(), 1);
        assert!(listing.refresh(dir.path()), "changed too recently to trust");

        // As if listed well after the directory last changed.
        listing.age();
        let (mtime, _) = listing.stamp.unwrap();
        assert!(!listing.refresh(dir.path()), "settled and unchanged");
        assert!(!listing.relisted());

        // The directory moved on since that listing.
        listing.stamp = Some((
            mtime - std::time::Duration::from_secs(60),
            mtime + LISTING_TRUSTED_AFTER,
        ));
        assert!(listing.refresh(dir.path()));

        // A runs directory that is not there lists nothing.
        let missing = dir.path().join("missing");
        assert!(listing.refresh(&missing));
        assert!(listing.dir_set().is_empty());
        assert!(listing.stamp.is_none());
    }

    /// A settled entry is answered from memory inside its window and from the
    /// filesystem outside it, and the window is worked out from what is
    /// cached.
    #[test]
    fn a_stat_cache_honours_the_recheck_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meta.json");
        std::fs::write(&path, "1").unwrap();
        let mut cache: StatCache<String> = StatCache::default();
        let parse = |s: &str| Some(s.to_string());
        let read = || std::fs::read_to_string(&path).ok();
        let mut seen = Vec::new();
        let mut window_for = |cached: Option<&String>| {
            seen.push(cached.cloned());
            std::time::Duration::from_secs(3600)
        };
        assert_eq!(
            *cache.get_reading(&path, read, &mut window_for).unwrap(),
            "1"
        );
        assert_eq!(
            *cache.get_reading(&path, read, &mut window_for).unwrap(),
            "1"
        );
        // Nothing was cached for the first read, so it had no window to ask
        // about; the second saw what the first cached.
        assert_eq!(seen, vec![Some("1".to_string())]);

        // The file changes. Inside the window the old value stands, because
        // the point is not to stat; outside it the change is seen. The new
        // content is a different LENGTH: the stamp is (mtime, len), and a
        // same-length rewrite inside one mtime tick (Windows CI has coarse
        // ticks) is invisible to it by design.
        std::fs::write(&path, "22").unwrap();
        let hour = std::time::Duration::from_secs(3600);
        let now = std::time::Duration::ZERO;
        assert_eq!(*cached_text(&mut cache, &path, parse, hour).unwrap(), "1");
        assert_eq!(*cached_text(&mut cache, &path, parse, now).unwrap(), "22");
        // A stat that finds the same stamp refreshes the check time without a
        // parse, so the next windowed read is answered from memory too.
        assert_eq!(*cached_text(&mut cache, &path, parse, now).unwrap(), "22");
        assert_eq!(*cached_text(&mut cache, &path, parse, hour).unwrap(), "22");

        // A missing file is forgotten, and a window does not resurrect it.
        std::fs::remove_file(&path).unwrap();
        assert!(cached_text(&mut cache, &path, parse, now).is_none());
        assert!(cached_text(&mut cache, &path, parse, hour).is_none());
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
    /// Give `path` a modification time comfortably in the future, so a rewrite
    /// that lands inside one filesystem clock tick still reads as a change.
    fn touch_newer(path: &std::path::Path) {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("the record is there to touch");
        file.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5))
            .expect("the modification time is ours to set");
    }

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
            let mut listing = RunDirListing::default();
            let mut stages = StatCache::default();
            assert_eq!(list_runs_cached(&mut metas, &mut listing).len(), 2);

            // Both records change on disk.
            done.title = Some("renamed".to_string());
            write_meta(&done).unwrap();
            live.iteration = 7;
            write_meta(&live).unwrap();
            // Written with a strictly newer mtime, the way `save_config` does:
            // the cache re-reads on a changed stat, and `iteration: 0` becomes
            // `iteration: 7` without the file changing length, so a rewrite
            // inside one filesystem clock tick is a stat the cache cannot tell
            // from the one it already holds.
            touch_newer(&run_file::path_in(&run_dir("live")));
            let listed = list_runs_cached(&mut metas, &mut listing);
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
                .get_reading(
                    &run_file::path_in(&run_dir("done")),
                    || read_meta_from(&run_dir("done")).ok(),
                    |_| std::time::Duration::ZERO,
                )
                .unwrap();
            assert_eq!(fresh.title.as_deref(), Some("renamed"));
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
            create_run(&fixtures::run_meta(run_id)).unwrap();
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
            create_run(&fixtures::run_meta(run_id)).unwrap();
            append_stage_log(run_id, 0, "event A");
            append_stage_log(run_id, 0, "event B");
            let log = tail_stage_log(run_id, 0, 4096);
            assert!(log.contains("event A"));
            assert!(log.contains("event B"));
        });
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
            create_run(&fixtures::run_meta(run_id)).unwrap();
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
    fn read_meta_from_fails_on_missing_file() {
        let tmpdir = tempfile::tempdir().unwrap();
        let result = read_meta_from(tmpdir.path());
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
        create_run_in(&run_subdir, &meta).unwrap();

        let runs = list_runs_in_dir(tmpdir.path().to_path_buf());
        assert!(runs.iter().any(|r| r.run_id == run_id));
    }

    #[test]
    fn list_runs_in_dir_skips_entry_with_corrupted_meta_json() {
        // A subdirectory whose run file does not read, and one with none, are
        // skipped rather than failing the listing.
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
        create_run_in(&good_subdir, &meta).unwrap();

        let bad_subdir = tmpdir.path().join(bad_run_id);
        std::fs::create_dir_all(&bad_subdir).unwrap();
        std::fs::write(run_file::path_in(&bad_subdir), "not a run file").unwrap();

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
        for status in [RunStatus::Complete, RunStatus::Error, RunStatus::Cancelled] {
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

    /// A directory whose run file cannot be written still counts as "found" -
    /// the caller must not report "no such run" for a run that plainly exists.
    #[test]
    fn force_cancel_reports_a_write_failure_but_still_found_the_run() {
        crate::test_support::with_tracing(|| {
            let base = tempfile::tempdir().unwrap();
            let dir = base.path().join("blocked-run");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(run_file::path_in(&dir), "not a run file").unwrap();

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

    /// A child whose run file will not parse is skipped by `list_runs`, so
    /// the parent-scan cannot see it. The parent's own `children` list can, and
    /// that is the half that keeps a corrupt child from being left behind.
    #[test]
    fn a_child_only_the_parent_remembers_is_still_found() {
        with_isolated_runs_dir("descendants-unparseable-child", |_d| {
            plant_run("root", None, &["broken-kid", "never-existed"]);
            plant_run("broken-kid", Some("root"), &[]);
            std::fs::write(run_file::path_in(&run_dir("broken-kid")), "{not json")
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

    /// A deleted run's uploads are read from its ledger before its directory
    /// goes; with no provider configured to delete them they are left to
    /// expire, and a run with no ledger does nothing at all.
    #[test]
    fn a_deleted_runs_uploads_are_taken_from_its_ledger_first() {
        with_isolated_runs_dir("forget-files", |_d| {
            let dir = run_dir("uploaded");
            std::fs::create_dir_all(&dir).unwrap();
            let ledger = dir.join(leviath_runtime::provider_files::LEDGER_FILE);
            std::fs::write(
                &ledger,
                r#"{"files":[{"provider":"nobody","sha256":"a","file":{"id":"f"}}]}"#,
            )
            .unwrap();
            forget_provider_files("uploaded");
            assert!(!ledger.exists(), "the ledger is taken");
            forget_provider_files("uploaded");
        });
    }

    #[tokio::test]
    async fn a_deleted_runs_uploads_are_deleted_in_the_background_on_a_running_runtime() {
        with_isolated_runs_dir_async("forget-files-async", |_d| async move {
            let dir = run_dir("uploaded");
            std::fs::create_dir_all(&dir).unwrap();
            let ledger = dir.join(leviath_runtime::provider_files::LEDGER_FILE);
            std::fs::write(
                &ledger,
                r#"{"files":[{"provider":"nobody","sha256":"a","file":{"id":"f"}}]}"#,
            )
            .unwrap();
            forget_provider_files("uploaded");
            assert!(!ledger.exists());
        })
        .await;
    }
}
