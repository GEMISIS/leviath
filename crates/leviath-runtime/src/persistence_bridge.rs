//! The async I/O lane for agent-state persistence.
//!
//! The snapshot-dispatch system builds a [`PersistJob`] (an agent's `meta.json` +
//! `context.json` value snapshot) whenever the agent meaningfully changes and
//! sends it to this single-worker lane. [`persistence_worker`] writes each job's
//! files under `<runs_dir>/<run_id>/` **one at a time**, so writes for a given
//! agent never race or land out of order. Each file is written to a temp path and
//! atomically renamed into place, so a concurrent reader (the dashboard) never
//! sees a half-written file. All errors are logged and swallowed - persistence is
//! best-effort and must never stall or fail the world.

use std::path::{Path, PathBuf};

use leviath_core::run_meta::{ContextSnapshot, RunMeta, StageRecord};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::UnboundedReceiver;

/// One agent snapshot to write to disk.
pub struct PersistJob {
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

/// One message on the persistence lane.
pub enum PersistMsg {
    /// A whole-agent snapshot (`meta.json` + `context.json` + the archive step).
    /// Boxed: a snapshot dwarfs an `Append` and the channel moves these by value.
    Snapshot(Box<PersistJob>),
    /// Append one journal record to `<runs_dir>/<run_id>/run.lvr` - how a tool
    /// batch's dispatch and per-call completions reach the archive between
    /// snapshots (issue #96).
    Append {
        /// The run id (its directory name under the runs dir).
        run_id: String,
        /// The record to append. Boxed like `Snapshot`'s job: `RunRecord`'s
        /// checkpoint variants are large and the channel moves these by value.
        record: Box<leviath_core::run_archive::RunRecord>,
        /// Fired once the append has been attempted (written, skipped, or
        /// failed) - the dispatch-side barrier that keeps a batch record ahead
        /// of the batch's side effects. `None` for fire-and-forget appends
        /// (per-call results).
        ack: Option<tokio::sync::oneshot::Sender<()>>,
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
pub async fn persistence_worker(
    runs_dir: Option<PathBuf>,
    mut jobs: UnboundedReceiver<PersistMsg>,
) {
    let Some(runs_dir) = runs_dir else {
        while let Some(msg) = jobs.recv().await {
            if let PersistMsg::Append { ack: Some(ack), .. } = msg {
                let _ = ack.send(());
            }
        }
        return;
    };
    let machine_id = load_or_create_machine_id(&runs_dir);
    let world_id = generate_id();
    // The last context window archived per run, so the next write can be stored
    // as a compact diff rather than a full snapshot.
    let mut last_context: std::collections::HashMap<String, ContextSnapshot> =
        std::collections::HashMap::new();
    while let Some(msg) = jobs.recv().await {
        match msg {
            PersistMsg::Snapshot(job) => {
                let prev = last_context.get(&job.run_id);
                write_snapshot(&runs_dir, &job, &machine_id, &world_id, prev).await;
                // Drop a fully-terminal run's cached context (it won't be written
                // again), so the map stays bounded by the set of *live* runs
                // rather than every run the daemon has ever seen.
                if is_terminal_run(&job.meta.status) {
                    last_context.remove(&job.run_id);
                } else {
                    last_context.insert(job.run_id.clone(), job.context.clone());
                }
            }
            PersistMsg::Append {
                run_id,
                record,
                ack,
            } => {
                append_record(&runs_dir, &run_id, &record).await;
                // Ack unconditionally - persistence is best-effort and the
                // dispatch-side barrier must never stall on a failed append.
                if let Some(ack) = ack {
                    let _ = ack.send(());
                }
            }
        }
    }
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
/// pool the same way the atomic writer's does. Best-effort like the rest of the
/// lane: a failure is reported to the caller, which logs and moves on.
async fn open_private_append(path: &Path) -> std::io::Result<tokio::fs::File> {
    let owned = path.to_path_buf();
    tokio::task::spawn_blocking(move || leviath_sys::open_private_append(&owned))
        .await
        .map_err(vanished_task)
        .and_then(|r| r)
        .map(tokio::fs::File::from_std)
}

/// Append a single record to an *existing* run archive. A run whose first
/// snapshot hasn't landed yet has no `run.lvr` (and no preamble/Header), so the
/// append is skipped rather than corrupting the file - the single-worker lane
/// makes that ordering all but impossible in practice, since the spawn tick's
/// snapshot is queued before any batch can dispatch. Best-effort like the rest
/// of the lane.
async fn append_record(
    runs_dir: &Path,
    run_id: &str,
    record: &leviath_core::run_archive::RunRecord,
) {
    let path = runs_dir.join(run_id).join("run.lvr");
    if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
        tracing::warn!(run_id = %run_id, "persistence: record append skipped, no archive yet");
        return;
    }
    let mut buf: Vec<u8> = Vec::new();
    leviath_core::run_archive::write_record(&mut buf, record)
        .expect("writing to a Vec never fails");
    match open_private_append(&path).await {
        Ok(mut file) => {
            let _ = file.write_all(&buf).await;
            let _ = file.flush().await;
        }
        Err(e) => {
            tracing::warn!(run_id = %run_id, error = %e, "persistence: record append failed");
        }
    }
}

/// Whether a run status is fully terminal (no further snapshots expected).
/// `CompleteInteractive` is excluded - such an agent stays live for follow-up.
fn is_terminal_run(status: &leviath_core::run_meta::RunStatus) -> bool {
    use leviath_core::run_meta::RunStatus;
    matches!(
        status,
        RunStatus::Complete | RunStatus::Error | RunStatus::Cancelled
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

/// Write one job's `meta.json` + `context.json` under `<runs_dir>/<run_id>/`,
/// each via a temp file + atomic rename. Best-effort: logs and returns on any
/// error. Serialization is infallible for these plain serde structs, so a
/// serialize error is a bug rather than a runtime condition (`.expect`).
async fn write_snapshot(
    runs_dir: &Path,
    job: &PersistJob,
    machine_id: &str,
    world_id: &str,
    prev_context: Option<&ContextSnapshot>,
) {
    let dir = runs_dir.join(&job.run_id);
    if let Err(e) = create_private_dir(&dir).await {
        tracing::warn!(run_id = %job.run_id, error = %e, "persistence: create run dir failed");
        return;
    }
    append_run_archive(&dir, job, machine_id, world_id, prev_context).await;
    let meta_json = serde_json::to_string_pretty(&job.meta).expect("RunMeta always serializes");
    write_bytes_atomic(&dir.join("meta.json"), meta_json.as_bytes(), &job.run_id).await;
    let ctx_json =
        serde_json::to_string_pretty(&job.context).expect("ContextSnapshot always serializes");
    write_bytes_atomic(&dir.join("context.json"), ctx_json.as_bytes(), &job.run_id).await;

    // The answer's bytes, beside the descriptor `meta.json` carries. Written
    // raw, so serving it is a read and `lev result --raw` is a copy.
    if let Some(content) = &job.final_output {
        write_bytes_atomic(
            &dir.join(leviath_core::FINAL_OUTPUT_FILE),
            content.as_bytes(),
            &job.run_id,
        )
        .await;
    }

    // Per-stage index (names/status), rewritten whole; empty ⇒ agent has no ledger.
    if !job.stages.is_empty() {
        let stages_json =
            serde_json::to_string_pretty(&job.stages).expect("StageRecord slice always serializes");
        write_bytes_atomic(
            &dir.join("stages.json"),
            stages_json.as_bytes(),
            &job.run_id,
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
            json.as_bytes(),
            &job.run_id,
        )
        .await;
    }
    // Fan-out waiting state (whole-file), or remove any stale file once the
    // parent is no longer parked on a fan-out.
    let fanout_path = dir.join("fanout.json");
    match &job.fanout {
        Some(json) => write_bytes_atomic(&fanout_path, json.as_bytes(), &job.run_id).await,
        None => {
            let _ = tokio::fs::remove_file(&fanout_path).await;
        }
    }
    // Interaction-point waiting state (whole-file), or remove any stale file once the
    // agent is no longer parked at a stage-boundary interaction point.
    let interactions_path = dir.join("interactions.json");
    match &job.interactions {
        Some(json) => write_bytes_atomic(&interactions_path, json.as_bytes(), &job.run_id).await,
        None => {
            let _ = tokio::fs::remove_file(&interactions_path).await;
        }
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
/// The archive always folds to the run's latest resumable state. Best-effort -
/// a failed write is logged and swallowed like the rest of the persistence lane.
async fn append_run_archive(
    dir: &Path,
    job: &PersistJob,
    machine_id: &str,
    world_id: &str,
    prev_context: Option<&ContextSnapshot>,
) {
    use leviath_core::run_archive::{self, RunIdentity, RunRecord};

    let path = dir.join("run.lvr");
    let file_exists = tokio::fs::try_exists(&path).await.unwrap_or(false);
    let at = job.meta.updated_at;

    // Encode into a buffer via the sync codec, then append in one async write.
    let mut buf: Vec<u8> = Vec::new();
    match prev_context {
        // A known prior context → the compact per-step record.
        Some(prev) => {
            let progress = RunRecord::Progress {
                meta: Box::new(job.meta.clone()),
                delta: run_archive::diff_context(prev, &job.context),
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

    match open_private_append(&path).await {
        Ok(mut file) => {
            let _ = file.write_all(&buf).await;
            let _ = file.flush().await;
        }
        Err(e) => {
            tracing::warn!(run_id = %job.run_id, error = %e, "persistence: run archive append failed");
        }
    }
}

/// Append one line (with a trailing newline) to `stages/<idx>/<file>` under the
/// run dir, creating the stage directory if needed. Best-effort: a failed
/// `create_dir_all` just makes the subsequent open fail, and the append write
/// result is intentionally ignored - persistence must never stall the world.
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
/// filesystem, so a reader never sees a half-written file). Best-effort.
async fn write_bytes_atomic(path: &Path, bytes: &[u8], run_id: &str) {
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
    let (tmp_for_write, bytes) = (tmp.clone(), bytes.to_vec());
    let written =
        tokio::task::spawn_blocking(move || leviath_sys::write_private(&tmp_for_write, &bytes))
            .await;
    if let Err(e) = written.map_err(vanished_task).and_then(|r| r) {
        tracing::warn!(run_id = %run_id, error = %e, "persistence: temp write failed");
        return;
    }
    if let Err(e) = tokio::fs::rename(&tmp, path).await {
        tracing::warn!(run_id = %run_id, error = %e, "persistence: rename failed");
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

        persistence_worker(Some(dir.path().to_path_buf()), rx).await;

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
                        content: format!("line {i}"),
                        tokens: 1,
                        kind: leviath_core::region::EntryKind::Text,
                        metadata: None,
                        key: None,
                        taint: Default::default(),
                    })
                    .collect(),
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
        write_snapshot(dir.path(), &j, "m", "w", None).await;

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
        write_snapshot(dir.path(), &j, "m", "w", None).await;

        let written = std::fs::read_to_string(
            dir.path()
                .join("run-answer")
                .join(leviath_core::FINAL_OUTPUT_FILE),
        )
        .expect("sidecar written");
        assert_eq!(written, "metric,value\nrows,2\n");
    }

    /// A job with no answer writes no sidecar, rather than an empty file that
    /// would read as "the agent answered with nothing".
    #[tokio::test]
    async fn no_answer_writes_no_sidecar() {
        let dir = tempfile::tempdir().expect("temp dir");
        write_snapshot(dir.path(), &job("run-silent"), "m", "w", None).await;
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

        write_snapshot(dir.path(), &job("run-perms"), "m", "w", None).await;
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
        write_snapshot(dir.path(), &job("run-1"), "machine-x", "world-y", None).await;

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
        write_snapshot(dir.path(), &first, "m", "w", None).await;
        write_snapshot(dir.path(), &second, "m", "w", Some(&first.context)).await;

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
        write_snapshot(dir.path(), &job("run-1"), "m1", "w1", None).await;
        // A "restarted" worker (no prior context) writes to the existing archive:
        // it records an ownership handoff + a fresh context re-anchor, not a Header.
        write_snapshot(dir.path(), &job("run-1"), "m2", "w2", None).await;

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
        assert!(is_terminal_run(&RunStatus::Complete));
        assert!(is_terminal_run(&RunStatus::Error));
        assert!(is_terminal_run(&RunStatus::Cancelled));
        assert!(!is_terminal_run(&RunStatus::Running));
        assert!(!is_terminal_run(&RunStatus::CompleteInteractive));
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
        persistence_worker(Some(dir.path().to_path_buf()), rx).await;
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
        persistence_worker(None, rx).await;
        assert_eq!(ack_rx.await, Ok(()));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn write_snapshot_writes_then_removes_fanout_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run-1").join("fanout.json");
        // A job carrying fan-out state writes fanout.json.
        let mut fo_job = job("run-1");
        fo_job.fanout = Some(r#"{"resume":"me"}"#.to_string());
        write_snapshot(dir.path(), &fo_job, "m", "w", None).await;
        assert!(path.exists());
        // A later job without fan-out state removes the now-stale file.
        write_snapshot(dir.path(), &job("run-1"), "m", "w", Some(&fo_job.context)).await;
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn run_archive_open_failure_is_swallowed() {
        // Pre-create `run.lvr` as a directory so opening it for append fails; the
        // best-effort archive write must not panic (and the rest still runs).
        let dir = tempfile::tempdir().unwrap();
        let run_dir = dir.path().join("run-1");
        std::fs::create_dir_all(run_dir.join("run.lvr")).unwrap();
        write_snapshot(dir.path(), &job("run-1"), "m", "w", None).await;
        // meta.json still written despite the archive failure.
        assert!(run_dir.join("meta.json").exists());
    }

    fn batch_record(iteration: usize, call_id: &str) -> leviath_core::run_archive::RunRecord {
        leviath_core::run_archive::RunRecord::ToolBatch {
            calls: vec![leviath_core::run_archive::ToolCallRecord {
                id: call_id.to_string(),
                name: "shell".to_string(),
                arguments: "{}".to_string(),
                result: None,
                thought_signature: None,
            }],
            at: 1,
            stage_index: 0,
            iteration,
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
                result: "ran".to_string(),
                at: 2,
            }),
            ack: None, // the fire-and-forget per-call path
        })
        .unwrap();
        drop(tx);
        persistence_worker(Some(dir.path().to_path_buf()), rx).await;

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
        persistence_worker(Some(dir.path().to_path_buf()), rx).await;

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
        append_record(dir.path(), "run-1", &batch_record(0, "c1")).await;
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
        )
        .await;
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("the plan"));

        // A later job that is no longer parked removes the stale sidecar.
        write_snapshot(dir.path(), &job("r"), "machine-test", "world-test", None).await;
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
        )
        .await;

        // context.json still written despite the meta.json rename conflict.
        assert!(run_dir.join("context.json").exists());
    }
}
