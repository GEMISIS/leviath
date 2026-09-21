//! The async I/O lane for agent-state persistence.
//!
//! The snapshot-dispatch system builds a [`PersistJob`] (an agent's `meta.json` +
//! `context.json` value snapshot) whenever the agent meaningfully changes and
//! sends it to this single-worker lane. [`persistence_worker`] writes each job's
//! files under `<runs_dir>/<run_id>/` **one at a time**, so writes for a given
//! agent never race or land out of order. Each file is written to a temp path and
//! atomically renamed into place, so a concurrent reader (the dashboard) never
//! sees a half-written file.
//!
//! Every error is logged and counted in
//! [`PersistLaneStats`](crate::persist_stats::PersistLaneStats), and the lane
//! itself never blocks or fails on one: a write that cannot be made is reported
//! and the lane moves to the next message. A lost **journal** write also names
//! its run there, and the world fails that run on its next tick, because a run
//! whose history cannot record what it did must not go on doing things. The
//! failure travels as an ordinary run-status change, so neither the lane nor the
//! schedule waits for it.
//!
//! A write dropped because the run directory is gone is not a failure of any of
//! this - see [`may_write`].

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::persist_stats::PersistLaneStats;

use leviath_core::run_archive;
use leviath_core::run_meta::{ContextSnapshot, RunMeta, StageRecord};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::UnboundedReceiver;

/// One agent snapshot to write to disk.
pub(crate) struct PersistJob {
    /// The run id (its directory name under the runs dir).
    pub run_id: String,
    /// The `meta.json` contents.
    pub meta: RunMeta,
    /// The `context.json` contents.
    pub context: ContextSnapshot,
    /// The `stages.json` index (per-stage names/status/tokens/timestamps). Empty
    /// ⇒ not rewritten this job (agents without a stage ledger).
    pub stages: Vec<StageRecord>,
    /// Readable output lines to append to `stages/<idx>/output.log`.
    pub output_appends: Vec<(usize, String)>,
    /// Operational log lines to append to `stages/<idx>/logs.log`.
    pub log_appends: Vec<(usize, String)>,
    /// `(stage_index, serialized GateEvent log)` to write to
    /// `stages/<idx>/taint_audit.json`. `None` ⇒ no audit to persist.
    pub taint_audit: Option<(usize, String)>,
    /// The run's answer, to write to its `final_output` sidecar. `None` ⇒
    /// nothing to write this job, either because the run has no answer or
    /// because the one it has is already on disk.
    ///
    /// `meta.json` carries only the descriptor, so this is the sole path by
    /// which the bytes reach disk. Sent only when it changes: a heartbeat that
    /// rewrote a quarter-megabyte answer every thirty seconds would be pure
    /// waste on a long run.
    pub final_output: Option<String>,
    /// Serialized [`FanOutState`](crate::fanout::FanOutState) for a parent parked
    /// mid fan-out, written to `fanout.json` so the split/merge resumes after a
    /// restart. `None` ⇒ the agent isn't waiting on a fan-out (any stale file is
    /// removed).
    pub fanout: Option<String>,
    /// Serialized [`InteractionPointState`](crate::interaction_points::InteractionPointState)
    /// for an agent parked at a stage-boundary interaction point (e.g. plan_approval),
    /// written to `interactions.json` so a restart re-presents the same prompt rather
    /// than dropping it and re-inferring. `None` ⇒ the agent isn't parked
    /// at an interaction point (any stale file is removed).
    pub interactions: Option<String>,
}

/// What became of one append.
///
/// The three states are different facts about the run: one says where the record
/// is, one says there is no journal for it to be in, and one says a write was
/// attempted and lost. Only the last is a problem, and telling it apart from the
/// second is why this is not a `bool` - a dispatch waiting for its record has to
/// be able to say which of them happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Appended {
    /// On disk, at this byte offset in the run's journal. Monotonic within a
    /// run: the journal is only ever appended to, so a later record always has
    /// a higher position, and the position of a record never changes.
    Landed {
        /// The byte offset of the record's frame.
        position: u64,
    },
    /// Nothing was written, and nothing is wrong. Either this world keeps no
    /// journal (an in-memory world), or the run has no archive yet, or the run
    /// was deleted from under the lane.
    NoJournal,
    /// The write was attempted, retried, and lost. The run is failed for it, so
    /// nothing else happens that the journal would have to explain.
    Failed,
}

#[cfg(test)]
impl Appended {
    /// Where the record landed, if it landed at all.
    ///
    /// For tests: the lane's own callers want the three states apart, which is
    /// what the enum is for, and a test asserting on a position wants the one
    /// number without a match arm it never takes.
    pub(crate) fn landed_at(self) -> Option<u64> {
        match self {
            Self::Landed { position } => Some(position),
            Self::NoJournal | Self::Failed => None,
        }
    }
}

/// One message on the persistence lane.
pub(crate) enum PersistMsg {
    /// A whole-agent snapshot (`meta.json` + `context.json` + the archive step).
    /// Boxed: a snapshot dwarfs an `Append` and the channel moves these by value.
    Snapshot(Box<PersistJob>),
    /// Append one journal record to `<runs_dir>/<run_id>/run.lvr` - how a tool
    /// batch's dispatch and per-call completions reach the archive between
    /// snapshots.
    Append {
        /// The run id (its directory name under the runs dir).
        run_id: String,
        /// The record to append. Boxed like `Snapshot`'s job: `RunRecord`'s
        /// checkpoint variants are large and the channel moves these by value.
        record: Box<leviath_core::run_archive::RunRecord>,
        /// Carries what became of the append: the dispatch-side barrier that
        /// keeps a batch record ahead of the batch's side effects, and now also
        /// tells the dispatcher whether the record is really there. Fired
        /// exactly once however the append went. `None` for fire-and-forget
        /// appends (per-call results).
        ack: Option<tokio::sync::oneshot::Sender<Appended>>,
    },
    /// Buffered per-stage output/log lines with nothing else to report. The
    /// dispatch system sends this instead of a full [`PersistMsg::Snapshot`]
    /// when lines were buffered but the run's watermark did not move, so tool
    /// activity between iterations does not force a whole-window snapshot
    /// (context deep-clone, meta/context rewrite, archive record) per batch of
    /// log lines, several times per iteration.
    StageLines {
        /// The run id (its directory name under the runs dir).
        run_id: String,
        /// Readable output lines to append to `stages/<idx>/output.log`.
        output_appends: Vec<(usize, String)>,
        /// Operational log lines to append to `stages/<idx>/logs.log`.
        log_appends: Vec<(usize, String)>,
    },
}

/// The single-lane persistence worker: writes each [`PersistJob`]'s files under
/// `runs_dir`, one at a time, until the job channel closes (world shutdown).
///
/// It also owns this daemon's run-ownership identity - a stable per-machine id
/// (`<runs_dir>/../machine-id`, created once) and a per-process `world_id` - which
/// it stamps into every run's portable archive so a run copied to another machine
/// is unambiguously attributable (see [`leviath_core::run_archive`]).
///
/// `runs_dir: None` means the world runs in memory only: the worker drains and
/// drops every message without touching the filesystem (no run dirs, no
/// machine-id), while keeping the channel-close shutdown contract so
/// [`flush_and_stop`](crate::world::PipelineWorld::flush_and_stop) still joins
/// it - and still acking appends, so a dispatch-side barrier never waits on a
/// dead channel.
///
/// `stats` is shared with the world, which reads it for `lev ps`, `lev doctor`
/// and the GraphQL schema, and drains the runs whose journal could not be
/// written so it can fail them.
pub(crate) async fn persistence_worker(
    runs_dir: Option<PathBuf>,
    mut jobs: UnboundedReceiver<PersistMsg>,
    stats: std::sync::Arc<PersistLaneStats>,
) {
    let Some(runs_dir) = runs_dir else {
        while let Some(msg) = jobs.recv().await {
            if let PersistMsg::Append { ack: Some(ack), .. } = msg {
                let _ = ack.send(Appended::NoJournal);
            }
        }
        return;
    };
    let machine_id = load_or_create_machine_id(&runs_dir);
    let world_id = generate_id();
    // The fingerprint of the last context window archived per run, so the next
    // write can be stored as a compact diff rather than a full snapshot. A
    // digest, not the snapshot itself: retaining a full copy of every live
    // run's context doubled the lane's resident cost.
    // What the lane has actually written to each run's `final_output` sidecar,
    // as `(submitted_at, bytes)`. `submit_output` replaces rather than appends,
    // so a new answer always carries a later stamp.
    //
    // This lives here rather than with the sender because the sender cannot
    // know whether a job it built was written: the coalescing below drops
    // superseded snapshots, and a watermark advanced on the dropped one leaves
    // the descriptor in `meta.json` with no sidecar beside it.
    // Here the skip is decided after that, so it reflects what is on disk.
    let mut last_output: std::collections::HashMap<String, (i64, usize)> =
        std::collections::HashMap::new();
    let mut last_context: std::collections::HashMap<String, run_archive::ContextDigest> =
        std::collections::HashMap::new();
    // The status last written into the archive, so a change of status can be
    // recorded as the event it is. `Progress` carries the whole `RunMeta` and
    // therefore the status too, but only as a value that happens to differ
    // from the previous record's - a post-mortem asking "when did this die,
    // and to what" had to diff its way there, and a run whose last act was to
    // go terminal might have no `Progress` after it at all.
    let mut last_status: std::collections::HashMap<String, leviath_core::run_meta::RunStatus> =
        std::collections::HashMap::new();
    // The runs this lane has already written for, so a later write can tell a
    // directory it is about to establish from one somebody has deleted. See
    // [`may_write`].
    let mut staked: HashSet<String> = HashSet::new();
    while let Some(first) = jobs.recv().await {
        // Drain whatever else is already queued and process it as one batch,
        // keeping only the NEWEST snapshot per run: each snapshot carries the
        // whole window, so writing a superseded one is pure disk and memory
        // churn. On a slow disk this is what stops queued full-window
        // snapshots from piling up unboundedly. Appends and stage lines keep
        // their order and are never dropped.
        let mut batch = vec![first];
        while let Ok(msg) = jobs.try_recv() {
            batch.push(msg);
        }
        // What the lane was holding when it last looked. Taken here, once the
        // queue has been drained into this batch, so it counts the work in hand
        // rather than the moment between two messages.
        stats.observe_queue(batch.len());
        let mut newest_snapshot: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for (i, msg) in batch.iter().enumerate() {
            if let PersistMsg::Snapshot(job) = msg {
                newest_snapshot.insert(job.run_id.clone(), i);
            }
        }
        for (i, msg) in batch.into_iter().enumerate() {
            match msg {
                PersistMsg::Snapshot(job) => {
                    if newest_snapshot.get(job.run_id.as_str()) != Some(&i) {
                        continue; // superseded by a newer snapshot in this batch
                    }
                    if !may_write(&runs_dir, &job.run_id, &mut staked, true) {
                        // Deleted. Forget what was cached about it too, so the
                        // maps stay bounded by the runs still being written.
                        last_context.remove(&job.run_id);
                        last_output.remove(&job.run_id);
                        last_status.remove(&job.run_id);
                        continue;
                    }
                    let prev = last_context.get(&job.run_id);
                    let written = last_output.get(&job.run_id).copied();
                    // Record what the write actually put on disk, not what this
                    // job hoped to - deriving the watermark from the job rather
                    // than the write is the shape of the bug being fixed.
                    let status_before = last_status.get(&job.run_id);
                    let outcome = write_snapshot(
                        &runs_dir,
                        &job,
                        &machine_id,
                        &world_id,
                        prev,
                        written,
                        status_before,
                    )
                    .await;
                    // The journal half and the rest of the files are counted
                    // apart because they mean different things: a lost
                    // `meta.json` is put back by the next snapshot, and a lost
                    // journal record is gone for good, which is what fails the
                    // run.
                    if let Some(record) = &outcome.journal {
                        stats.append_attempted();
                        if let Err(lost) = record {
                            stats.journal_append_failed(&job.run_id, &lost.path, &lost.message);
                        }
                    }
                    if let Some(lost) = &outcome.files {
                        stats.snapshot_failed(&job.run_id, &lost.path, &lost.message);
                    }
                    if let Some(key) = outcome.output {
                        last_output.insert(job.run_id.clone(), key);
                    }
                    // Drop a fully-terminal run's cached digest (it won't be
                    // written again), so the map stays bounded by the set of
                    // *live* runs rather than every run the daemon has ever
                    // seen.
                    if outcome.archived() {
                        last_status.insert(job.run_id.clone(), job.meta.status.clone());
                    }
                    if is_terminal_run(&job.meta.status) {
                        last_context.remove(&job.run_id);
                        last_output.remove(&job.run_id);
                        last_status.remove(&job.run_id);
                    } else if outcome.archived() {
                        // Only for a record that reached the file. If the append
                        // failed, the digest stays at the last state a reader
                        // can actually rebuild, so the next write diffs against
                        // that instead - which re-records everything the failed
                        // one carried and puts the archive back in step.
                        last_context.insert(
                            job.run_id.clone(),
                            run_archive::digest_context(&job.context),
                        );
                    }
                }
                PersistMsg::Append {
                    run_id,
                    record,
                    ack,
                } => {
                    let landed = if may_write(&runs_dir, &run_id, &mut staked, false) {
                        stats.append_attempted();
                        append_record(&runs_dir, &run_id, &record, &stats).await
                    } else {
                        Appended::NoJournal
                    };
                    // Ack unconditionally: the dispatch-side barrier must never
                    // stall on a failed append, or on a run that has been
                    // deleted out from under it. What the ack carries is which
                    // of those happened, so the waiter can say so instead of
                    // assuming the record landed.
                    if let Some(ack) = ack {
                        let _ = ack.send(landed);
                    }
                }
                PersistMsg::StageLines {
                    run_id,
                    output_appends,
                    log_appends,
                } => {
                    if !may_write(&runs_dir, &run_id, &mut staked, false) {
                        continue;
                    }
                    let dir = runs_dir.join(&run_id);
                    for (idx, line) in &output_appends {
                        append_stage_line(&dir, *idx, "output.log", line, &run_id).await;
                    }
                    for (idx, line) in &log_appends {
                        append_stage_line(&dir, *idx, "logs.log", line, &run_id).await;
                    }
                }
            }
        }
    }
}

