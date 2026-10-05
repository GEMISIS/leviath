//! The async I/O lane for agent-state persistence.
//!
//! The snapshot-dispatch system builds a [`PersistJob`] whenever an agent
//! meaningfully changes and sends it to this single-worker lane, with the
//! run-file step the world decided for it; what happened to a run with no
//! snapshot due comes as a step of its own. [`persistence_worker`] writes each
//! message under `<runs_dir>/<run_id>/` **one at a time**, so writes for a given
//! run never race or land out of order: the answer's bytes, the readable
//! per-stage logs and the taint audit beside the run file, then the step in the
//! run file, naming each of those files as it now stands. The lane decides
//! nothing about a run's state; it writes what it is handed.
//!
//! Every error is logged and counted in
//! [`crate::persist_stats::PersistLaneStats`], and the lane
//! itself never blocks or fails on one: a write that cannot be made is reported
//! and the lane moves to the next message. A lost **run-file step** also names
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
use crate::state::files::{FileRef, RunFiles, StageFile};

use leviath_core::run_meta::RunMeta;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::UnboundedReceiver;

/// One agent snapshot to write to disk.
pub(crate) struct PersistJob {
    /// The run id (its directory name under the runs dir).
    pub run_id: String,
    /// The run's metadata as the world sees it: its status, and the
    /// descriptor of its answer when it has one.
    pub meta: RunMeta,
    /// Readable output lines to append to `stages/<idx>/output.log`.
    pub output_appends: Vec<(usize, String)>,
    /// Operational log lines to append to `stages/<idx>/logs.log`.
    pub log_appends: Vec<(usize, String)>,
    /// `(stage_index, serialized GateEvent log)` to write to
    /// Each stage's taint audit to write whole, as `(stage index, JSON)`,
    /// to `stages/<idx>/taint_audit.json`. Empty when no stage's audit
    /// changed.
    pub taint_audit: Vec<(usize, String)>,
    /// The run's answer, to write to its `final_output` sidecar. `None` ⇒
    /// nothing to write this job, either because the run has no answer or
    /// because the one it has is already on disk.
    ///
    /// Sent only when it changes: a heartbeat that rewrote a quarter-megabyte
    /// answer every thirty seconds would be pure waste on a long run.
    pub final_output: Option<String>,
    /// The run's step for its run file, when it has a spec to write one
    /// from. Coalescing drops a superseded one's state with its snapshot,
    /// which loses nothing: the next step is a diff against whatever was last
    /// written, and carries the superseded step's events and waiters.
    pub run_file: Option<Box<crate::runfile::lane::RunFileStep>>,
}

/// Output lines and log lines for a run's stage logs, and its stages' taint
/// audit files, each tagged with its stage index.
type StageLineSet = (
    Vec<(usize, String)>,
    Vec<(usize, String)>,
    Vec<(usize, String)>,
);

/// What became of one append.
///
/// The three states are different facts about the run: one says where the record
/// is, one says there is no run file for it to be in, and one says a write was
/// attempted and lost. Only the last is a problem, and telling it apart from the
/// second is why this is not a `bool` - a dispatch waiting for its record has to
/// be able to say which of them happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Appended {
    /// On disk, as this step of the run's file. Monotonic within a run: a
    /// later record always lands in a later step.
    Landed {
        /// The step the record was written in.
        position: u64,
    },
    /// Nothing was written, and nothing is wrong. Either this world keeps no
    /// run files (an in-memory world), or the run has no file open yet, or the
    /// run was deleted from under the lane.
    NoJournal,
    /// The write was attempted and lost. The run is failed for it, so nothing
    /// else happens that its history would have to explain.
    Failed,
}

