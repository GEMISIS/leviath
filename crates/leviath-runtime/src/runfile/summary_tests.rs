use super::*;
use crate::runfile::reader_tests::{code, initial, read, spec, spec_frame};
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
        artifacts: Vec::new(),
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

/// A run of a blueprint read from its directory names the file it ran, and
/// a parked run says why it is parked.
#[test]
fn a_run_names_its_blueprint_file_and_why_it_waits() {
    use crate::spec::names::{BlueprintName, BlueprintPath};
    let mut spec = spec();
    let dir = std::env::temp_dir();
    spec.origin = crate::spec::run_spec::SpecOrigin::BlueprintFile {
        path: BlueprintPath::new(dir.to_string_lossy()).unwrap(),
        name: BlueprintName::new("coder").unwrap(),
        digest: None,
        version: "1.0.0".to_string(),
    };
    let mut state = initial();
    state.status = RunStatus::Waiting;
    state.wait_reason = Some(crate::state::WaitState::Children(2));
    let meta = summary_of(&spec, &state, 5);
    assert_eq!(
        std::path::PathBuf::from(&meta.agent_path),
        dir.join(leviath_core::files::BLUEPRINT_MANIFEST)
    );
    assert_eq!(
        meta.waiting_on,
        Some(leviath_core::run_meta::WaitReason::Children { outstanding: 2 })
    );
}

/// A run file whose state does not decode lists nothing, and says why.
#[test]
fn a_run_file_whose_state_does_not_decode_is_an_error() {
    use crate::runfile::codec;
    use crate::runfile::codec::FrameKind;
    let state = codec::encode(FrameKind::State, &0u64).unwrap();
    let reader = read(&[spec_frame(), state]).unwrap();
    assert!(summary(&reader).is_err());
}

/// The record at any step is the one the last step would give for that
/// state, and the window and ledger read as the live run would show them.
#[test]
fn any_step_reads_as_its_record_window_and_ledger() {
    let spec = spec();
    let states = crate::runfile::reader_tests::scripted_run(3);
    let last = states.last().unwrap();
    let meta = summary_of(&spec, last, 99);
    assert_eq!(meta.updated_at, 99);
    assert_eq!(meta.run_id, spec.run_id.as_str());

    let window = context_snapshot(&spec, last);
    assert_eq!(window.stage_name, last.cursor.stage.as_str());
    let names: Vec<&str> = window.regions.iter().map(|r| r.name.as_str()).collect();
    assert!(names.contains(&"conversation"), "{names:?}");
    let conversation = window
        .regions
        .iter()
        .find(|r| r.name == "conversation")
        .unwrap();
    assert_eq!(
        conversation.entries.len(),
        last.context.region("conversation").unwrap().entries.len()
    );

    let ledger = stage_records(last);
    assert_eq!(ledger.len(), last.ledger.len());
    assert_eq!(ledger[0].name, "plan");
}