/// Whether the lane should still write for `run_id`, remembering the run the
/// first time a message that can establish it is asked about.
///
/// `establishes` says whether this message is one that creates the run
/// directory. Only a snapshot does; an appended record needs an archive file
/// that is already there, and a stage line needs the directory. So only a
/// snapshot may claim a run the lane has not seen before, and a record that
/// arrives ahead of the first snapshot is dropped rather than counted as the
/// run's arrival. Letting one claim the run meant the snapshot behind it -
/// the write that would have made the directory - found the run already
/// claimed, no directory on disk, and dropped itself as a write to a deleted
/// run. The run then never appeared at all: no `meta.json`, nothing to list,
/// and anyone waiting for it waited for ever.
///
/// A run directory is created **once**, by whoever starts the run: the CLI
/// spawner before the world is built, or - for an embedded world with no CLI
/// in front of it - this lane's own first write. After that, its existence
/// belongs to whoever is looking after the machine. `d` in the dashboard,
/// `DELETE /api/runs/{id}` and an operator with `rm -rf` all say the same
/// thing by removing it.
///
/// So a write that finds the directory gone is not a gap to repair, and it is
/// not a failure either. Repairing it - letting each snapshot run
/// `create_dir_all` first - brings the run back from the dead, meta, context,
/// stage logs, transcript and all, moments after the console said it was
/// deleted; a cancelled run's closing write is enough on its own to do it, which
/// is what makes deleting a stuck run look like it did nothing. Counting it as a
/// fault is the opposite mistake: deleting a run would start failing runs, and
/// the person who deleted it asked for exactly what happened. Both roads lead
/// back here, to a write that is quietly dropped and reported as
/// [`Appended::NoJournal`].
///
/// One `stat` per message, taken inline rather than through the blocking pool:
/// the hop would cost more than the syscall it is avoiding, and the lane is
/// already doing far heavier work per message than this.
fn may_write(
    runs_dir: &Path,
    run_id: &str,
    staked: &mut HashSet<String>,
    establishes: bool,
) -> bool {
    // First snapshot for this run. The directory is normally already there, and
    // creating it is a no-op; the embedded case is the one that needs it made.
    if establishes && staked.insert(run_id.to_string()) {
        return true;
    }
    if runs_dir.join(run_id).is_dir() {
        return true;
    }
    tracing::info!(
        run_id = %run_id,
        "persistence: no run directory, so the run was deleted or has not started yet; dropping this write"
    );
    false
}

/// Create a run directory and any missing parents, owner-only, off the async
/// runtime.
///
/// The run directory is normally staked out by the CLI, which makes it `0o700`.
/// The persistence lane can get there first though (a run reloaded on daemon
/// restart, an embedded world with no CLI in front of it), and
/// `create_dir_all` makes it at the umask default. Everything under a run
/// directory is owner-only, so the directory holding it has to be too - it is
/// what stops another local user walking in.
async fn create_private_dir(path: &Path) -> std::io::Result<()> {
    let owned = path.to_path_buf();
    tokio::task::spawn_blocking(move || leviath_sys::create_private_dir_all(&owned))
        .await
        .map_err(vanished_task)
        .and_then(|r| r)
}

/// A blocking-pool task that never came back: it panicked, or the runtime was
/// shutting down. Named rather than inlined at each call site so the three of
/// them share one branch, and so a test can reach it with a real
/// [`tokio::task::JoinError`] instead of never at all.
fn vanished_task(e: tokio::task::JoinError) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

/// Open a run file for appending, owner-only, off the async runtime.
///
/// `leviath-sys` owns the per-platform mode handling (including the Windows
/// `icacls` path), and it is a blocking API, so the open happens on the blocking
/// pool the same way the atomic writer's does. A failure is handed back to the
/// caller, which decides what it means: a lost journal record fails its run, a
/// lost log line does not.
async fn open_private_append(path: &Path) -> std::io::Result<tokio::fs::File> {
    let owned = path.to_path_buf();
    tokio::task::spawn_blocking(move || leviath_sys::open_private_append(&owned))
        .await
        .map_err(vanished_task)
        .and_then(|r| r)
        .map(tokio::fs::File::from_std)
}

/// Append a single record to an *existing* run archive, reporting where it
/// landed. A run whose first snapshot hasn't landed yet has no `run.lvr` (and no
/// preamble/Header), so the append is skipped rather than corrupting the file -
/// the single-worker lane makes that ordering all but impossible in practice,
/// since the spawn tick's snapshot is queued before any batch can dispatch.
///
/// A record that cannot be written after a retry is counted in `stats`, which
/// also names the run so the world can fail it. **A run deleted from under the
/// lane is not that.** Its directory going away is somebody saying they want it
/// gone, and every write for it after that is a no-op by design - so the failure
/// is classified against the run directory rather than against the error, and a
/// delete that lands between the check and the open reads as
/// [`Appended::NoJournal`] like any other delete.
///
/// The position is the archive's length before the write, which is where this
/// record's frame begins. One writer per run is what makes that true, and the
/// lane is that writer.
async fn append_record(
    runs_dir: &Path,
    run_id: &str,
    record: &leviath_core::run_archive::RunRecord,
    stats: &PersistLaneStats,
) -> Appended {
    let run_dir = runs_dir.join(run_id);
    let path = run_dir.join(leviath_core::files::ARCHIVE_FILE);
    let Ok(existing) = tokio::fs::metadata(&path).await else {
        tracing::warn!(run_id = %run_id, "persistence: record append skipped, no archive yet");
        return Appended::NoJournal;
    };
    let position = existing.len();
    let mut buf: Vec<u8> = Vec::new();
    leviath_core::run_archive::write_record(&mut buf, record)
        .expect("writing to a Vec never fails");
    // Opening, writing and flushing are one outcome to everybody upstream: the
    // record either reached the file or it did not, and which of the three calls
    // failed belongs in the log rather than in the answer.
    let Err(lost) = append_with_retry(&path, &buf, run_id).await else {
        return Appended::Landed { position };
    };
    if !run_dir.is_dir() {
        tracing::info!(
            run_id = %run_id,
            "persistence: the run was deleted while its record was being written; dropping it"
        );
        return Appended::NoJournal;
    }
    stats.journal_append_failed(run_id, &lost.path, &lost.message);
    Appended::Failed
}

/// Append `buf` to `path`, flushing before it returns.
async fn append_bytes(path: &Path, buf: &[u8]) -> std::io::Result<()> {
    let mut file = open_private_append(path).await?;
    // Both results, one answer: a flush that fails is the one to report, since
    // it is the one that says the bytes are not on disk.
    let written = file.write_all(buf).await;
    file.flush().await.and(written)
}

/// Whether a run status is fully terminal (no further snapshots expected).
/// `CompleteInteractive` is excluded - such an agent stays live for follow-up.
fn is_terminal_run(status: &leviath_core::run_meta::RunStatus) -> bool {
    use leviath_core::run_meta::RunStatus;
    matches!(
        status,
        leviath_core::run_meta::RunStatus::Complete | RunStatus::Error | RunStatus::Cancelled
    )
}

/// A short opaque id derived from the current time + pid. Not cryptographic - it
/// only needs to distinguish concurrent daemons/runs on a shared filesystem.
fn generate_id() -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::time::SystemTime::now().hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// This machine's stable id, persisted at `<runs_dir>/../machine-id` (next to the
/// leviath home) and created once. Falls back to a fresh (unpersisted) id if the
/// file can't be written.
fn load_or_create_machine_id(runs_dir: &Path) -> String {
    let path = runs_dir.parent().unwrap_or(runs_dir).join("machine-id");
    let existing = std::fs::read_to_string(&path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    match existing {
        Some(id) => id,
        None => {
            let id = generate_id();
            let _ = std::fs::write(&path, &id);
            id
        }
    }
}

/// One write the lane attempted and lost, with what a person needs to act on it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Lost {
    /// The file that could not be written.
    path: PathBuf,
    /// What the operating system said about it.
    message: String,
}

impl Lost {
    /// Note `error` against `path`, for a caller collecting what a write lost.
    fn at(path: &Path, error: &std::io::Error) -> Self {
        Self {
            path: path.to_path_buf(),
            message: error.to_string(),
        }
    }
}

/// Keep the first loss of a write, so what is reported is the file that failed
/// first rather than the last one tried.
fn keep_first(slot: &mut Option<Lost>, lost: Lost) {
    if slot.is_none() {
        *slot = Some(lost);
    }
}

/// What one snapshot write actually managed to put on disk.
///
/// The three answers are independent and each matters to somebody. `output` is
/// the sidecar watermark the lane remembers; `journal` is what the run's
/// append-only history got out of this write; `files` is whatever else the write
/// could not place.
#[derive(Default)]
struct WriteOutcome {
    /// The `final_output` sidecar this write landed, if it wrote one.
    output: Option<(i64, usize)>,
    /// What became of the run-archive record for this snapshot. `None` means the
    /// write never got as far as trying, because the run directory could not be
    /// made.
    ///
    /// The lane keeps a digest of the last archived context so the next write
    /// can be a compact diff. Advancing that digest for a record that never
    /// landed is unrecoverable: every later diff is then relative to a state no
    /// reader can reconstruct, so the folded archive drifts from the run for
    /// the rest of its life - while `context.json`, written whole each time,
    /// stays correct. One unreported append error is enough.
    journal: Option<Result<(), Lost>>,
    /// The first of this write's other files it could not place: `meta.json`,
    /// `context.json`, the stage index, the answer sidecar, or the waiting
    /// state beside them. Counted, not fatal - the next snapshot rewrites every
    /// one of them whole.
    files: Option<Lost>,
}

