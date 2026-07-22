//! The async I/O lane for agent-state persistence.
//!
//! The snapshot-dispatch system builds a [`PersistJob`] (an agent's `meta.json` +
//! `context.json` value snapshot) whenever the agent meaningfully changes and
//! sends it to this single-worker lane. [`persistence_worker`] writes each job's
//! files under `<runs_dir>/<run_id>/` **one at a time**, so writes for a given
//! agent never race or land out of order. Each file is written to a temp path and
//! atomically renamed into place, so a concurrent reader (the dashboard) never
//! sees a half-written file. All errors are logged and swallowed — persistence is
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
}

/// The single-lane persistence worker: writes each [`PersistJob`]'s files under
/// `runs_dir`, one at a time, until the job channel closes (world shutdown).
///
/// It also owns this daemon's run-ownership identity — a stable per-machine id
/// (`<runs_dir>/../machine-id`, created once) and a per-process `world_id` — which
/// it stamps into every run's portable archive so a run copied to another machine
/// is unambiguously attributable (see [`leviath_core::run_archive`]).
pub async fn persistence_worker(runs_dir: PathBuf, mut jobs: UnboundedReceiver<PersistJob>) {
    let machine_id = load_or_create_machine_id(&runs_dir);
    let world_id = generate_id();
    while let Some(job) = jobs.recv().await {
        write_snapshot(&runs_dir, &job, &machine_id, &world_id).await;
    }
}

/// A short opaque id derived from the current time + pid. Not cryptographic — it
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
async fn write_snapshot(runs_dir: &Path, job: &PersistJob, machine_id: &str, world_id: &str) {
    let dir = runs_dir.join(&job.run_id);
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        tracing::warn!(run_id = %job.run_id, error = %e, "persistence: create run dir failed");
        return;
    }
    append_run_archive(&dir, job, machine_id, world_id).await;
    let meta_json = serde_json::to_string_pretty(&job.meta).expect("RunMeta always serializes");
    write_bytes_atomic(&dir.join("meta.json"), meta_json.as_bytes(), &job.run_id).await;
    let ctx_json =
        serde_json::to_string_pretty(&job.context).expect("ContextSnapshot always serializes");
    write_bytes_atomic(&dir.join("context.json"), ctx_json.as_bytes(), &job.run_id).await;

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
        let _ = tokio::fs::create_dir_all(&stage_dir).await;
        write_bytes_atomic(
            &stage_dir.join("taint_audit.json"),
            json.as_bytes(),
            &job.run_id,
        )
        .await;
    }
}

