//! Tests for the export job.

use super::{EXPORT_TTL_SECS, ExportStatus, Exports, export_path, exports_dir, start};
use crate::commands::serve::core::runs::{ParentFilter, RunSelection, SortKey, Source};
use crate::commands::serve::testutil::state_with_agent_paths;
use crate::runstate::{RunMeta, create_run};

/// A run on disk, started at a known second.
fn meta_at(id: &str, started_at: i64) -> RunMeta {
    let mut meta = RunMeta::new(
        id.to_string(),
        "test-agent".to_string(),
        "/agents/test".to_string(),
        "do the thing".to_string(),
        None,
        "/work".to_string(),
        1,
    );
    meta.started_at = started_at;
    meta.updated_at = started_at;
    meta
}

/// Everything, with the given field projection.
fn all_runs(fields: Option<Vec<&str>>) -> RunSelection {
    RunSelection {
        limit: usize::MAX,
        statuses: Vec::new(),
        sort: SortKey::Started,
        descending: true,
        order: None,
        q: None,
        sources: vec![Source::Meta, Source::Files],
        sources_raw: String::new(),
        fields: fields.map(|named| named.into_iter().map(str::to_string).collect()),
        ids: None,
        since: None,
        parent: ParentFilter::Any,
        blueprint: None,
        predicate: None,
        preloaded: None,
    }
}

/// The fields a run record can carry, for the projection check.
fn known() -> std::collections::HashSet<String> {
    ["run_id", "status", "started_at"]
        .into_iter()
        .map(str::to_string)
        .collect()
}