impl WriteOutcome {
    /// Whether the journal record for this snapshot is durably on disk.
    fn archived(&self) -> bool {
        matches!(self.journal, Some(Ok(())))
    }
}

/// Write one job's `meta.json` + `context.json` under `<runs_dir>/<run_id>/`,
/// each via a temp file + atomic rename, and report what reached disk.
///
/// Serialization is infallible for these plain serde structs, so a serialize
/// error is a bug rather than a runtime condition (`.expect`).
async fn write_snapshot(
    runs_dir: &Path,
    job: &PersistJob,
    machine_id: &str,
    world_id: &str,
    prev_context: Option<&run_archive::ContextDigest>,
    written_output: Option<(i64, usize)>,
    status_before: Option<&leviath_core::run_meta::RunStatus>,
) -> WriteOutcome {
    let mut files = None;
    let dir = runs_dir.join(&job.run_id);
    if let Err(e) = create_private_dir(&dir).await {
        tracing::warn!(run_id = %job.run_id, error = %e, "persistence: create run dir failed");
        // Reported as a lost snapshot rather than a lost journal record, even
        // though the record is lost with it: a directory that will not be made
        // is also the shape a run deleted between the check and the write
        // leaves, and a delete must never fail a run.
        return WriteOutcome {
            files: Some(Lost::at(&dir, &e)),
            ..WriteOutcome::default()
        };
    }
    let journal =
        append_run_archive(&dir, job, machine_id, world_id, prev_context, status_before).await;
    let meta_json = serde_json::to_string_pretty(&job.meta).expect("RunMeta always serializes");
    write_bytes_atomic(
        &dir.join(leviath_core::files::META_FILE),
        meta_json.into_bytes(),
        &job.run_id,
        &mut files,
    )
    .await;
    // Compact, not pretty: the context is the largest file the lane writes and
    // every consumer parses it; pretty-printing only inflated the write (and
    // its transient allocation) by half.
    let ctx_json = serde_json::to_string(&job.context).expect("ContextSnapshot always serializes");
    write_bytes_atomic(
        &dir.join(leviath_core::files::CONTEXT_FILE),
        ctx_json.into_bytes(),
        &job.run_id,
        &mut files,
    )
    .await;

    // The answer's bytes, beside the descriptor `meta.json` carries. Written
    // raw, so serving it is a read and `lev result --raw` is a copy.
    //
    // Skipped only when this lane has already written *this* answer, so a
    // heartbeat does not rewrite an unchanged quarter-megabyte file every
    // thirty seconds. The check is here rather than at the sender because only
    // the lane knows a job survived coalescing to be written at all - deciding
    // it earlier is what leaves descriptors without sidecars.
    let submitted = job
        .meta
        .final_output
        .as_ref()
        .map(|d| (d.submitted_at, d.bytes));
    // `submitted.is_none()` writes unconditionally: with no descriptor there is
    // nothing to identify the answer by, and skipping on an unidentifiable key
    // is how the sidecar goes missing in the first place.
    let mut wrote_output = None;
    if let Some(content) = &job.final_output
        && (submitted.is_none() || written_output != submitted)
    {
        write_bytes_atomic(
            &dir.join(leviath_core::FINAL_OUTPUT_FILE),
            content.clone().into_bytes(),
            &job.run_id,
            &mut files,
        )
        .await;
        wrote_output = submitted;
    }

    // Per-stage index (names/status), rewritten whole; empty ⇒ agent has no ledger.
    if !job.stages.is_empty() {
        let stages_json =
            serde_json::to_string_pretty(&job.stages).expect("StageRecord slice always serializes");
        write_bytes_atomic(
            &dir.join(leviath_core::files::STAGES_FILE),
            stages_json.into_bytes(),
            &job.run_id,
            &mut files,
        )
        .await;
    }
    // Append-only per-stage output + logs.
    for (idx, line) in &job.output_appends {
        append_stage_line(&dir, *idx, "output.log", line, &job.run_id).await;
    }
    for (idx, line) in &job.log_appends {
        append_stage_line(&dir, *idx, "logs.log", line, &job.run_id).await;
    }
    // Per-stage taint audit (whole-file, atomic).
    if let Some((idx, json)) = &job.taint_audit {
        let stage_dir = dir.join("stages").join(idx.to_string());
        let _ = create_private_dir(&stage_dir).await;
        write_bytes_atomic(
            &stage_dir.join("taint_audit.json"),
            json.clone().into_bytes(),
            &job.run_id,
            &mut files,
        )
        .await;
    }
    // Fan-out waiting state (whole-file), or remove any stale file once the
    // parent is no longer parked on a fan-out.
    let fanout_path = dir.join(leviath_core::files::FANOUT_FILE);
    match &job.fanout {
        Some(json) => {
            write_bytes_atomic(
                &fanout_path,
                json.clone().into_bytes(),
                &job.run_id,
                &mut files,
            )
            .await
        }
        None => {
            let _ = tokio::fs::remove_file(&fanout_path).await;
        }
    }
    // Interaction-point waiting state (whole-file), or remove any stale file once the
    // agent is no longer parked at a stage-boundary interaction point.
    let interactions_path = dir.join(leviath_core::files::INTERACTIONS_FILE);
    match &job.interactions {
        Some(json) => {
            write_bytes_atomic(
                &interactions_path,
                json.clone().into_bytes(),
                &job.run_id,
                &mut files,
            )
            .await
        }
        None => {
            let _ = tokio::fs::remove_file(&interactions_path).await;
        }
    }
    WriteOutcome {
        output: wrote_output,
        journal: Some(journal),
        files,
    }
}

/// Append this snapshot to the run's portable archive (`<run_dir>/run.lvr`).
///
/// The context window (the bulk) is stored as a diff between writes:
/// - **new archive** (file absent): preamble + a `Header` (identity + metadata)
///   + a full `ContextCheckpoint` the diffs rebase on;
/// - **resumed** (file present, but this worker has no prior context for the
///   run, e.g. after a daemon restart): an `OwnershipChanged` recording this
///   world/machine took over + a fresh `ContextCheckpoint` re-anchor;
/// - **ongoing** (a prior context is known): a compact `Progress` step carrying
///   the updated metadata + a `ContextDiff` since the previous point.
///
/// The archive always folds to the run's latest resumable state. A failed write
/// is logged and reported back rather than raised: the lane finishes the rest of
/// the snapshot, counts the loss, and the world fails the run on its next tick.
async fn append_run_archive(
    dir: &Path,
    job: &PersistJob,
    machine_id: &str,
    world_id: &str,
    prev_context: Option<&run_archive::ContextDigest>,
    status_before: Option<&leviath_core::run_meta::RunStatus>,
) -> Result<(), Lost> {
    use leviath_core::run_archive::{RunIdentity, RunRecord};

    let path = dir.join(leviath_core::files::ARCHIVE_FILE);
    let file_exists = tokio::fs::try_exists(&path).await.unwrap_or(false);
    let at = job.meta.updated_at;

    // Encode into a buffer via the sync codec, then append in one async write.
    let mut buf: Vec<u8> = Vec::new();
    match prev_context {
        // A known prior context → the compact per-step record.
        Some(prev) => {
            let progress = RunRecord::Progress {
                meta: Box::new(job.meta.clone()),
                delta: run_archive::diff_context_digest(prev, &job.context),
                at,
            };
            run_archive::write_record(&mut buf, &progress).expect("writing to a Vec never fails");
        }
        // No prior context this process.
        None => {
            if file_exists {
                // Resumed run: record the ownership handoff to this world.
                let owned = RunRecord::OwnershipChanged {
                    machine_id: machine_id.to_string(),
                    world_id: world_id.to_string(),
                    at,
                };
                run_archive::write_record(&mut buf, &owned).expect("writing to a Vec never fails");
            } else {
                // Brand-new archive: preamble + Header.
                run_archive::write_archive_start(&mut buf, run_archive::RUN_ARCHIVE_VERSION)
                    .expect("writing to a Vec never fails");
                let header = RunRecord::Header {
                    identity: RunIdentity {
                        run_id: job.run_id.clone(),
                        machine_id: machine_id.to_string(),
                        world_id: world_id.to_string(),
                        created_at: job.meta.started_at,
                    },
                    meta: Box::new(job.meta.clone()),
                };
                run_archive::write_record(&mut buf, &header).expect("writing to a Vec never fails");
            }
            // Either way, anchor with a full context snapshot the diffs rebase on.
            let checkpoint = RunRecord::ContextCheckpoint {
                snapshot: job.context.clone(),
                at,
            };
            run_archive::write_record(&mut buf, &checkpoint).expect("writing to a Vec never fails");
        }
    }

    // Beside whatever else this write records: a *change* of status is an
    // event in its own right, and the one a post-mortem looks for first.
    //
    // Only a change. The first write has no previous status to differ from,
    // and the `Header` it goes out with already carries the run's opening
    // state - a `StatusChanged` there would say the same thing twice.
    if status_before.is_some_and(|before| before != &job.meta.status) {
        let changed = RunRecord::StatusChanged {
            status: job.meta.status.clone(),
            at,
        };
        run_archive::write_record(&mut buf, &changed).expect("writing to a Vec never fails");
    }

    // Open and write share one outcome, because they share one meaning: either
    // the record is on disk or it is not. A partial write counts as failure -
    // it leaves a torn frame the lenient reader stops at, so the tail is lost
    // either way.
    // Retried once before it counts as lost, for the same reason the record
    // matters: a write that failed on a momentarily unavailable disk is worth
    // asking again about, and a second failure is a real answer rather than a
    // flake.
    append_with_retry(&path, &buf, &job.run_id).await
}

/// How long to wait before asking the disk a second time.
///
/// Short enough that the lane is not held up - it is one pause, on the one
/// message that already failed - and long enough to be past a momentary refusal
/// rather than inside the same instant as it.
const APPEND_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(50);

/// Append `buf` to `path`, and if that fails, once more.
///
/// The retry is the difference between a journal that gives up at the first
/// hiccup and one that gives up when the disk really will not take the record.
/// Only the second failure is reported, because only the second is worth failing
/// a run over.
async fn append_with_retry(path: &Path, buf: &[u8], run_id: &str) -> Result<(), Lost> {
    retrying(one_append, path, buf, APPEND_RETRY_DELAY, run_id).await
}

/// One attempt at an append: the call that puts `buf` on the end of `path`.
///
/// A function pointer rather than a generic parameter, so the loop below is
/// compiled exactly once. A generic one is compiled per caller, and the lane's
/// own copy would carry a path - the attempt that works the second time - that
/// only a test's copy ever takes.
type AppendAttempt = for<'a> fn(
    &'a Path,
    &'a [u8],
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = std::io::Result<()>> + Send + 'a>,
>;

/// [`append_bytes`] as an [`AppendAttempt`].
fn one_append<'a>(
    path: &'a Path,
    buf: &'a [u8],
) -> std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<()>> + Send + 'a>> {
    Box::pin(append_bytes(path, buf))
}

/// Make `attempt`, and after `delay` make it once more if it failed.
///
/// The whole append is retried, not the failed syscall within it: a write that
/// stopped half way leaves a torn frame, and the archive reader stops at the
/// first torn frame either way, so asking again can only recover a record that
/// would otherwise be missing. Taking the attempt as a pointer is what lets a
/// test drive both answers without a disk that has to fail on cue.
async fn retrying(
    attempt: AppendAttempt,
    path: &Path,
    buf: &[u8],
    delay: std::time::Duration,
    run_id: &str,
) -> Result<(), Lost> {
    let Err(first) = attempt(path, buf).await else {
        return Ok(());
    };
    tracing::warn!(
        run_id = %run_id,
        error = %first,
        "persistence: journal append failed, asking again"
    );
    tokio::time::sleep(delay).await;
    let Err(second) = attempt(path, buf).await else {
        return Ok(());
    };
    tracing::warn!(
        run_id = %run_id,
        path = %path.display(),
        error = %second,
        "persistence: journal append failed twice; the run is failed for it"
    );
    Err(Lost::at(path, &second))
}