/// One message on the persistence lane.
pub(crate) enum PersistMsg {
    /// A whole-agent snapshot. Boxed: a snapshot dwarfs the other messages and
    /// the channel moves these by value.
    Snapshot(Box<PersistJob>),
    /// A run-file step with no snapshot behind it: what happened to a run
    /// since its last step, decided by the world, written at once. Boxed like
    /// `Snapshot`'s job.
    Step(Box<crate::runfile::lane::RunFileStep>),
    /// Buffered per-stage output/log lines with nothing else to report. The
    /// dispatch system sends this instead of a full [`PersistMsg::Snapshot`]
    /// when lines were buffered but the run's watermark did not move, so tool
    /// activity between iterations does not force a whole-state snapshot per
    /// batch of log lines, several times per iteration.
    StageLines {
        /// The run id (its directory name under the runs dir).
        run_id: String,
        /// Readable output lines to append to `stages/<idx>/output.log`.
        output_appends: Vec<(usize, String)>,
        /// Operational log lines to append to `stages/<idx>/logs.log`.
        log_appends: Vec<(usize, String)>,
    },
}

/// Who waits to hear where a step landed.
type Ack = tokio::sync::oneshot::Sender<Appended>;

/// Tell everyone waiting on a step what became of it. A waiter that has
/// stopped waiting is not an error.
fn answer(acks: Vec<Ack>, landed: Appended) {
    for ack in acks {
        let _ = ack.send(landed);
    }
}

