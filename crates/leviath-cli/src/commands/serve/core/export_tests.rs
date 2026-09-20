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
        q: None,
        sources: vec![Source::Meta, Source::Files],
        sources_raw: String::new(),
        fields: fields.map(|named| named.into_iter().map(str::to_string).collect()),
        ids: None,
        since: None,
        parent: ParentFilter::Any,
        blueprint: None,
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
