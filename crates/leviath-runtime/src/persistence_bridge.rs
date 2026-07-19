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

use leviath_core::run_meta::{ContextSnapshot, RunMeta};
use tokio::sync::mpsc::UnboundedReceiver;

/// One agent snapshot to write to disk.
pub struct PersistJob {
    /// The run id (its directory name under the runs dir).
    pub run_id: String,
    /// The `meta.json` contents.
    pub meta: RunMeta,
    /// The `context.json` contents.
    pub context: ContextSnapshot,
}

/// The single-lane persistence worker: writes each [`PersistJob`]'s files under
/// `runs_dir`, one at a time, until the job channel closes (world shutdown).
pub async fn persistence_worker(runs_dir: PathBuf, mut jobs: UnboundedReceiver<PersistJob>) {
    while let Some(job) = jobs.recv().await {
        write_snapshot(&runs_dir, &job).await;
    }
}

/// Write one job's `meta.json` + `context.json` under `<runs_dir>/<run_id>/`,
/// each via a temp file + atomic rename. Best-effort: logs and returns on any
/// error. Serialization is infallible for these plain serde structs, so a
/// serialize error is a bug rather than a runtime condition (`.expect`).
async fn write_snapshot(runs_dir: &Path, job: &PersistJob) {
    let dir = runs_dir.join(&job.run_id);
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        tracing::warn!(run_id = %job.run_id, error = %e, "persistence: create run dir failed");
        return;
    }
    let meta_json = serde_json::to_string_pretty(&job.meta).expect("RunMeta always serializes");
    write_bytes_atomic(&dir.join("meta.json"), meta_json.as_bytes(), &job.run_id).await;
    let ctx_json =
        serde_json::to_string_pretty(&job.context).expect("ContextSnapshot always serializes");
    write_bytes_atomic(&dir.join("context.json"), ctx_json.as_bytes(), &job.run_id).await;
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
            },
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
            },
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
            },
        )
        .await;

        // context.json still written despite the meta.json rename conflict.
        assert!(run_dir.join("context.json").exists());
    }
}