/// Wait for a job to settle, so the assertions read a finished export.
async fn settled(exports: &Exports, id: &str) -> super::ExportJob {
    for _ in 0..200 {
        let job = exports.get(id).expect("the job is recorded");
        if job.status != ExportStatus::Queued && job.status != ExportStatus::Running {
            return job;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("the export did not finish");
}

/// An export writes one JSON object per line, and says how many it wrote.
///
/// JSONL rather than one JSON array: a reader can start on the file before the
/// writer has finished, and the store can be larger than memory.
#[tokio::test]
async fn an_export_writes_one_run_per_line() {
    crate::runstate::with_isolated_runs_dir_async("export-writes", |_d| async move {
        for i in 0..3 {
            create_run(&meta_at(&format!("run-{i}"), 100 + i)).expect("run written");
        }
        let state = state_with_agent_paths(Vec::new());

        let job = start(&state, all_runs(None).resolve(None).expect("a spec"), known)
            .await
            .expect("the export starts");
        let finished = settled(&state.caches.exports, &job.id).await;

        assert_eq!(finished.status, ExportStatus::Complete);
        assert_eq!(finished.written, 3);
        assert!(finished.error.is_none());
        let written = std::fs::read_to_string(export_path(&job.id)).expect("the file");
        let lines: Vec<&str> = written.lines().collect();
        assert_eq!(lines.len(), 3, "one run per line");
        for line in lines {
            let row: serde_json::Value = serde_json::from_str(line).expect("each line is JSON");
            assert!(row["run_id"].is_string(), "{row}");
        }
    })
    .await;
}

/// A filter narrows the export the same way it narrows a listing: one predicate
/// for both.
#[tokio::test]
async fn an_export_honours_the_listings_filter() {
    crate::runstate::with_isolated_runs_dir_async("export-filter", |_d| async move {
        create_run(&meta_at("root", 100)).expect("run written");
        let mut child = meta_at("worker", 200);
        child.parent_run_id = Some("root".to_string());
        create_run(&child).expect("run written");
        let state = state_with_agent_paths(Vec::new());

        let mut only_children = all_runs(None);
        only_children.parent = ParentFilter::Of("root".to_string());
        let job = start(&state, only_children.resolve(None).expect("a spec"), known)
            .await
            .expect("the export starts");
        let finished = settled(&state.caches.exports, &job.id).await;

        assert_eq!(finished.written, 1);
        let written = std::fs::read_to_string(export_path(&job.id)).expect("the file");
        let row: serde_json::Value =
            serde_json::from_str(written.lines().next().expect("a line")).expect("JSON");
        assert_eq!(row["run_id"], "worker", "the child, not its parent");
    })
    .await;
}

/// A projection keeps the named fields, and the id whatever else is asked for.
#[tokio::test]
async fn a_projection_narrows_each_row() {
    crate::runstate::with_isolated_runs_dir_async("export-projection", |_d| async move {
        create_run(&meta_at("run-a", 100)).expect("run written");
        let state = state_with_agent_paths(Vec::new());

        let job = start(
            &state,
            all_runs(Some(vec!["run_id", "status"]))
                .resolve(None)
                .expect("a spec"),
            known,
        )
        .await
        .expect("the export starts");
        settled(&state.caches.exports, &job.id).await;

        let written = std::fs::read_to_string(export_path(&job.id)).expect("the file");
        let row: serde_json::Value =
            serde_json::from_str(written.lines().next().expect("a line")).expect("JSON");
        let keys: Vec<&String> = row.as_object().expect("an object").keys().collect();
        assert_eq!(keys.len(), 2, "only what was asked for: {keys:?}");
        assert!(row["run_id"].is_string());
        assert!(row["status"].is_string());
    })
    .await;
}

/// An unknown field is refused before anything is written.
///
/// A column quietly missing from an export is discovered downstream, by
/// somebody who did not ask for it.
#[tokio::test]
async fn an_unknown_field_is_refused() {
    crate::runstate::with_isolated_runs_dir_async("export-unknown", |_d| async move {
        let state = state_with_agent_paths(Vec::new());
        let failure = start(
            &state,
            all_runs(Some(vec!["run_id", "no_such_field"]))
                .resolve(None)
                .expect("a spec"),
            known,
        )
        .await
        .expect_err("the field does not exist");
        assert_eq!(failure.code(), "BAD_USER_INPUT");
        assert!(failure.to_string().contains("no_such_field"), "{failure}");
    })
    .await;
}

/// An export that has aged out is forgotten, file and record together.
///
/// Neither outliving the other is the point: a record pointing at a file that is
/// gone would hand out a link to nothing.
#[tokio::test]
async fn an_aged_out_export_is_swept_with_its_file() {
    crate::runstate::with_isolated_runs_dir_async("export-sweep", |_d| async move {
        create_run(&meta_at("run-a", 100)).expect("run written");
        let state = state_with_agent_paths(Vec::new());

        let old = start(&state, all_runs(None).resolve(None).expect("a spec"), known)
            .await
            .expect("the export starts");
        settled(&state.caches.exports, &old.id).await;
        let path = export_path(&old.id);
        assert!(path.exists(), "the file is there");

        // Age the record past the window, then start another export: the sweep
        // runs when one does, because an export is the only thing that makes
        // these.
        state
            .caches
            .exports
            .sweep(&exports_dir(), old.started_at + EXPORT_TTL_SECS + 1);

        assert!(state.caches.exports.get(&old.id).is_none(), "record gone");
        assert!(!path.exists(), "and its file with it");
    })
    .await;
}

/// A status word per state, because a client renders these.
#[test]
fn each_status_has_its_own_word() {
    assert_eq!(ExportStatus::Queued.wire(), "queued");
    assert_eq!(ExportStatus::Running.wire(), "running");
    assert_eq!(ExportStatus::Complete.wire(), "complete");
    assert_eq!(ExportStatus::Failed.wire(), "failed");
}

/// An unknown id is nothing rather than an error: an export that expired and
/// one that never existed look the same, and both mean "ask again".
#[test]
fn an_unknown_export_is_simply_absent() {
    let exports = Exports::default();
    assert!(exports.get("export-1-1").is_none());
}

/// The registry says how many jobs it is holding, for a log line.
#[test]
fn the_registry_describes_itself() {
    let exports = Exports::default();
    let empty = format!("{exports:?}");
    assert!(empty.contains("jobs: 0"), "{empty}");
    let state = state_with_agent_paths(Vec::new());
    super::test_job(&state, ExportStatus::Queued, "");
    let holding = format!("{:?}", state.caches.exports);
    assert!(holding.contains("jobs: 1"), "{holding}");
}

/// An update to a job nobody recorded is a no-op rather than a panic.
///
/// The worker holds an id and the registry may have been swept underneath it, so
/// the write has to tolerate an id that is gone.
#[test]
fn updating_a_job_that_is_gone_does_nothing() {
    let exports = Exports::default();
    exports.update("export-never", &mut |job: &mut super::ExportJob| {
        job.status = ExportStatus::Complete
    });
    assert!(exports.get("export-never").is_none());
}

/// A file the writer cannot create is reported rather than reading as written.
#[tokio::test]
async fn an_export_the_filesystem_refuses_is_reported() {
    crate::runstate::with_isolated_runs_dir_async("export-refused", |_d| async move {
        create_run(&meta_at("run-a", 100)).expect("run written");
        let state = state_with_agent_paths(Vec::new());
        // A directory where the export's own file should go, which no export
        // creates but which stands in for any write the filesystem refuses.
        let dir = exports_dir();
        std::fs::create_dir_all(&dir).expect("the exports directory");
        let job = state
            .caches
            .exports
            .enqueue(leviath_core::duration::now_secs());
        std::fs::create_dir_all(export_path(&job.id)).expect("a directory in the way");

        let written = super::write_file(&export_path(&job.id), &[serde_json::json!({"a": 1})]);
        assert!(written.is_err(), "a directory is not a file to write");
    })
    .await;
}

/// Two exports in one second are two jobs, because the id carries a counter as
/// well as the clock.
#[test]
fn two_exports_in_one_second_are_two_jobs() {
    let exports = Exports::default();
    let first = exports.enqueue(100);
    let second = exports.enqueue(100);
    assert_ne!(first.id, second.id, "one second, two jobs");
    assert!(exports.get(&first.id).is_some());
    assert!(exports.get(&second.id).is_some());
}

/// A job that has not aged out is kept, file and record.
#[tokio::test]
async fn a_recent_export_survives_the_sweep() {
    crate::runstate::with_isolated_runs_dir_async("export-kept", |_d| async move {
        let state = state_with_agent_paths(Vec::new());
        let job = super::test_job(&state, ExportStatus::Complete, "{}\n");
        let path = export_path(&job);
        state
            .caches
            .exports
            .sweep(&exports_dir(), leviath_core::duration::now_secs());
        assert!(state.caches.exports.get(&job).is_some(), "still recorded");
        assert!(path.exists(), "and its file is still there");
    })
    .await;
}

/// An export whose write fails records the failure on the job.
///
/// The request answered before the file existed, so the only place a client can
/// learn the write broke is the job it is polling: a job stuck on `running`
/// forever would read as a slow export.
#[tokio::test]
async fn an_export_that_cannot_be_written_records_why() {
    crate::runstate::with_isolated_runs_dir_async("export-write-fails", |_d| async move {
        create_run(&meta_at("run-a", 100)).expect("run written");
        let state = state_with_agent_paths(Vec::new());
        // A directory standing where the next export's file will go. The id is
        // the clock plus a counter, so the one this export will take is known.
        std::fs::create_dir_all(exports_dir()).expect("the exports directory");
        let now = leviath_core::duration::now_secs();
        std::fs::create_dir_all(exports_dir().join(format!("export-{now}-0.jsonl")))
            .expect("a directory in the way");

        let job = start(&state, all_runs(None).resolve(None).expect("a spec"), known)
            .await
            .expect("the export starts");
        let finished = settled(&state.caches.exports, &job.id).await;
        assert_eq!(finished.status, ExportStatus::Failed);
        assert!(
            finished.error.as_deref().is_some_and(|why| !why.is_empty()),
            "the job says why: {finished:?}"
        );
    })
    .await;
}

/// A runs directory at the filesystem root has nowhere beside it, so the
/// exports go inside it.
///
/// Not a path any install takes: `LEVIATH_RUNS_DIR=/` is the shape, and the
/// answer is a directory that exists rather than one built from an empty parent.
#[test]
fn a_runs_directory_with_no_parent_keeps_its_exports_inside() {
    let at_root = super::exports_dir_beside(std::path::Path::new("/"));
    assert_eq!(at_root, std::path::Path::new("/exports"));
}

/// Where exports go is beside the run store rather than inside it.
///
/// Inside it, an export would be a run directory to every walk of the store: the
/// listing would count it, and a sweep by age would consider deleting it.
#[test]
fn exports_live_beside_the_run_store() {
    let dir = exports_dir();
    assert!(dir.ends_with("exports"), "{}", dir.display());
    assert_ne!(dir, crate::runstate::runs_dir());
}

/// An exports directory that cannot be made is reported before a job is
/// recorded.
///
/// A job with no directory to write into would sit at `queued` forever, which is
/// indistinguishable from a slow export.
#[tokio::test]
async fn an_exports_directory_that_cannot_be_made_is_reported() {
    crate::runstate::with_isolated_runs_dir_async("export-no-dir", |_d| async move {
        let state = state_with_agent_paths(Vec::new());
        // A file standing where the directory goes, which nothing creates but
        // which stands in for a directory the filesystem refuses.
        let dir = exports_dir();
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent).expect("the data directory");
        }
        std::fs::write(&dir, "not a directory").expect("a file in the way");

        let failure = start(&state, all_runs(None).resolve(None).expect("a spec"), known)
            .await
            .expect_err("there is nowhere to write");
        assert_eq!(failure.code(), "INTERNAL");
        assert!(
            failure.to_string().contains("exports directory"),
            "{failure}"
        );
    })
    .await;
}