/// Append one line (with a trailing newline) to `stages/<idx>/<file>` under the
/// run dir, creating the stage directory if needed.
///
/// A failed `create_dir_all` just makes the subsequent open fail, and the append
/// result is deliberately not reported: these are the readable logs beside the
/// run, not the record of what it did, and a line that does not reach `logs.log`
/// leaves nothing claiming otherwise. The journal is what a run is failed for.
async fn append_stage_line(run_dir: &Path, stage_idx: usize, file: &str, line: &str, run_id: &str) {
    let stage_dir = run_dir.join("stages").join(stage_idx.to_string());
    let _ = create_private_dir(&stage_dir).await;
    match open_private_append(&stage_dir.join(file)).await {
        Ok(mut handle) => {
            let mut bytes = line.as_bytes().to_vec();
            bytes.push(b'\n');
            let _ = handle.write_all(&bytes).await;
            // tokio::fs::File buffers; flush so a reader (dashboard / a sync test)
            // sees the line before the handle is dropped.
            let _ = handle.flush().await;
        }
        Err(e) => {
            tracing::warn!(run_id = %run_id, error = %e, "persistence: stage log open failed");
        }
    }
}

/// Write `bytes` to `path` via a sibling temp file + rename (atomic on the same
/// filesystem, so a reader never sees a half-written file).
///
/// A failure is logged and left in `lost` for the caller to count. The lane
/// carries on with the rest of the snapshot either way: every one of these files
/// is rewritten whole the next time the run changes, so one lost write costs the
/// freshness of a file rather than its contents.
async fn write_bytes_atomic(path: &Path, bytes: Vec<u8>, run_id: &str, lost: &mut Option<Lost>) {
    let tmp = path.with_extension("json.tmp");
    // `write_private`, not a plain write: these files carry the run's task
    // prompt, its conversation, its tool output, and - in `meta.json` - the
    // webhook signing secret. A plain write lands them at the umask default,
    // usually 0644, leaving the 0700 on the enclosing run directory as the only
    // thing between them and any other user. The CLI's writer was fixed for
    // exactly this reason; the daemon's, which is what actually writes these
    // files during a run, was missed.
    //
    // Blocking, so it goes to a blocking thread rather than stalling the
    // persistence lane. The alternative is a per-platform mode dance here, and
    // `leviath-sys` already owns that (including the Windows `icacls` path).
    // Owned bytes in, so a multi-hundred-KB context serialization is written
    // from the one buffer it was serialized into instead of being copied again.
    let tmp_for_write = tmp.clone();
    let written =
        tokio::task::spawn_blocking(move || leviath_sys::write_private(&tmp_for_write, &bytes))
            .await;
    if let Err(e) = written.map_err(vanished_task).and_then(|r| r) {
        tracing::warn!(run_id = %run_id, error = %e, "persistence: temp write failed");
        keep_first(lost, Lost::at(&tmp, &e));
        return;
    }
    if let Err(e) = tokio::fs::rename(&tmp, path).await {
        tracing::warn!(run_id = %run_id, error = %e, "persistence: rename failed");
        keep_first(lost, Lost::at(path, &e));
        let _ = tokio::fs::remove_file(&tmp).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::run_meta::RunMeta;
    use tokio::sync::mpsc;

    fn meta(run_id: &str) -> RunMeta {
        RunMeta::new(
            run_id.to_string(),
            "agent".to_string(),
            "/path".to_string(),
            "task".to_string(),
            None,
            "/work".to_string(),
            1,
        )
    }

    fn context() -> ContextSnapshot {
        ContextSnapshot {
            stage_name: "s".to_string(),
            total_tokens: 0,
            max_tokens: 100,
            regions: vec![],
        }
    }

    /// Fresh counters, for a test that does not read them back.
    fn health() -> std::sync::Arc<PersistLaneStats> {
        std::sync::Arc::new(PersistLaneStats::new())
    }

    /// The whole loop, end to end: two snapshots for one run in a single batch,
    /// both describing and carrying the answer.
    ///
    /// The first is dropped as superseded, and that coalescing loses the bytes
    /// if the sender has already marked them sent. The surviving snapshot must
    /// still produce the sidecar, and the second batch (a heartbeat re-sending
    /// the same answer) must not rewrite it.
    #[tokio::test]
    async fn worker_writes_the_sidecar_even_when_the_first_snapshot_is_coalesced_away() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::unbounded_channel();

        let answered = |body: &str| {
            let mut m = meta("run-fast");
            m.final_output = Some(leviath_core::output::FinalOutputDescriptor {
                format: None,
                stage: "out".to_string(),
                submitted_at: 100,
                bytes: body.len(),
                truncated: false,
                artifacts: Vec::new(),
            });
            Box::new(PersistJob {
                run_id: "run-fast".to_string(),
                meta: m,
                context: context(),
                stages: vec![],
                output_appends: vec![],
                log_appends: vec![],
                taint_audit: None,
                fanout: None,
                interactions: None,
                final_output: Some(body.to_string()),
            })
        };

        // Both land before the worker drains, so the first is superseded.
        tx.send(PersistMsg::Snapshot(answered("the answer")))
            .unwrap();
        tx.send(PersistMsg::Snapshot(answered("the answer")))
            .unwrap();
        drop(tx);
        persistence_worker(Some(dir.path().to_path_buf()), rx, health()).await;

        let sidecar = dir
            .path()
            .join("run-fast")
            .join(leviath_core::FINAL_OUTPUT_FILE);
        assert_eq!(
            std::fs::read_to_string(&sidecar).expect("sidecar written"),
            "the answer",
            "a coalesced-away first snapshot must not lose the answer"
        );
        // Both halves, because `read_final_output` needs both and a test that
        // checked one would pass on the bug.
        let back: RunMeta = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("run-fast").join("meta.json")).unwrap(),
        )
        .unwrap();
        assert!(back.final_output.is_some(), "descriptor written too");
    }

    #[tokio::test]
    async fn worker_writes_meta_and_context_then_exits_on_close() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(PersistMsg::Snapshot(Box::new(PersistJob {
            run_id: "run-1".to_string(),
            meta: meta("run-1"),
            context: context(),
            stages: vec![],
            output_appends: vec![],
            log_appends: vec![],
            taint_audit: None,
            fanout: None,
            interactions: None,
            final_output: None,
        })))
        .unwrap();
        drop(tx); // close so the worker loop ends

        persistence_worker(Some(dir.path().to_path_buf()), rx, health()).await;

        let run_dir = dir.path().join("run-1");
        let meta_json = std::fs::read_to_string(run_dir.join("meta.json")).unwrap();
        let back: RunMeta = serde_json::from_str(&meta_json).unwrap();
        assert_eq!(back.run_id, "run-1");
        assert!(run_dir.join("context.json").exists());
        // No temp files left behind.
        assert!(!run_dir.join("meta.json.tmp").exists());
    }

    fn job(run_id: &str) -> PersistJob {
        PersistJob {
            run_id: run_id.to_string(),
            meta: meta(run_id),
            context: context(),
            stages: vec![],
            output_appends: vec![],
            log_appends: vec![],
            taint_audit: None,
            fanout: None,
            interactions: None,
            final_output: None,
        }
    }

    /// A job whose context window has one region with the given entries (for
    /// exercising context diffs across writes).
    fn job_with_context(run_id: &str, entries: usize) -> PersistJob {
        let ctx = ContextSnapshot {
            stage_name: "s".to_string(),
            total_tokens: entries,
            max_tokens: 100,
            regions: vec![leviath_core::run_meta::RegionSnapshot {
                name: "conv".to_string(),
                kind: "clearable".to_string(),
                current_tokens: entries,
                max_tokens: 100,
                entries: (0..entries)
                    .map(|i| leviath_core::run_meta::RegionEntrySnapshot {
                        content: format!("line {i}").into(),
                        tokens: 1,
                        kind: leviath_core::region::EntryKind::Text,
                        metadata: None,
                        key: None,
                        taint: Default::default(),
                        reasoning: None,
                    })
                    .collect(),
                description: None,
            }],
        };
        PersistJob {
            context: ctx,
            ..job(run_id)
        }
    }

    /// Every file the persistence lane writes is private to this user.
    ///
    /// These carry the run's task prompt, its conversation, its tool output, and
    /// - in `meta.json` - the webhook signing secret. A plain write lands them
    /// at the umask default, usually 0644, leaving the 0700 on the run directory
    /// as the only thing between them and another user. Defence in depth is the
    /// point of a mode on the file itself.
    ///
    /// The assertion is Unix-only because that is where modes exist; the write
    /// path itself runs on every platform, and on Windows `leviath-sys` applies
    /// the equivalent ACL.
    #[tokio::test]
    async fn every_persisted_run_file_is_private_to_this_user() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut j = job("run-perms");
        j.final_output = Some("the answer".to_string());
        write_snapshot(dir.path(), &j, "m", "w", None, None, None).await;

        let run_dir = dir.path().join("run-perms");
        for name in ["meta.json", "context.json", leviath_core::FINAL_OUTPUT_FILE] {
            let path = run_dir.join(name);
            assert!(path.exists(), "{name} was written");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&path)
                    .expect("written file")
                    .permissions()
                    .mode()
                    & 0o777;
                assert_eq!(mode, 0o600, "{name} should be private, got {mode:o}");
            }
        }
    }

    /// The sidecar holds the answer's bytes verbatim, with no wrapper, so
    /// serving it is a read.
    #[tokio::test]
    async fn the_final_output_sidecar_holds_the_answer_verbatim() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut j = job("run-answer");
        j.final_output = Some("metric,value\nrows,2\n".to_string());
        write_snapshot(dir.path(), &j, "m", "w", None, None, None).await;

        let written = std::fs::read_to_string(
            dir.path()
                .join("run-answer")
                .join(leviath_core::FINAL_OUTPUT_FILE),
        )
        .expect("sidecar written");
        assert_eq!(written, "metric,value\nrows,2\n");
    }

    /// A descriptor must never reach `meta.json` without its sidecar landing
    /// too.
    ///
    /// The hazard is not in either write, it is in deciding *earlier* than the
    /// write whether the bytes are needed. A sender that advances a watermark
    /// when it builds the job loses them when the lane drops that job as
    /// superseded and writes a later one, which describes the answer and
    /// carries nothing. Here
    /// the second job is written with `written_output: None` - the honest state
    /// after the first was dropped - and must still produce the sidecar.
    #[tokio::test]
    async fn a_described_answer_is_written_even_after_an_earlier_job_was_dropped() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut j = job("run-coalesced");
        j.final_output = Some("the answer".to_string());
        j.meta.final_output = Some(leviath_core::output::FinalOutputDescriptor {
            format: None,
            stage: "out".to_string(),
            submitted_at: 100,
            bytes: "the answer".len(),
            truncated: false,
            artifacts: Vec::new(),
        });

        // `None` is what the lane knows after the job that would have written
        // the bytes was coalesced away: nothing has been written for this run.
        write_snapshot(dir.path(), &j, "m", "w", None, None, None).await;

        let sidecar = dir
            .path()
            .join("run-coalesced")
            .join(leviath_core::FINAL_OUTPUT_FILE);
        assert_eq!(
            std::fs::read_to_string(&sidecar).expect("sidecar written"),
            "the answer"
        );
        // The pairing, asserted as a pair: `read_final_output` needs both, so a
        // test that checked only the descriptor would pass on the bug.
        let meta: leviath_core::run_meta::RunMeta = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("run-coalesced").join("meta.json"))
                .expect("meta written"),
        )
        .expect("meta parses");
        assert!(meta.final_output.is_some(), "descriptor present");
    }

    /// The optimisation the watermark existed for, now decided where it is
    /// true: an answer this lane has already written is not rewritten, so a
    /// heartbeat does not rewrite a large file every thirty seconds.
    #[tokio::test]
    async fn an_answer_already_written_by_this_lane_is_not_rewritten() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut j = job("run-heartbeat");
        j.final_output = Some("the answer".to_string());
        j.meta.final_output = Some(leviath_core::output::FinalOutputDescriptor {
            format: None,
            stage: "out".to_string(),
            submitted_at: 100,
            bytes: "the answer".len(),
            truncated: false,
            artifacts: Vec::new(),
        });
        let sidecar = dir
            .path()
            .join("run-heartbeat")
            .join(leviath_core::FINAL_OUTPUT_FILE);

        write_snapshot(dir.path(), &j, "m", "w", None, None, None).await;
        assert!(sidecar.exists(), "first write lands");
        // Replace it with a marker: if the skip does not hold, the marker is
        // overwritten, which is a difference a mere "file exists" cannot see.
        std::fs::write(&sidecar, "MARKER").expect("marker");

        write_snapshot(
            dir.path(),
            &j,
            "m",
            "w",
            None,
            Some((100, "the answer".len())),
            None,
        )
        .await;
        assert_eq!(
            std::fs::read_to_string(&sidecar).expect("still there"),
            "MARKER",
            "an answer already on disk is not rewritten"
        );

        // A new submission (later stamp) is written again.
        j.meta
            .final_output
            .as_mut()
            .expect("descriptor")
            .submitted_at = 200;
        write_snapshot(
            dir.path(),
            &j,
            "m",
            "w",
            None,
            Some((100, "the answer".len())),
            None,
        )
        .await;
        assert_eq!(
            std::fs::read_to_string(&sidecar).expect("rewritten"),
            "the answer",
            "a newer submission replaces it"
        );
    }

    /// A job with no answer writes no sidecar, rather than an empty file that
    /// would read as "the agent answered with nothing".
    #[tokio::test]
    async fn no_answer_writes_no_sidecar() {
        let dir = tempfile::tempdir().expect("temp dir");
        write_snapshot(dir.path(), &job("run-silent"), "m", "w", None, None, None).await;
        assert!(
            !dir.path()
                .join("run-silent")
                .join(leviath_core::FINAL_OUTPUT_FILE)
                .exists()
        );
    }

    /// The invariant, over everything a run writes rather than the one file that
    /// happened to be looked at. `run.lvr` held every context snapshot, the whole
    /// conversation and every tool result, and was created at the umask default
    /// while the far smaller answer sidecar beside it was owner-only.
    ///
    /// The containing run directory is `0o700`, so these were not reachable in
    /// place. A directory's mode does not survive a copy though: `tar`, `rsync`
    /// or a backup tool preserves the per-file mode and drops the protection the
    /// directory was providing.
    #[tokio::test]
    async fn every_file_a_run_writes_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("run-perms");

        write_snapshot(dir.path(), &job("run-perms"), "m", "w", None, None, None).await;
        append_stage_line(&run, 0, "output.log", "a line of agent output", "run-perms").await;
        append_stage_line(&run, 0, "logs.log", "a line of tool activity", "run-perms").await;
        append_record(
            dir.path(),
            "run-perms",
            &leviath_core::run_archive::RunRecord::OwnershipChanged {
                machine_id: "m2".to_string(),
                world_id: "w2".to_string(),
                at: 1,
            },
            &health(),
        )
        .await;

        let walked = walkdir(&run);

        // Asserted on every platform: a walk that found nothing would pass the
        // mode check below vacuously, and on Windows there are no mode bits to
        // check at all. Naming the files also pins *what* this test covers, so
        // a new writer added to the lane and left out of it is visible here.
        for name in ["meta.json", "run.lvr", "stages"] {
            assert!(
                walked.iter().any(|p| p.ends_with(name)),
                "the lane did not write {name}: {walked:?}"
            );
        }
        assert!(
            walked.iter().any(|p| p.ends_with("output.log")),
            "the stage logs are missing: {walked:?}"
        );

        #[cfg(unix)]
        for entry in &walked {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(entry).unwrap().permissions().mode() & 0o777;
            let expected = if entry.is_dir() { 0o700 } else { 0o600 };
            let shown = entry.display().to_string();
            assert_eq!(
                mode, expected,
                "{shown} is {mode:o}, and a copy of this tree would carry that"
            );
        }
    }

    /// The three writers all hand their blocking work to the pool, and a task
    /// that panics there comes back as a `JoinError` rather than as the io error
    /// the caller is written against. Reached with a real one, because
    /// `JoinError` cannot be constructed by hand.
    #[tokio::test]
    async fn a_vanished_blocking_task_becomes_an_io_error() {
        let joined = tokio::task::spawn_blocking(|| panic!("the pool task died"))
            .await
            .expect_err("a panicking task joins as an error");

        let mapped = vanished_task(joined);
        assert_eq!(mapped.kind(), std::io::ErrorKind::Other);
        assert!(
            mapped.to_string().contains("panic"),
            "the reason has to survive: {mapped}"
        );
    }

    /// Every file and directory under `root`, `root` itself included.
    fn walkdir(root: &Path) -> Vec<PathBuf> {
        let mut found = vec![root.to_path_buf()];
        let mut queue = vec![root.to_path_buf()];
        while let Some(dir) = queue.pop() {
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    queue.push(path.clone());
                }
                found.push(path);
            }
        }
        found
    }

    #[tokio::test]
    async fn write_snapshot_creates_a_readable_run_archive() {
        use leviath_core::run_archive::{RunRecord, fold, read_archive};
        let dir = tempfile::tempdir().unwrap();
        write_snapshot(
            dir.path(),
            &job("run-1"),
            "machine-x",
            "world-y",
            None,
            None,
            None,
        )
        .await;

        let bytes = std::fs::read(dir.path().join("run-1").join("run.lvr")).unwrap();
        let (version, records) = read_archive(&mut bytes.as_slice()).unwrap();
        assert_eq!(version, leviath_core::run_archive::RUN_ARCHIVE_VERSION);
        // A brand-new archive: a Header then a full ContextCheckpoint.
        assert!(
            records
                .iter()
                .any(|r| matches!(r, RunRecord::Header { .. }))
        );
        assert!(
            records
                .iter()
                .any(|r| matches!(r, RunRecord::ContextCheckpoint { .. }))
        );
        // The archive folds to the run's identity + state.
        let folded = fold(&records).unwrap();
        assert_eq!(folded.identity.run_id, "run-1");
        assert_eq!(folded.identity.machine_id, "machine-x");
        assert_eq!(folded.identity.world_id, "world-y");
        assert_eq!(folded.meta.run_id, "run-1");
    }

    #[tokio::test]
    async fn run_archive_stores_subsequent_writes_as_progress_diffs() {
        use leviath_core::run_archive::{RunRecord, read_archive, replay_points};
        let dir = tempfile::tempdir().unwrap();
        let first = job_with_context("run-1", 1);
        let second = job_with_context("run-1", 3); // grew by 2 entries
        write_snapshot(dir.path(), &first, "m", "w", None, None, None).await;
        write_snapshot(
            dir.path(),
            &second,
            "m",
            "w",
            Some(&run_archive::digest_context(&first.context)),
            None,
            None,
        )
        .await;

        let bytes = std::fs::read(dir.path().join("run-1").join("run.lvr")).unwrap();
        let (_v, records) = read_archive(&mut bytes.as_slice()).unwrap();
        let count = |pred: fn(&RunRecord) -> bool| records.iter().filter(|r| pred(r)).count();
        assert_eq!(count(|r| matches!(r, RunRecord::Header { .. })), 1);
        assert_eq!(
            count(|r| matches!(r, RunRecord::ContextCheckpoint { .. })),
            1
        );
        assert_eq!(
            count(|r| matches!(r, RunRecord::Progress { .. })),
            1,
            "the second write is a compact Progress diff, not a full checkpoint"
        );
        // Replaying yields the growing window at each point (1 entry → 3).
        let points = replay_points(&records);
        assert_eq!(points.len(), 2);
        assert_eq!(points[0].context.regions[0].entries.len(), 1);
        assert_eq!(points[1].context.regions[0].entries.len(), 3);
    }

    #[tokio::test]
    async fn run_archive_records_ownership_handoff_on_resume() {
        use leviath_core::run_archive::{RunRecord, read_archive};
        let dir = tempfile::tempdir().unwrap();
        // First process writes the archive.
        write_snapshot(dir.path(), &job("run-1"), "m1", "w1", None, None, None).await;
        // A "restarted" worker (no prior context) writes to the existing archive:
        // it records an ownership handoff + a fresh context re-anchor, not a Header.
        write_snapshot(dir.path(), &job("run-1"), "m2", "w2", None, None, None).await;

        let bytes = std::fs::read(dir.path().join("run-1").join("run.lvr")).unwrap();
        let (_v, records) = read_archive(&mut bytes.as_slice()).unwrap();
        assert_eq!(
            records
                .iter()
                .filter(|r| matches!(r, RunRecord::Header { .. }))
                .count(),
            1,
            "no second Header on resume"
        );
        let owned = records
            .iter()
            .find_map(|r| match r {
                RunRecord::OwnershipChanged {
                    machine_id,
                    world_id,
                    ..
                } => Some((machine_id.clone(), world_id.clone())),
                _ => None,
            })
            .expect("ownership handoff recorded");
        assert_eq!(owned, ("m2".to_string(), "w2".to_string()));
    }

    #[test]
    fn is_terminal_run_classifies_statuses() {
        use leviath_core::run_meta::RunStatus;
        assert!(is_terminal_run(
            &leviath_core::run_meta::RunStatus::Complete
        ));
        assert!(is_terminal_run(&RunStatus::Error));
        assert!(is_terminal_run(&RunStatus::Cancelled));
        assert!(!is_terminal_run(
            &leviath_core::run_meta::RunStatus::Running
        ));
        assert!(!is_terminal_run(
            &leviath_core::run_meta::RunStatus::CompleteInteractive
        ));
    }

    /// Snapshots queued behind a slow write are coalesced latest-wins per run:
    /// three queued snapshots produce ONE write carrying the newest context,
    /// not three full-window writes.
    #[tokio::test]
    async fn worker_coalesces_queued_snapshots_to_the_newest_per_run() {
        use leviath_core::run_archive::{RunRecord, read_archive};
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(PersistMsg::Snapshot(Box::new(job_with_context("run-1", 1))))
            .unwrap();
        tx.send(PersistMsg::Snapshot(Box::new(job_with_context("run-1", 2))))
            .unwrap();
        tx.send(PersistMsg::Snapshot(Box::new(job_with_context("run-1", 3))))
            .unwrap();
        // A different run's snapshot is not swallowed by run-1's coalescing.
        tx.send(PersistMsg::Snapshot(Box::new(job_with_context("run-2", 1))))
            .unwrap();
        drop(tx);
        persistence_worker(Some(dir.path().to_path_buf()), rx, health()).await;

        let bytes = std::fs::read(dir.path().join("run-1").join("run.lvr")).unwrap();
        let (_v, records) = read_archive(&mut bytes.as_slice()).unwrap();
        let checkpoints: Vec<_> = records
            .iter()
            .filter_map(|r| match r {
                RunRecord::ContextCheckpoint { snapshot, .. } => Some(snapshot),
                _ => None,
            })
            .collect();
        assert_eq!(checkpoints.len(), 1, "one write for three queued snapshots");
        assert_eq!(
            checkpoints[0].regions[0].entries.len(),
            3,
            "and it carries the NEWEST context"
        );
        // Header + one ContextCheckpoint and nothing else: no superseded
        // snapshot produced a Progress record.
        assert_eq!(records.len(), 2);
        assert!(dir.path().join("run-2").join("meta.json").exists());
    }

    /// `StageLines` appends output/log lines without rewriting `meta.json` or
    /// `context.json` - the whole point of the lines-only fast path.
    #[tokio::test]
    async fn worker_appends_stage_lines_without_a_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        // Establish the run dir with one snapshot first (like the spawn tick).
        tx.send(PersistMsg::Snapshot(Box::new(job("run-1"))))
            .unwrap();
        tx.send(PersistMsg::StageLines {
            run_id: "run-1".to_string(),
            output_appends: vec![(0, "an output line".to_string())],
            log_appends: vec![(0, "[tool] shell: ls".to_string())],
        })
        .unwrap();
        drop(tx);

        let meta_before = {
            // The worker hasn't run yet; nothing exists.
            !dir.path().join("run-1").join("meta.json").exists()
        };
        assert!(meta_before);
        persistence_worker(Some(dir.path().to_path_buf()), rx, health()).await;

        let run = dir.path().join("run-1");
        let out = std::fs::read_to_string(run.join("stages/0/output.log")).unwrap();
        assert!(out.contains("an output line"));
        let log = std::fs::read_to_string(run.join("stages/0/logs.log")).unwrap();
        assert!(log.contains("[tool] shell: ls"));
        // meta.json exists from the snapshot, and was not rewritten by the
        // lines message (same content as the snapshot wrote).
        let meta: RunMeta =
            serde_json::from_str(&std::fs::read_to_string(run.join("meta.json")).unwrap()).unwrap();
        assert_eq!(meta.run_id, "run-1");
    }

    #[tokio::test]
    async fn worker_drops_terminal_runs_from_the_context_cache() {
        // A terminal job exercises the cache-cleanup branch; the run is still
        // written. (Non-terminal jobs exercise the insert branch elsewhere.)
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        let mut terminal = job("run-term");
        terminal.meta.status = leviath_core::run_meta::RunStatus::Complete;
        tx.send(PersistMsg::Snapshot(Box::new(terminal))).unwrap();
        drop(tx);
        persistence_worker(Some(dir.path().to_path_buf()), rx, health()).await;
        assert!(dir.path().join("run-term").join("meta.json").exists());
    }

    #[tokio::test]
    async fn worker_without_a_runs_dir_drains_messages_and_writes_nothing() {
        // `runs_dir: None` is the in-memory mode: snapshots are received and
        // dropped, appends are still acked (a dispatch-side barrier must not
        // wait on a dead channel), the worker still ends when the channel
        // closes, and the directory a persistent world would have written to
        // stays empty (no run dirs, no machine-id next to it).
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(PersistMsg::Snapshot(Box::new(job("run-a"))))
            .unwrap();
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        tx.send(PersistMsg::Append {
            run_id: "run-a".to_string(),
            record: Box::new(batch_record(0, "c1")),
            ack: Some(ack_tx),
        })
        .unwrap();
        tx.send(PersistMsg::Append {
            run_id: "run-a".to_string(),
            record: Box::new(batch_record(0, "c2")),
            ack: None, // the fire-and-forget shape drains too
        })
        .unwrap();
        drop(tx);
        persistence_worker(None, rx, health()).await;
        // A world with no runs dir has no journal, and the ack says exactly
        // that rather than reporting a record that was never written.
        assert_eq!(ack_rx.await, Ok(Appended::NoJournal));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn write_snapshot_writes_then_removes_fanout_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run-1").join("fanout.json");
        // A job carrying fan-out state writes fanout.json.
        let mut fo_job = job("run-1");
        fo_job.fanout = Some(r#"{"resume":"me"}"#.to_string());
        write_snapshot(dir.path(), &fo_job, "m", "w", None, None, None).await;
        assert!(path.exists());
        // A later job without fan-out state removes the now-stale file.
        write_snapshot(
            dir.path(),
            &job("run-1"),
            "m",
            "w",
            Some(&run_archive::digest_context(&fo_job.context)),
            None,
            None,
        )
        .await;
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn run_archive_open_failure_is_swallowed() {
        // Pre-create `run.lvr` as a directory so opening it for append fails; the
        // best-effort archive write must not panic (and the rest still runs).
        let dir = tempfile::tempdir().unwrap();
        let run_dir = dir.path().join("run-1");
        std::fs::create_dir_all(run_dir.join("run.lvr")).unwrap();
        write_snapshot(dir.path(), &job("run-1"), "m", "w", None, None, None).await;
        // meta.json still written despite the archive failure.
        assert!(run_dir.join("meta.json").exists());
    }

    /// A run somebody deleted has to stay deleted.
    ///
    /// The lane ran `create_dir_all` before every write, so the next message
    /// for that run rebuilt the whole directory - meta, context, transcript -
    /// seconds after the console said it was gone. The closing write of a run
    /// being cancelled was enough on its own, which is what made deleting a
    /// stuck run from the dashboard look like it did nothing at all.
    #[tokio::test]
    async fn a_deleted_run_is_not_written_back() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().to_path_buf();
        let (tx, rx) = mpsc::unbounded_channel();
        let worker = tokio::spawn(persistence_worker(Some(runs.clone()), rx, health()));

        // The run establishes its directory the ordinary way. The lane handles
        // messages in order and an append carries an ack, so an acked append
        // sent after the snapshot is a barrier: when it answers, the snapshot
        // is entirely on disk. Polling for `meta.json` alone is not enough:
        // `remove_dir_all` below then races the lane still writing the rest of
        // the snapshot beside it, which is `DirectoryNotEmpty` on a macOS
        // runner.
        tx.send(PersistMsg::Snapshot(Box::new(job("run-1"))))
            .unwrap();
        let (settled_tx, settled_rx) = tokio::sync::oneshot::channel();
        tx.send(PersistMsg::Append {
            run_id: "run-1".to_string(),
            record: Box::new(batch_record(0, "c0")),
            ack: Some(settled_tx),
        })
        .unwrap();
        let settled = settled_rx.await.expect("the first snapshot is written");
        assert!(
            settled.landed_at().is_some(),
            "the record went into a real archive: {settled:?}"
        );
        let run_dir = runs.join("run-1");
        assert!(
            run_dir.join("meta.json").exists(),
            "the first write establishes the directory"
        );

        // Deleted out from under the lane, which is all `d` in the dashboard
        // and `DELETE /api/runs/{id}` do.
        std::fs::remove_dir_all(&run_dir).unwrap();

        // Every later message for it is dropped - a snapshot, a journal append,
        // and a batch of stage lines, since all three would otherwise create
        // the directory again.
        tx.send(PersistMsg::Snapshot(Box::new(job("run-1"))))
            .unwrap();
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        tx.send(PersistMsg::Append {
            run_id: "run-1".to_string(),
            record: Box::new(batch_record(0, "c1")),
            ack: Some(ack_tx),
        })
        .unwrap();
        tx.send(PersistMsg::StageLines {
            run_id: "run-1".to_string(),
            output_appends: vec![(0, "a line".to_string())],
            log_appends: vec![(0, "a log line".to_string())],
        })
        .unwrap();
        // The ack still fires. A dropped append that never answered would park
        // the tool lane's dispatch barrier for the rest of the run. It reports
        // no journal rather than a failure: the run is gone, which is not the
        // same as a write that broke.
        assert_eq!(
            ack_rx
                .await
                .expect("a dropped append is still acknowledged"),
            Appended::NoJournal
        );

        // A different run is unaffected: this is one run being forgotten, not
        // the lane giving up.
        tx.send(PersistMsg::Snapshot(Box::new(job("run-2"))))
            .unwrap();
        drop(tx);
        worker.await.unwrap();

        // Listed before the assert rather than inside its message: a message
        // on an assert that passes never runs, and never-run code does not
        // meet the coverage gate.
        let left: Vec<String> = std::fs::read_dir(&runs)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            !left.iter().any(|n| n == "run-1"),
            "the deleted run came back: {left:?}"
        );
        assert!(
            runs.join("run-2").join("meta.json").exists(),
            "another run still writes"
        );
    }

    /// Each record's position is where its frame starts, and positions climb.
    ///
    /// This is what lets a debugger name a record: a position identifies one
    /// record in one run's journal forever, because the journal is only
    /// appended to. Derived from the file's length rather than counted, so a
    /// reader seeking there finds the frame this ack described.
    #[tokio::test]
    async fn an_append_reports_where_the_record_landed() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().to_path_buf();
        let (tx, rx) = mpsc::unbounded_channel();
        let worker = tokio::spawn(persistence_worker(Some(runs.clone()), rx, health()));
        tx.send(PersistMsg::Snapshot(Box::new(job("run-1"))))
            .unwrap();

        let mut positions = Vec::new();
        for call in ["c1", "c2", "c3"] {
            let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
            tx.send(PersistMsg::Append {
                run_id: "run-1".to_string(),
                record: Box::new(batch_record(0, call)),
                ack: Some(ack_tx),
            })
            .unwrap();
            let acked = ack_rx.await.expect("acked");
            positions.push(acked.landed_at().expect("a real archive takes it"));
        }
        drop(tx);
        worker.await.unwrap();

        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "positions climb: {positions:?}"
        );
        // The last position plus its frame is the whole file, so nothing was
        // written past where the ack said the record began.
        let written = std::fs::metadata(runs.join("run-1").join("run.lvr"))
            .unwrap()
            .len();
        assert!(
            positions.last().is_some_and(|last| *last < written),
            "{positions:?} inside {written} bytes"
        );
        // Reading from that offset finds the record the ack described.
        use std::io::{Read, Seek};
        let mut file = std::fs::File::open(runs.join("run-1").join("run.lvr")).unwrap();
        file.seek(std::io::SeekFrom::Start(*positions.last().unwrap()))
            .unwrap();
        let mut tail = Vec::new();
        file.read_to_end(&mut tail).unwrap();
        let record = run_archive::read_record(&mut tail.as_slice())
            .unwrap()
            .expect("a whole record sits at that position");
        assert_eq!(
            batch_call_id(&record),
            Some("c3"),
            "the record at that position is the batch this test appended"
        );
        assert_eq!(
            batch_call_id(&run_archive::RunRecord::StatusChanged {
                status: leviath_core::run_meta::RunStatus::Complete,
                at: 1,
            }),
            None,
            "a record of another kind names no call"
        );
    }

    /// A write that fails is reported as a failure, not as a landing.
    ///
    /// The difference matters at the dispatch barrier: a batch whose record was
    /// lost runs with nothing in the journal to say it ever started, and the
    /// only way anyone learns that is this answer.
    #[tokio::test]
    async fn a_failed_append_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().to_path_buf();
        // A directory where the archive belongs: it exists, so the append is
        // attempted, and opening it for writing cannot work.
        std::fs::create_dir_all(runs.join("run-1").join("run.lvr")).unwrap();
        let landed = append_record(&runs, "run-1", &batch_record(0, "c1"), &health()).await;
        assert_eq!(landed, Appended::Failed);
        assert!(landed.landed_at().is_none(), "a failure is nowhere");
    }

    /// A run with no archive yet has nowhere to append, and says so.
    #[tokio::test]
    async fn an_append_before_the_first_snapshot_has_no_journal() {
        let dir = tempfile::tempdir().unwrap();
        let landed = append_record(dir.path(), "run-1", &batch_record(0, "c1"), &health()).await;
        assert_eq!(landed, Appended::NoJournal);
        assert!(landed.landed_at().is_none(), "no journal is nowhere");
    }

    /// The first call id of a batch record, or nothing for any other record.
    ///
    /// Exercised both ways below, so the arm that says "this is not a batch" is
    /// a claim a test makes rather than a branch nothing takes.
    fn batch_call_id(record: &leviath_core::run_archive::RunRecord) -> Option<&str> {
        match record {
            run_archive::RunRecord::ToolBatch { calls, .. } => {
                calls.first().map(|call| call.id.as_str())
            }
            _ => None,
        }
    }

    fn batch_record(iteration: usize, call_id: &str) -> leviath_core::run_archive::RunRecord {
        leviath_core::run_archive::RunRecord::ToolBatch {
            calls: vec![leviath_core::run_archive::ToolCallRecord {
                id: call_id.to_string(),
                execution_id: String::new(),
                name: "shell".to_string(),
                arguments: "{}".to_string(),
                result: None,
                thought_signature: None,
            }],
            at: 1,
            stage_index: 0,
            iteration,
            visit_id: String::new(),
            requested_by: String::new(),
            response: "running".to_string(),
        }
    }

    #[tokio::test]
    async fn worker_appends_records_after_a_snapshot_and_acks() {
        // A Snapshot creates the archive; a subsequent Append lands behind it on
        // the same single-worker lane, so the record always follows the Header.
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(PersistMsg::Snapshot(Box::new(job("run-1"))))
            .unwrap();
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        tx.send(PersistMsg::Append {
            run_id: "run-1".to_string(),
            record: Box::new(batch_record(0, "c1")),
            ack: Some(ack_tx),
        })
        .unwrap();
        tx.send(PersistMsg::Append {
            run_id: "run-1".to_string(),
            record: Box::new(leviath_core::run_archive::RunRecord::ToolCallDone {
                iteration: 0,
                call_id: "c1".to_string(),
                execution_id: String::new(),
                result: "ran".to_string().into(),
                outcome: None,
                at: 2,
            }),
            ack: None, // the fire-and-forget per-call path
        })
        .unwrap();
        drop(tx);
        persistence_worker(Some(dir.path().to_path_buf()), rx, health()).await;

        ack_rx.await.expect("append acked");
        let bytes = std::fs::read(dir.path().join("run-1").join("run.lvr")).unwrap();
        let (_v, records) = leviath_core::run_archive::read_archive(&mut bytes.as_slice()).unwrap();
        let folded = leviath_core::run_archive::fold(&records).unwrap();
        let pending = folded.pending_batch.expect("batch folds as pending");
        assert_eq!(pending.calls[0].result.as_deref(), Some("ran"));
    }

    #[tokio::test]
    async fn append_without_an_archive_is_skipped_but_still_acks() {
        // No snapshot ever landed for this run: appending would write a frame
        // with no preamble/Header, so it is skipped - and the ack still fires so
        // the dispatch-side barrier never hangs on the miss.
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        tx.send(PersistMsg::Append {
            run_id: "run-none".to_string(),
            record: Box::new(batch_record(0, "c1")),
            ack: Some(ack_tx),
        })
        .unwrap();
        drop(tx);
        persistence_worker(Some(dir.path().to_path_buf()), rx, health()).await;

        ack_rx.await.expect("acked despite the skip");
        assert!(!dir.path().join("run-none").join("run.lvr").exists());
    }

    #[tokio::test]
    async fn append_open_failure_is_swallowed() {
        // `run.lvr` exists but is a directory: the existence probe passes and the
        // append open fails; best-effort means no panic (and the ack still fires
        // via the worker, exercised above - here we call the writer directly).
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("run-1").join("run.lvr")).unwrap();
        append_record(dir.path(), "run-1", &batch_record(0, "c1"), &health()).await;
    }

    #[test]
    fn machine_id_is_persisted_and_reused() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        std::fs::create_dir_all(&runs).unwrap();
        let first = load_or_create_machine_id(&runs);
        assert!(!first.is_empty());
        // A second call returns the same persisted id.
        assert_eq!(load_or_create_machine_id(&runs), first);
        // It lives next to the runs dir.
        assert!(dir.path().join("machine-id").exists());
    }

    #[test]
    fn machine_id_regenerates_when_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        std::fs::create_dir_all(&runs).unwrap();
        std::fs::write(dir.path().join("machine-id"), "   \n").unwrap();
        let id = load_or_create_machine_id(&runs);
        assert!(!id.is_empty());
    }

    #[test]
    fn generate_id_is_sixteen_hex_chars() {
        let id = generate_id();
        assert_eq!(id.len(), 16);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[tokio::test]
    async fn worker_writes_stages_index_and_appends_output_and_logs() {
        let dir = tempfile::tempdir().unwrap();
        write_snapshot(
            dir.path(),
            &PersistJob {
                run_id: "r".to_string(),
                meta: meta("r"),
                context: context(),
                stages: vec![StageRecord::new("plan".to_string(), 0)],
                output_appends: vec![(0, "the plan".to_string())],
                log_appends: vec![(0, "[tool] list_dir: .".to_string())],
                taint_audit: None,
                fanout: None,
                interactions: None,
                final_output: None,
            },
            "machine-test",
            "world-test",
            None,
            None,
            None,
        )
        .await;

        let run = dir.path().join("r");
        let idx: Vec<StageRecord> =
            serde_json::from_str(&std::fs::read_to_string(run.join("stages.json")).unwrap())
                .unwrap();
        assert_eq!(idx[0].name, "plan");
        let out = std::fs::read_to_string(run.join("stages/0/output.log")).unwrap();
        assert!(out.contains("the plan"));
        let log = std::fs::read_to_string(run.join("stages/0/logs.log")).unwrap();
        assert!(log.contains("[tool] list_dir"));
    }

    #[tokio::test]
    async fn worker_writes_taint_audit_to_the_stage_dir() {
        let dir = tempfile::tempdir().unwrap();
        write_snapshot(
            dir.path(),
            &PersistJob {
                run_id: "r".to_string(),
                meta: meta("r"),
                context: context(),
                stages: vec![],
                output_appends: vec![],
                log_appends: vec![],
                taint_audit: Some((2, r#"[{"tool_name":"shell"}]"#.to_string())),
                fanout: None,
                interactions: None,
                final_output: None,
            },
            "machine-test",
            "world-test",
            None,
            None,
            None,
        )
        .await;
        let audit =
            std::fs::read_to_string(dir.path().join("r/stages/2/taint_audit.json")).unwrap();
        assert!(audit.contains("shell"));
    }

    #[tokio::test]
    async fn interactions_sidecar_is_written_then_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r/interactions.json");

        // A job parked at an interaction point writes the sidecar.
        write_snapshot(
            dir.path(),
            &PersistJob {
                interactions: Some(r#"{"cursor":0,"round":1,"body":"the plan"}"#.to_string()),
                ..job("r")
            },
            "machine-test",
            "world-test",
            None,
            None,
            None,
        )
        .await;
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("the plan"));

        // A later job that is no longer parked removes the stale sidecar.
        write_snapshot(
            dir.path(),
            &job("r"),
            "machine-test",
            "world-test",
            None,
            None,
            None,
        )
        .await;
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn empty_stages_are_not_written() {
        let dir = tempfile::tempdir().unwrap();
        write_snapshot(
            dir.path(),
            &PersistJob {
                run_id: "r".to_string(),
                meta: meta("r"),
                context: context(),
                stages: vec![],
                output_appends: vec![],
                log_appends: vec![],
                taint_audit: None,
                fanout: None,
                interactions: None,
                final_output: None,
            },
            "machine-test",
            "world-test",
            None,
            None,
            None,
        )
        .await;
        // No stages.json, no stages dir when there's nothing to write.
        assert!(!dir.path().join("r/stages.json").exists());
    }

    #[tokio::test]
    async fn stage_line_open_failure_is_handled() {
        crate::test_support::with_tracing(|| {});
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("r");
        // `output.log` already exists as a *directory*, so the append open fails.
        std::fs::create_dir_all(run.join("stages/0/output.log")).unwrap();
        append_stage_line(&run, 0, "output.log", "line", "r").await;
        // No panic; nothing appended (the path is a directory).
    }

    #[tokio::test]
    async fn write_is_skipped_when_runs_dir_unwritable() {
        crate::test_support::with_tracing(|| {});
        // runs_dir points at a *file*, so create_dir_all fails - must not panic.
        let file = tempfile::NamedTempFile::new().unwrap();
        write_snapshot(
            file.path(), // a file, not a dir
            &PersistJob {
                run_id: "r".to_string(),
                meta: meta("r"),
                context: context(),
                stages: vec![],
                output_appends: vec![],
                log_appends: vec![],
                taint_audit: None,
                fanout: None,
                interactions: None,
                final_output: None,
            },
            "machine-test",
            "world-test",
            None,
            None,
            None,
        )
        .await;
    }

    #[tokio::test]
    async fn temp_write_failure_is_handled() {
        crate::test_support::with_tracing(|| {});
        let dir = tempfile::tempdir().unwrap();
        let run_dir = dir.path().join("r");
        std::fs::create_dir_all(&run_dir).unwrap();
        // Make the meta temp path a directory so the temp-file write fails.
        std::fs::create_dir_all(run_dir.join("meta.json.tmp")).unwrap();

        write_snapshot(
            dir.path(),
            &PersistJob {
                run_id: "r".to_string(),
                meta: meta("r"),
                context: context(),
                stages: vec![],
                output_appends: vec![],
                log_appends: vec![],
                taint_audit: None,
                fanout: None,
                interactions: None,
                final_output: None,
            },
            "machine-test",
            "world-test",
            None,
            None,
            None,
        )
        .await;

        assert!(!run_dir.join("meta.json").exists()); // temp write failed
        assert!(run_dir.join("context.json").exists()); // still written
    }

    #[tokio::test]
    async fn rename_failure_is_handled() {
        crate::test_support::with_tracing(|| {});
        // Make the destination path a directory so rename over it fails; the temp
        // file is cleaned up and we don't panic.
        let dir = tempfile::tempdir().unwrap();
        let run_dir = dir.path().join("r");
        std::fs::create_dir_all(&run_dir).unwrap();
        std::fs::create_dir_all(run_dir.join("meta.json")).unwrap(); // dir where a file goes

        write_snapshot(
            dir.path(),
            &PersistJob {
                run_id: "r".to_string(),
                meta: meta("r"),
                context: context(),
                stages: vec![],
                output_appends: vec![],
                log_appends: vec![],
                taint_audit: None,
                fanout: None,
                interactions: None,
                final_output: None,
            },
            "machine-test",
            "world-test",
            None,
            None,
            None,
        )
        .await;

        // context.json still written despite the meta.json rename conflict.
        assert!(run_dir.join("context.json").exists());
    }

    /// The archive append is best-effort, and the lane keeps a digest of the
    /// last *archived* context so the next write can be a compact diff.
    /// Advancing that digest for a record that never landed is unrecoverable:
    /// every later delta is then relative to a state no reader can rebuild, so
    /// the folded archive drifts from the run for the rest of its life while
    /// `context.json` - written whole each time - stays correct.
    ///
    /// A directory where `run.lvr` should be is the cheapest real append
    /// failure: the open fails, the rest of the write still succeeds.
    #[tokio::test]
    async fn a_failed_archive_append_is_reported_so_the_baseline_can_stay_put() {
        let dir = tempfile::tempdir().expect("temp dir");
        let j = job_with_context("run-torn", 3);

        // Nothing in the way: the record lands and the write says so.
        let ok = write_snapshot(dir.path(), &j, "m", "w", None, None, None).await;
        assert!(ok.archived(), "an unobstructed append lands");

        // Now make `run.lvr` unopenable and try again.
        let blocked = tempfile::tempdir().expect("temp dir");
        let run_dir = blocked.path().join(&j.run_id);
        std::fs::create_dir_all(run_dir.join("run.lvr")).expect("occupy the archive path");
        let failed = write_snapshot(blocked.path(), &j, "m", "w", None, None, None).await;
        assert!(
            !failed.archived(),
            "an append that could not open its file must not report success"
        );
        // The rest of the write is unaffected - this is why the failure is
        // invisible without the flag, and why `context.json` stays right while
        // the journal drifts.
        assert!(
            run_dir.join("context.json").exists(),
            "the context snapshot is still written"
        );
    }

    /// End to end through the lane: when the archive append fails, the digest
    /// the next diff rebases on must stay where it was.
    ///
    /// Observable from the archive itself. The lane anchors with a full
    /// `ContextCheckpoint` whenever it has no prior digest for a run, so if the
    /// baseline were advanced past the failed write, the second snapshot would
    /// be recorded as a compact `Progress` diff against a state that was never
    /// written - and the archive would be unfoldable from its own contents.
    /// Holding the baseline means the second write re-anchors instead.
    #[tokio::test]
    async fn a_run_whose_append_failed_re_anchors_instead_of_diffing_against_nothing() {
        let dir = tempfile::tempdir().unwrap();
        // Occupy the archive path so the first append cannot open it.
        let run_dir = dir.path().join("run-torn");
        std::fs::create_dir_all(run_dir.join("run.lvr")).unwrap();

        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(PersistMsg::Snapshot(Box::new(job_with_context(
            "run-torn", 2,
        ))))
        .unwrap();
        drop(tx);
        persistence_worker(Some(dir.path().to_path_buf()), rx, health()).await;

        // The context still landed; only the journal did not.
        assert!(run_dir.join("context.json").exists());

        // Free the path and send a second, different snapshot.
        std::fs::remove_dir(run_dir.join("run.lvr")).unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(PersistMsg::Snapshot(Box::new(job_with_context(
            "run-torn", 5,
        ))))
        .unwrap();
        drop(tx);
        persistence_worker(Some(dir.path().to_path_buf()), rx, health()).await;

        let bytes = std::fs::read(run_dir.join("run.lvr")).unwrap();
        let (_, records) =
            leviath_core::run_archive::read_archive_lenient(&mut bytes.as_slice()).unwrap();
        assert!(
            records.iter().any(|r| matches!(
                r,
                leviath_core::run_archive::RunRecord::ContextCheckpoint { .. }
            )),
            "the second write re-anchored with a full snapshot: {records:?}"
        );
        // And the archive folds to the run's real state rather than to a
        // diff against something absent.
        let folded = leviath_core::run_archive::fold(&records).expect("has a header");
        assert_eq!(
            folded.context.regions[0].entries.len(),
            5,
            "folds to what the run actually held"
        );
    }

    /// A run's terminal transition is recorded as an event, not left to be
    /// inferred by diffing metadata between records.
    ///
    /// The post-mortem side is what needs it: without the record, a journal
    /// carries no statement of when a run went terminal or to what, so working
    /// it out means correlating daemon logs a harness may have sent to
    /// /dev/null.
    #[tokio::test]
    async fn a_change_of_status_is_journalled_as_its_own_record() {
        let dir = tempfile::tempdir().unwrap();
        let mut running = job("run-status");
        running.meta.status = leviath_core::run_meta::RunStatus::Running;
        let mut done = job("run-status");
        done.meta.status = leviath_core::run_meta::RunStatus::Complete;

        // Two batches through *one* worker: the lane remembers the last status
        // it archived for the life of the worker, which is how a long-running
        // daemon sees the transition. Sending both at once would coalesce them
        // to the newest and there would be no transition to see.
        let (tx, rx) = mpsc::unbounded_channel();
        let runs_dir = dir.path().to_path_buf();
        let worker =
            tokio::spawn(async move { persistence_worker(Some(runs_dir), rx, health()).await });
        tx.send(PersistMsg::Snapshot(Box::new(running))).unwrap();
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        tx.send(PersistMsg::Snapshot(Box::new(done))).unwrap();
        drop(tx);
        worker
            .await
            .expect("the worker exits when the channel closes");

        let bytes = std::fs::read(dir.path().join("run-status").join("run.lvr")).unwrap();
        let (_, records) =
            leviath_core::run_archive::read_archive_lenient(&mut bytes.as_slice()).unwrap();
        let changes: Vec<&leviath_core::run_meta::RunStatus> = records
            .iter()
            .filter_map(|r| match r {
                leviath_core::run_archive::RunRecord::StatusChanged { status, .. } => Some(status),
                _ => None,
            })
            .collect();
        assert_eq!(
            changes,
            vec![&leviath_core::run_meta::RunStatus::Complete],
            "the transition, and not the opening status the Header already carries"
        );
    }

    /// How many times [`busy_once`] has been asked.
    static BUSY_ONCE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    /// An attempt that refuses once and works after that, which is the disk the
    /// retry exists for. A plain `fn`, because that is what the loop takes.
    fn busy_once<'a>(
        _path: &'a Path,
        _buf: &'a [u8],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<()>> + Send + 'a>> {
        let first = BUSY_ONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 0;
        Box::pin(async move {
            match first {
                true => Err(std::io::Error::other("the disk was busy")),
                false => Ok(()),
            }
        })
    }

    /// How many times [`full_disk`] has been asked.
    static FULL_DISK: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    /// An attempt that never works, which is the disk a run is failed for.
    fn full_disk<'a>(
        _path: &'a Path,
        _buf: &'a [u8],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<()>> + Send + 'a>> {
        FULL_DISK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Box::pin(async { Err(std::io::Error::other("no space left on device")) })
    }

    /// The retry is what tells a momentary refusal from a disk that will not
    /// take the record: an append that works the second time is not a loss.
    #[tokio::test]
    async fn an_append_that_works_the_second_time_is_not_a_loss() {
        crate::test_support::with_tracing(|| {});
        BUSY_ONCE.store(0, std::sync::atomic::Ordering::Relaxed);
        let outcome = retrying(
            busy_once,
            Path::new("/runs/run-1/run.lvr"),
            b"a record",
            std::time::Duration::ZERO,
            "run-1",
        )
        .await;
        assert_eq!(outcome, Ok(()));
        assert_eq!(
            BUSY_ONCE.load(std::sync::atomic::Ordering::Relaxed),
            2,
            "asked again, and only once again"
        );
    }

    /// Twice is the answer: the record is lost, and the loss names the file.
    #[tokio::test]
    async fn an_append_that_fails_twice_is_lost() {
        crate::test_support::with_tracing(|| {});
        FULL_DISK.store(0, std::sync::atomic::Ordering::Relaxed);
        let outcome = retrying(
            full_disk,
            Path::new("/runs/run-1/run.lvr"),
            b"a record",
            std::time::Duration::ZERO,
            "run-1",
        )
        .await;
        let lost = outcome.expect_err("two refusals are a loss");
        assert_eq!(lost.path, Path::new("/runs/run-1/run.lvr"));
        assert!(lost.message.contains("no space left"), "{lost:?}");
        assert_eq!(
            FULL_DISK.load(std::sync::atomic::Ordering::Relaxed),
            2,
            "not tried for ever"
        );
    }

    /// A genuine write failure is counted, described, and names the run so the
    /// world can fail it.
    #[tokio::test]
    async fn a_lost_record_is_counted_and_names_its_run() {
        crate::test_support::with_tracing(|| {});
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().to_path_buf();
        // A directory where the archive belongs: it exists, so the append is
        // attempted, and opening it for writing cannot work.
        std::fs::create_dir_all(runs.join("run-1").join("run.lvr")).unwrap();
        let stats = health();

        assert_eq!(
            append_record(&runs, "run-1", &batch_record(0, "c1"), &stats).await,
            Appended::Failed
        );

        let report = stats.report();
        assert_eq!(report.appends_failed, 1);
        assert!(!report.is_healthy());
        let last = report.last_error.expect("the loss is described");
        assert_eq!(last.run_id, "run-1");
        assert!(last.path.ends_with("run.lvr"), "{}", last.path);
        assert_eq!(
            stats
                .take_unwritable()
                .iter()
                .map(|e| e.run_id.clone())
                .collect::<Vec<_>>(),
            vec!["run-1".to_string()],
            "the run is named so it can be failed"
        );
    }

    /// **Deleting a run must never fail it.** A delete that lands while the lane
    /// is writing leaves exactly the shape a broken disk does - an open that
    /// fails - and the two are told apart by looking for the run directory
    /// afterwards, not by the error.
    #[tokio::test]
    async fn a_run_deleted_mid_write_is_not_a_failure() {
        crate::test_support::with_tracing(|| {});
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().to_path_buf();
        let run_dir = runs.join("run-1");
        // The archive path is a directory, so every attempt to open it fails.
        std::fs::create_dir_all(run_dir.join("run.lvr")).unwrap();
        let gone = run_dir.clone();
        // Deleted between the first attempt and the second, which is the window
        // the retry opens and the one a real delete lands in.
        let deleter = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            std::fs::remove_dir_all(&gone).expect("the run is deleted");
        });
        let stats = health();

        let landed = append_record(&runs, "run-1", &batch_record(0, "c1"), &stats).await;
        deleter.await.expect("the deleter finishes");

        assert_eq!(landed, Appended::NoJournal, "a delete is not a failure");
        let report = stats.report();
        assert!(report.is_healthy(), "and nothing is counted against it");
        assert!(
            stats.take_unwritable().is_empty(),
            "and no run is named to be failed"
        );
    }

    /// A snapshot reports the first file it lost, not the last one it tried, so
    /// what an operator reads is where the trouble started.
    #[tokio::test]
    async fn a_snapshot_reports_the_first_file_it_lost() {
        crate::test_support::with_tracing(|| {});
        let dir = tempfile::tempdir().unwrap();
        let run_dir = dir.path().join("r");
        std::fs::create_dir_all(&run_dir).unwrap();
        // Both temp paths occupied by directories: both writes fail.
        std::fs::create_dir_all(run_dir.join("meta.json.tmp")).unwrap();
        std::fs::create_dir_all(run_dir.join("context.json.tmp")).unwrap();

        let outcome = write_snapshot(dir.path(), &job("r"), "m", "w", None, None, None).await;

        let lost = outcome.files.as_ref().expect("two files were lost");
        assert!(
            lost.path.ends_with("meta.json.tmp"),
            "the first one: {lost:?}"
        );
        assert!(
            outcome.archived(),
            "the journal record is unaffected by either"
        );
    }

    /// End to end through the lane: a snapshot whose journal record cannot be
    /// written counts against the journal and names its run, while a snapshot
    /// that only lost an ordinary file counts as a snapshot and names nobody.
    #[tokio::test]
    async fn the_lane_counts_the_journal_apart_from_the_files_beside_it() {
        crate::test_support::with_tracing(|| {});
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().to_path_buf();
        // run-lost cannot have its journal written; run-meta cannot have its
        // `meta.json` written.
        std::fs::create_dir_all(runs.join("run-lost").join("run.lvr")).unwrap();
        std::fs::create_dir_all(runs.join("run-meta").join("meta.json.tmp")).unwrap();
        let stats = health();
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(PersistMsg::Snapshot(Box::new(job("run-lost"))))
            .unwrap();
        tx.send(PersistMsg::Snapshot(Box::new(job("run-meta"))))
            .unwrap();
        drop(tx);

        persistence_worker(Some(runs), rx, stats.clone()).await;

        let report = stats.report();
        assert_eq!(report.appends_attempted, 2, "one per snapshot");
        assert_eq!(report.appends_failed, 1);
        assert_eq!(report.snapshots_failed, 1);
        assert_eq!(report.queue_depth, 2, "both arrived in one batch");
        assert_eq!(
            stats
                .take_unwritable()
                .iter()
                .map(|e| e.run_id.clone())
                .collect::<Vec<_>>(),
            vec!["run-lost".to_string()],
            "only the run whose history is missing a record"
        );
    }

    /// A snapshot that cannot make its run directory never gets as far as the
    /// journal, so nothing is counted as an append and no run is failed for it:
    /// a directory that will not be made is also what a delete leaves behind.
    #[tokio::test]
    async fn a_snapshot_with_nowhere_to_write_counts_no_append() {
        crate::test_support::with_tracing(|| {});
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().to_path_buf();
        // A file where the run directory belongs, so `create_dir_all` fails.
        std::fs::write(runs.join("run-blocked"), b"in the way").unwrap();
        let stats = health();
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(PersistMsg::Snapshot(Box::new(job("run-blocked"))))
            .unwrap();
        drop(tx);

        persistence_worker(Some(runs), rx, stats.clone()).await;

        let report = stats.report();
        assert_eq!(report.appends_attempted, 0, "the journal was never reached");
        assert_eq!(report.appends_failed, 0);
        assert_eq!(report.snapshots_failed, 1);
        assert!(
            stats.take_unwritable().is_empty(),
            "and no run is failed for it"
        );
    }
}