/// The single-lane persistence worker: writes each [`PersistJob`] under
/// `runs_dir`, one at a time, until the job channel closes (world shutdown).
///
/// It also owns this daemon's run-ownership identity - a stable per-machine id
/// (`<runs_dir>/../machine-id`, created once) and a per-process `world_id` - which
/// it stamps into every run file it takes over, so a run copied to another
/// machine is unambiguously attributable.
///
/// `runs_dir: None` means the world runs in memory only: the worker drains and
/// drops every message without touching the filesystem (no run dirs, no
/// machine-id), while keeping the channel-close shutdown contract so
/// [`flush_and_stop`](crate::world::PipelineWorld::flush_and_stop) still joins
/// it - and still answering every step's waiters, so a dispatch-side barrier
/// never waits on a dead channel.
///
/// `stats` is shared with the world, which reads it for `lev ps`, `lev doctor`
/// and the GraphQL schema, and drains the runs whose run file could not be
/// written so it can fail them.
pub(crate) async fn persistence_worker(
    runs_dir: Option<PathBuf>,
    mut jobs: UnboundedReceiver<PersistMsg>,
    stats: std::sync::Arc<PersistLaneStats>,
) {
    let Some(runs_dir) = runs_dir else {
        while let Some(msg) = jobs.recv().await {
            let step = match msg {
                PersistMsg::Snapshot(mut job) => job.run_file.take(),
                PersistMsg::Step(step) => Some(step),
                PersistMsg::StageLines { .. } => None,
            };
            if let Some(step) = step {
                answer(step.acks, Appended::NoJournal);
            }
        }
        return;
    };
    let machine_id = load_or_create_machine_id(&runs_dir);
    let world_id = generate_id();
    // What the lane has actually written to each run's `final_output` sidecar,
    // as `(submitted_at, bytes)`. `submit_output` replaces rather than appends,
    // so a new answer always carries a later stamp.
    //
    // This lives here rather than with the sender because the sender cannot
    // know whether a job it built was written: the coalescing below drops
    // superseded snapshots. Here the skip is decided after that, so it
    // reflects what is on disk.
    let mut last_output: std::collections::HashMap<String, (i64, usize)> =
        std::collections::HashMap::new();
    // The runs this lane has already written for, so a later write can tell a
    // directory it is about to establish from one somebody has deleted. See
    // [`may_write`].
    let mut staked: HashSet<String> = HashSet::new();
    let mut run_files = crate::runfile::lane::RunFileLane::new(&machine_id, &world_id);
    while let Some(first) = jobs.recv().await {
        // Drain whatever else is already queued and process it as one batch,
        // keeping only the NEWEST snapshot per run: each snapshot carries the
        // whole state, so writing a superseded one is pure disk and memory
        // churn. On a slow disk this is what stops queued snapshots from
        // piling up unboundedly. Steps and stage lines keep their order and
        // are never dropped.
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
        // What a superseded snapshot carried besides its state, kept for the
        // run's next message: its stage lines and audit files for the newest
        // snapshot, which writes them first, and its events and waiters for
        // the next step. The state is replaced; what happened is not.
        let mut carried: std::collections::HashMap<String, StageLineSet> =
            std::collections::HashMap::new();
        let mut happened = Happened::default();
        for (i, msg) in batch.into_iter().enumerate() {
            match msg {
                PersistMsg::Snapshot(mut job) => {
                    if newest_snapshot.get(job.run_id.as_str()) != Some(&i) {
                        // Superseded by a newer snapshot in this batch.
                        let kept = carried.entry(job.run_id.clone()).or_default();
                        kept.0.append(&mut job.output_appends);
                        kept.1.append(&mut job.log_appends);
                        kept.2.append(&mut job.taint_audit);
                        if let Some(step) = job.run_file.take() {
                            happened.keep(*step);
                        }
                        continue;
                    }
                    if let Some((mut output, mut logs, audits)) = carried.remove(&job.run_id) {
                        output.append(&mut job.output_appends);
                        logs.append(&mut job.log_appends);
                        (job.output_appends, job.log_appends) = (output, logs);
                        // The newest audit of each stage is the whole of it.
                        let newest: std::collections::BTreeMap<usize, String> =
                            audits.into_iter().chain(job.taint_audit).collect();
                        job.taint_audit = newest.into_iter().collect();
                    }
                    let step = happened.onto(&job.run_id, job.run_file.take());
                    if !may_write(&runs_dir, &job.run_id, &mut staked, true) {
                        // Deleted. Forget what was kept about it too, so the
                        // maps stay bounded by the runs still being written.
                        last_output.remove(&job.run_id);
                        run_files.forget(&job.run_id);
                        answer_dropped(step);
                        continue;
                    }
                    let written = last_output.get(&job.run_id).copied();
                    let mut outcome = write_snapshot(&runs_dir, &job, written).await;
                    run_files.wrote(&job.run_id, std::mem::take(&mut outcome.named));
                    if let Some(lost) = &outcome.files {
                        stats.snapshot_failed(&job.run_id, &lost.path, &lost.message);
                    }
                    if let Some(key) = outcome.output {
                        last_output.insert(job.run_id.clone(), key);
                    }
                    if is_terminal_run(&job.meta.status) {
                        last_output.remove(&job.run_id);
                    }
                    // A directory that will not be made is also the shape a run
                    // deleted between the check and the write leaves, and a
                    // delete must never fail a run, so nothing is recorded.
                    match step {
                        Some(step) if outcome.dir_made => {
                            record_run_file(&mut run_files, &runs_dir, step, &stats).await;
                        }
                        dropped => answer_dropped(dropped),
                    }
                }
                PersistMsg::Step(step) => {
                    let run_id = step.run_id.clone();
                    let step = happened.onto_step(*step);
                    // Every waiter hears back: the dispatch-side barrier must
                    // never stall on a failed step, or on a run that has been
                    // deleted out from under it. What it hears is which of
                    // those happened, so it can say so instead of assuming the
                    // record landed.
                    match may_write(&runs_dir, &run_id, &mut staked, false) {
                        true => record_run_file(&mut run_files, &runs_dir, step, &stats).await,
                        false => {
                            run_files.forget(&run_id);
                            answer(step.acks, Appended::NoJournal);
                        }
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
                    let mut named = RunFiles::default();
                    append_lines(&dir, &output_appends, &log_appends, &run_id, &mut named).await;
                    run_files.wrote(&run_id, named);
                }
            }
        }
        // Files written beside a finished run's file, which no later step
        // will name, are named now, so a log's size in the run file is the
        // size the run left it at. A run deleted since has no file to name
        // them in, and nothing is written.
        for run_id in run_files.noted() {
            name_files(&mut run_files, &runs_dir, &run_id, &stats).await;
        }
    }
    // The world is stopping: name the files written since each live run's
    // last step, so a log's size in its run file is the size it was left at.
    for run_id in run_files.unnamed() {
        name_files(&mut run_files, &runs_dir, &run_id, &stats).await;
    }
}

/// The events and waiters of the steps a batch superseded, per run, for the
/// run's next step to carry: a superseded snapshot's state is replaced by a
/// newer one, but what happened before it still happened.
#[derive(Default)]
struct Happened(std::collections::HashMap<String, (Vec<crate::state::RunEvent>, Vec<Ack>)>);

impl Happened {
    /// Keep a superseded step's events and waiters.
    fn keep(&mut self, mut step: crate::runfile::lane::RunFileStep) {
        let kept = self.0.entry(step.run_id).or_default();
        kept.0.append(&mut step.events);
        kept.1.append(&mut step.acks);
    }

    /// The newest snapshot's step, with what was kept for its run ahead of
    /// its own events. A snapshot with no step of its own gets a step of
    /// events when something was kept.
    fn onto(
        &mut self,
        run_id: &str,
        step: Option<Box<crate::runfile::lane::RunFileStep>>,
    ) -> Option<crate::runfile::lane::RunFileStep> {
        match step {
            Some(step) => Some(self.onto_step(*step)),
            None => self.0.contains_key(run_id).then(|| {
                self.onto_step(crate::runfile::lane::RunFileStep::events(
                    run_id,
                    now(),
                    Vec::new(),
                ))
            }),
        }
    }

    /// `step` with what was kept for its run ahead of its own events.
    fn onto_step(
        &mut self,
        mut step: crate::runfile::lane::RunFileStep,
    ) -> crate::runfile::lane::RunFileStep {
        if let Some((mut events, mut acks)) = self.0.remove(&step.run_id) {
            events.append(&mut step.events);
            acks.append(&mut step.acks);
            (step.events, step.acks) = (events, acks);
        }
        step
    }
}

/// Tell the waiters of a step that will not be written that nothing landed.
fn answer_dropped(step: Option<crate::runfile::lane::RunFileStep>) {
    if let Some(step) = step {
        answer(step.acks, Appended::NoJournal);
    }
}

/// Now, in unix seconds.
fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Name the files written beside `run_id`'s run file since its last step,
/// in a step of their own.
async fn name_files(
    run_files: &mut crate::runfile::lane::RunFileLane,
    runs_dir: &Path,
    run_id: &str,
    stats: &PersistLaneStats,
) {
    let step = crate::runfile::lane::RunFileStep::events(run_id, now(), Vec::new());
    record_run_file(run_files, runs_dir, step, stats).await;
}

/// Record one run-file step, and tell whoever waits on it where it landed. A
/// step that cannot be written is the run's history lost, so it is counted
/// against the run, which the world then fails.
async fn record_run_file(
    run_files: &mut crate::runfile::lane::RunFileLane,
    runs_dir: &Path,
    mut step: crate::runfile::lane::RunFileStep,
    stats: &PersistLaneStats,
) {
    let run_id = step.run_id.clone();
    let acks = std::mem::take(&mut step.acks);
    stats.append_attempted();
    let landed = match run_files.record(runs_dir, step).await {
        Ok(Some(seq)) => {
            stats.stepped(&run_id, seq);
            Appended::Landed { position: seq }
        }
        Ok(None) => Appended::NoJournal,
        Err(e) => {
            let message = e.kind.to_string();
            tracing::warn!(run_id = %run_id, error = %e, "persistence: run file step not written");
            stats.journal_append_failed(&run_id, &e.path, &message);
            Appended::Failed
        }
    };
    answer(acks, landed);
}

/// Whether the lane should still write for `run_id`, remembering the run the
/// first time a message that can establish it is asked about.
///
/// `establishes` says whether this message is one that creates the run
/// directory. Only a snapshot does; an appended record needs a run file that
/// is already open, and a stage line needs the directory. So only a snapshot
/// may claim a run the lane has not seen before, and a record that arrives
/// ahead of the first snapshot is dropped rather than counted as the run's
/// arrival. Letting one claim the run meant the snapshot behind it - the write
/// that would have made the directory - found the run already claimed, no
/// directory on disk, and dropped itself as a write to a deleted run.
///
/// A run directory is created **once**, by whoever starts the run: the daemon
/// writing the run's file before the run is placed, or - for an embedded world
/// with no daemon in front of it - this lane's own first write. After that, its
/// existence belongs to whoever is looking after the machine. `d` in the
/// dashboard, `DELETE /api/runs/{id}` and an operator with `rm -rf` all say the
/// same thing by removing it.
///
/// So a write that finds the directory gone is not a gap to repair, and it is
/// not a failure either. Repairing it brings the run back from the dead moments
/// after the console said it was deleted; counting it as a fault would have
/// deleting a run start failing runs. Both roads lead back here, to a write that
/// is quietly dropped and reported as [`Appended::NoJournal`].
///
/// One `stat` per message, taken inline rather than through the blocking pool:
/// the hop would cost more than the syscall it is avoiding.
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
/// Everything under a run directory is owner-only, so the directory holding it
/// has to be too - it is what stops another local user walking in.
async fn create_private_dir(path: &Path) -> std::io::Result<()> {
    let owned = path.to_path_buf();
    tokio::task::spawn_blocking(move || leviath_sys::create_private_dir_all(&owned))
        .await
        .map_err(vanished_task)
        .and_then(|r| r)
}

/// A blocking-pool task that never came back: it panicked, or the runtime was
/// shutting down. Named rather than inlined at each call site so they share
/// one branch, and so a test can reach it with a real
/// [`tokio::task::JoinError`] instead of never at all.
fn vanished_task(e: tokio::task::JoinError) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

/// Open a file for appending, owner-only, off the async runtime.
async fn open_private_append(path: &Path) -> std::io::Result<tokio::fs::File> {
    let owned = path.to_path_buf();
    tokio::task::spawn_blocking(move || leviath_sys::open_private_append(&owned))
        .await
        .map_err(vanished_task)
        .and_then(|r| r)
        .map(tokio::fs::File::from_std)
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

/// What one snapshot write actually managed to put on disk, beside the run
/// file step.
#[derive(Default)]
struct WriteOutcome {
    /// Whether the run directory is there to write into.
    dir_made: bool,
    /// The `final_output` sidecar this write landed, if it wrote one.
    output: Option<(i64, usize)>,
    /// The first of this write's files it could not place: the answer
    /// sidecar or the taint audit. Counted, not fatal - the next snapshot
    /// writes each of them again.
    files: Option<Lost>,
    /// The files this write placed, as the run's next step names them.
    named: RunFiles,
}

/// Write what one job carries besides its run-file step under
/// `<runs_dir>/<run_id>/`: the answer's bytes, the stage logs and the taint
/// audit, and report what reached disk.
async fn write_snapshot(
    runs_dir: &Path,
    job: &PersistJob,
    written_output: Option<(i64, usize)>,
) -> WriteOutcome {
    let mut files = None;
    let dir = runs_dir.join(&job.run_id);
    if let Err(e) = create_private_dir(&dir).await {
        tracing::warn!(run_id = %job.run_id, error = %e, "persistence: create run dir failed");
        return WriteOutcome {
            files: Some(Lost::at(&dir, &e)),
            ..WriteOutcome::default()
        };
    }

    // The answer's bytes, written raw, so serving it is a read and
    // `lev result --raw` is a copy.
    //
    // Skipped only when this lane has already written *this* answer, so a
    // heartbeat does not rewrite an unchanged quarter-megabyte file every
    // thirty seconds. The check is here rather than at the sender because only
    // the lane knows a job survived coalescing to be written at all.
    let submitted = job
        .meta
        .final_output
        .as_ref()
        .map(|d| (d.submitted_at, d.bytes));
    // `submitted.is_none()` writes unconditionally: with no descriptor there is
    // nothing to identify the answer by, and skipping on an unidentifiable key
    // is how the sidecar goes missing in the first place.
    let mut wrote_output = None;
    let mut named = RunFiles::default();
    if let Some(content) = &job.final_output
        && (submitted.is_none() || written_output != submitted)
    {
        let path = leviath_core::FINAL_OUTPUT_FILE;
        if write_bytes_atomic(
            &dir.join(path),
            content.clone().into_bytes(),
            &job.run_id,
            &mut files,
        )
        .await
        {
            named.final_output = Some(FileRef::whole(path, content.as_bytes()));
        }
        wrote_output = submitted;
    }

    append_lines(
        &dir,
        &job.output_appends,
        &job.log_appends,
        &job.run_id,
        &mut named,
    )
    .await;
    // Per-stage taint audit (whole-file, atomic).
    for (idx, json) in &job.taint_audit {
        let index = u32::try_from(*idx).unwrap_or(u32::MAX);
        let path = StageFile::TaintAudit.path(index);
        let target = dir.join(&path);
        let _ = create_private_dir(target.parent().unwrap_or(&dir)).await;
        if write_bytes_atomic(&target, json.clone().into_bytes(), &job.run_id, &mut files).await {
            named.set_stage_file(
                index,
                StageFile::TaintAudit,
                FileRef::whole(path, json.as_bytes()),
            );
        }
    }
    WriteOutcome {
        dir_made: true,
        output: wrote_output,
        files,
        named,
    }
}

/// Append each stage's output and log lines, in order, and name in `named`
/// each log as long as the appends left it.
async fn append_lines(
    dir: &Path,
    output: &[(usize, String)],
    logs: &[(usize, String)],
    run_id: &str,
    named: &mut RunFiles,
) {
    let streams = [(StageFile::Output, output), (StageFile::Logs, logs)];
    for (which, lines) in streams {
        for (idx, line) in lines {
            let index = u32::try_from(*idx).unwrap_or(u32::MAX);
            if let Some(len) = append_stage_line(dir, index, which, line, run_id).await {
                named.set_stage_file(index, which, FileRef::log(which.path(index), len));
            }
        }
    }
}

/// Append one line (with a trailing newline) to the stage's log `which` under
/// the run dir, creating the stage directory if needed. Returns the log's
/// length after the append, or `None` when it could not be opened.
///
/// A failed `create_dir_all` just makes the subsequent open fail, and a failed
/// write is deliberately not reported: these are the readable logs beside the
/// run, not the record of what it did. The run file names each log as long as
/// the lane last found it, so a log that lost lines reads as shorter than that.
/// The run file is what a run is failed for.
async fn append_stage_line(
    run_dir: &Path,
    index: u32,
    which: StageFile,
    line: &str,
    run_id: &str,
) -> Option<u64> {
    let path = crate::state::under(run_dir, &which.path(index));
    let _ = create_private_dir(path.parent().unwrap_or(run_dir)).await;
    match open_private_append(&path).await {
        Ok(mut handle) => {
            let mut bytes = line.as_bytes().to_vec();
            bytes.push(b'\n');
            let _ = handle.write_all(&bytes).await;
            // tokio::fs::File buffers; flush so a reader (dashboard / a sync test)
            // sees the line before the handle is dropped.
            let _ = handle.flush().await;
            handle.metadata().await.ok().map(|meta| meta.len())
        }
        Err(e) => {
            tracing::warn!(run_id = %run_id, error = %e, "persistence: stage log open failed");
            None
        }
    }
}

/// Write `bytes` to `path` via a sibling temp file + rename (atomic on the same
/// filesystem, so a reader never sees a half-written file).
///
/// A failure is logged and left in `lost` for the caller to count. The lane
/// carries on with the rest of the snapshot either way. Returns whether the
/// file is in place.
async fn write_bytes_atomic(
    path: &Path,
    bytes: Vec<u8>,
    run_id: &str,
    lost: &mut Option<Lost>,
) -> bool {
    let tmp = path.with_extension("tmp");
    // `write_private`, not a plain write: these files carry the run's answer
    // and its security decisions. A plain write lands them at the umask
    // default, usually 0644, leaving the 0700 on the enclosing run directory as
    // the only thing between them and any other user.
    //
    // Blocking, so it goes to a blocking thread rather than stalling the
    // persistence lane. `leviath-sys` owns the per-platform mode handling
    // (including the Windows `icacls` path).
    let tmp_for_write = tmp.clone();
    let written =
        tokio::task::spawn_blocking(move || leviath_sys::write_private(&tmp_for_write, &bytes))
            .await;
    if let Err(e) = written.map_err(vanished_task).and_then(|r| r) {
        tracing::warn!(run_id = %run_id, error = %e, "persistence: temp write failed");
        keep_first(lost, Lost::at(&tmp, &e));
        return false;
    }
    match tokio::fs::rename(&tmp, path).await {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(run_id = %run_id, error = %e, "persistence: rename failed");
            keep_first(lost, Lost::at(path, &e));
            let _ = tokio::fs::remove_file(&tmp).await;
            false
        }
    }
}

#[cfg(test)]
#[path = "persistence_bridge_tests.rs"]
mod tests;