/// Append this snapshot to the run's portable archive (`<run_dir>/run.lvr`).
///
/// The first write for a run lays down the archive preamble + a `Header` (the
/// run's identity + metadata); every write (including the first) appends a
/// `Checkpoint` carrying the full metadata + context window, so the archive
/// always folds to the run's latest resumable state. Best-effort — a failed
/// write is logged and swallowed like the rest of the persistence lane.
///
/// (Context is carried in full per checkpoint for now; storing it as a
/// [`run_archive::ContextDiff`] between checkpoints is a follow-up efficiency.)
async fn append_run_archive(dir: &Path, job: &PersistJob, machine_id: &str, world_id: &str) {
    use leviath_core::run_archive::{self, RunIdentity, RunRecord};

    let path = dir.join("run.lvr");
    let is_new = !tokio::fs::try_exists(&path).await.unwrap_or(false);

    // Encode into a buffer via the sync codec, then append in one async write.
    let mut buf: Vec<u8> = Vec::new();
    if is_new {
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
    let checkpoint = RunRecord::Checkpoint {
        meta: Box::new(job.meta.clone()),
        context: job.context.clone(),
        at: job.meta.updated_at,
    };
    run_archive::write_record(&mut buf, &checkpoint).expect("writing to a Vec never fails");

    match tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await
    {
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
/// result is intentionally ignored — persistence must never stall the world.
async fn append_stage_line(run_dir: &Path, stage_idx: usize, file: &str, line: &str, run_id: &str) {
    let stage_dir = run_dir.join("stages").join(stage_idx.to_string());
    let _ = tokio::fs::create_dir_all(&stage_dir).await;
    match tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(stage_dir.join(file))
        .await
    {
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
    if let Err(e) = tokio::fs::write(&tmp, bytes).await {
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
        tx.send(PersistJob {
            run_id: "run-1".to_string(),
            meta: meta("run-1"),
            context: context(),
            stages: vec![],
            output_appends: vec![],
            log_appends: vec![],
            taint_audit: None,
        })
        .unwrap();
        drop(tx); // close so the worker loop ends

        persistence_worker(dir.path().to_path_buf(), rx).await;

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
        }
    }

    #[tokio::test]
    async fn write_snapshot_creates_a_readable_run_archive() {
        use leviath_core::run_archive::{RunRecord, fold, read_archive};
        let dir = tempfile::tempdir().unwrap();
        write_snapshot(dir.path(), &job("run-1"), "machine-x", "world-y").await;

        let bytes = std::fs::read(dir.path().join("run-1").join("run.lvr")).unwrap();
        let (version, records) = read_archive(&mut bytes.as_slice()).unwrap();
        assert_eq!(version, leviath_core::run_archive::RUN_ARCHIVE_VERSION);
        // First write lays down a Header then a Checkpoint.
        assert!(
            records
                .iter()
                .any(|r| matches!(r, RunRecord::Header { .. }))
        );
        assert!(
            records
                .iter()
                .any(|r| matches!(r, RunRecord::Checkpoint { .. }))
        );
        // The archive folds to the run's identity + state.
        let folded = fold(&records).unwrap();
        assert_eq!(folded.identity.run_id, "run-1");
        assert_eq!(folded.identity.machine_id, "machine-x");
        assert_eq!(folded.identity.world_id, "world-y");
        assert_eq!(folded.meta.run_id, "run-1");
    }

    #[tokio::test]
    async fn run_archive_appends_a_checkpoint_per_write_with_one_header() {
        use leviath_core::run_archive::{RunRecord, read_archive};
        let dir = tempfile::tempdir().unwrap();
        write_snapshot(dir.path(), &job("run-1"), "m", "w").await;
        write_snapshot(dir.path(), &job("run-1"), "m", "w").await;

        let bytes = std::fs::read(dir.path().join("run-1").join("run.lvr")).unwrap();
        let (_v, records) = read_archive(&mut bytes.as_slice()).unwrap();
        let headers = records
            .iter()
            .filter(|r| matches!(r, RunRecord::Header { .. }))
            .count();
        let checkpoints = records
            .iter()
            .filter(|r| matches!(r, RunRecord::Checkpoint { .. }))
            .count();
        assert_eq!(headers, 1, "header only written once");
        assert_eq!(checkpoints, 2, "a checkpoint per write");
    }

    #[tokio::test]
    async fn run_archive_open_failure_is_swallowed() {
        // Pre-create `run.lvr` as a directory so opening it for append fails; the
        // best-effort archive write must not panic (and the rest still runs).
        let dir = tempfile::tempdir().unwrap();
        let run_dir = dir.path().join("run-1");
        std::fs::create_dir_all(run_dir.join("run.lvr")).unwrap();
        write_snapshot(dir.path(), &job("run-1"), "m", "w").await;
        // meta.json still written despite the archive failure.
        assert!(run_dir.join("meta.json").exists());
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
            },
            "machine-test",
            "world-test",
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
            },
            "machine-test",
            "world-test",
        )
        .await;
        let audit =
            std::fs::read_to_string(dir.path().join("r/stages/2/taint_audit.json")).unwrap();
        assert!(audit.contains("shell"));
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
            },
            "machine-test",
            "world-test",
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
        // runs_dir points at a *file*, so create_dir_all fails — must not panic.
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
            },
            "machine-test",
            "world-test",
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
            },
            "machine-test",
            "world-test",
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
            },
            "machine-test",
            "world-test",
        )
        .await;

        // context.json still written despite the meta.json rename conflict.
        assert!(run_dir.join("context.json").exists());
    }
}
