use super::*;
use crate::runfile::reader_tests::{code, initial, spec};
use crate::runfile::{CheckpointPolicy, RunFileWriter};
use crate::state::{FinalOutputState, RunStatus};

/// A run file holding `spec()` from `initial()`, in a temp dir.
fn written() -> (tempfile::TempDir, std::path::PathBuf, RunFileWriter) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(leviath_core::files::RUN_FILE);
    let writer = RunFileWriter::create(
        &path,
        &spec(),
        &code(),
        &initial(),
        CheckpointPolicy::default(),
    )
    .unwrap();
    (dir, path, writer)
}

/// A run that has taken no step yet is listed as it was resolved: its id,
/// its blueprint, and its creation time as when it last moved.
#[test]
fn a_run_with_no_steps_is_listed_as_resolved() {
    let (_dir, path, _writer) = written();
    let meta = summary(&RunFileReader::open(&path).unwrap()).unwrap();
    let spec = spec();
    assert_eq!(meta.run_id, spec.run_id.as_str());
    assert_eq!(meta.updated_at, spec.created_at);
    assert_eq!(meta.num_stages, spec.graph.stages.len());
    assert!(meta.final_output.is_none());
}

/// A run that has moved is listed as of its last step: its status, its
/// title, its answer, and when that step was taken.
#[test]
fn a_run_is_listed_as_of_its_last_step() {
    let (_dir, path, mut writer) = written();
    let mut next = writer.state().clone();
    next.status = RunStatus::Error("it broke".to_string());
    next.title = Some("A title".to_string());
    next.totals.spend.prompt_tokens = 42;
    next.final_output = Some(FinalOutputState {
        content: "the answer".to_string(),
        format: None,
        stage: next.cursor.stage.clone(),
        submitted_at: 7,
        truncated: false,
    });
    writer.record(next, 1234, Vec::new()).unwrap();

    let meta = summary(&RunFileReader::open(&path).unwrap()).unwrap();
    assert_eq!(meta.status, leviath_core::run_meta::RunStatus::Error);
    assert_eq!(meta.error.as_deref(), Some("it broke"));
    assert_eq!(meta.title.as_deref(), Some("A title"));
    assert_eq!(meta.prompt_tokens, 42);
    assert_eq!(meta.updated_at, 1234);
    assert_eq!(meta.final_output.unwrap().submitted_at, 7);
}