/// Writing no rows is a file with nothing in it rather than no file.
///
/// A client that asked for an export of a filter nothing matches gets an empty
/// file and a `written` of zero, which is an answer; a missing file would read
/// as a failure.
#[test]
fn an_export_of_nothing_is_an_empty_file() {
    let dir = tempfile::tempdir().expect("a directory");
    let path = dir.path().join("empty.jsonl");
    let written = super::write_file(&path, &[]).expect("it writes");
    assert_eq!(written, 0);
    assert_eq!(std::fs::read_to_string(&path).expect("the file"), "");
}

/// A write that fails partway is reported rather than counted as written.
///
/// A client only ever learns about this from the job, so the worker has to hear
/// it: the request was answered before the file existed.
#[test]
fn a_writer_that_gives_up_is_reported() {
    /// A writer that takes one line and then refuses, standing in for a disk
    /// that filled up mid-export.
    struct OneLine {
        written: usize,
    }

    impl std::io::Write for OneLine {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.written += buf.len();
            match self.written > 8 {
                true => Err(std::io::Error::other("no space left")),
                false => Ok(buf.len()),
            }
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let rows: Vec<serde_json::Value> = (0..4)
        .map(|i| serde_json::json!({ "run_id": format!("run-{i}") }))
        .collect();
    let failed = super::write_rows(&mut OneLine { written: 0 }, &rows)
        .expect_err("the writer gave up partway");
    assert_eq!(failed.to_string(), "no space left");

    // The separator is its own write, so a disk that fills between the row and
    // the newline leaves a file whose last line is not a line. One row short
    // enough to fit, whose newline does not.
    let failed = super::write_rows(
        &mut OneLine { written: 0 },
        &[serde_json::json!({ "ab": 1 })],
    )
    .expect_err("the separator did not fit");
    assert_eq!(failed.to_string(), "no space left");
}

/// A writer that accepts everything and refuses to flush is reported too.
///
/// The bytes are buffered, so a flush is where a full disk usually announces
/// itself: an export that ignored it would report a complete file with its tail
/// missing.
#[test]
fn a_flush_that_fails_is_reported() {
    struct NeverFlushes;

    impl std::io::Write for NeverFlushes {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("the disk went away"))
        }
    }

    let failed = super::write_rows(&mut NeverFlushes, &[serde_json::json!({ "run_id": "a" })])
        .expect_err("the flush failed");
    assert_eq!(failed.to_string(), "the disk went away");
}
