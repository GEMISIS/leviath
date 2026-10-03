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
        bytes: 10,
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
    let answer = meta.final_output.unwrap();
    assert_eq!((answer.submitted_at, answer.bytes), (7, 10));
}

/// Why a run has no title, and how many of its read paths were granted, are
/// listed from the run file as they were from the live run.
#[test]
fn a_run_lists_its_title_error_and_read_path_grants() {
    let (_dir, path, mut writer) = written();
    let mut next = writer.state().clone();
    next.title_error = Some("mock/m failed: HTTP 403".to_string());
    next.read_paths = Some(crate::state::ReadPathCounts {
        declared: 3,
        granted: 2,
    });
    writer.record(next, 99, Vec::new()).unwrap();
    let meta = summary(&RunFileReader::open(&path).unwrap()).unwrap();
    assert_eq!(meta.title_error.as_deref(), Some("mock/m failed: HTTP 403"));
    assert_eq!(
        meta.read_paths,
        Some(leviath_core::run_meta::ReadPathGrantCounts {
            declared: 3,
            granted: 2
        })
    );
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

/// A held run is listed as waiting on the machine being put back, whatever
/// it was waiting on when it was held.
#[test]
fn a_held_run_says_the_machine_changed() {
    let mut state = initial();
    state.status = RunStatus::Waiting;
    state.wait_reason = Some(crate::state::WaitState::UserPrompt);
    let issues: crate::spec::issues::SpawnIssues = crate::spec::issues::SpawnIssue::new(
        crate::spec::issues::SpecPath::root(),
        crate::spec::issues::IssueCode::Changed,
        "an MCP server's tools changed",
    )
    .into();
    state.held = Some(issues.clone());
    let meta = summary_of(&spec(), &state, 5);
    assert_eq!(meta.waiting_on, Some(crate::restore::held_reason(&issues)));
}

/// A held run reads paused, as the daemon lists it, and so does the stage it
/// is in; the same run brought back reads as its state recorded it.
#[test]
fn a_held_run_and_its_stage_read_paused() {
    let mut state = initial();
    state.status = RunStatus::Active;
    let here = state.cursor.stage.clone();
    let mut rec = crate::insert::place::pending_stage(here);
    rec.entered = true;
    rec.status = crate::state::StageStatus::Active;
    state.ledger = vec![rec];
    let stage_word = |state: &crate::state::RunState| {
        serde_json::to_value(&stage_records(state)[0].status).unwrap()
    };
    let back = summary_of(&spec(), &state, 5);
    assert_eq!(
        (back.status, stage_word(&state)),
        (
            leviath_core::run_meta::RunStatus::Running,
            serde_json::json!("active")
        )
    );

    state.held = Some(
        crate::spec::issues::SpawnIssue::new(
            crate::spec::issues::SpecPath::root(),
            crate::spec::issues::IssueCode::Changed,
            "an MCP server's tools changed",
        )
        .into(),
    );
    let held = summary_of(&spec(), &state, 5);
    assert_eq!(
        (held.status, stage_word(&state)),
        (
            leviath_core::run_meta::RunStatus::Paused,
            serde_json::json!("paused")
        )
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

/// A run is listed as last moving at its last step unless its state keeps
/// that apart, and a run that never entered a stage names no stage.
#[test]
fn a_kept_progress_time_and_an_unentered_stage_are_listed_as_kept() {
    let mut state = initial();
    state.visits.clear();
    let meta = summary_of(&spec(), &state, 9);
    assert_eq!(meta.last_progress_at, Some(9));
    assert_eq!(meta.current_stage, "", "no stage was ever entered");

    state.last_progress_at = Some(4);
    state.visits.insert(state.cursor.stage.clone(), 1);
    let meta = summary_of(&spec(), &state, 9);
    assert_eq!((meta.last_progress_at, meta.updated_at), (Some(4), 9));
    assert_eq!(meta.current_stage, state.cursor.stage.as_str());
}

/// A run that is not held stands as recorded, and a stage recorded cancelled
/// reads cancelled.
#[test]
fn an_unheld_run_stands_as_recorded_and_a_cancelled_stage_reads_so() {
    let mut state = initial();
    state.status = RunStatus::Cancelled;
    let mut rec = crate::insert::place::pending_stage(state.cursor.stage.clone());
    rec.entered = true;
    rec.status = crate::state::StageStatus::Cancelled;
    state.ledger = vec![rec];
    assert_eq!(as_it_stands(state.clone()), state);
    assert_eq!(
        serde_json::to_value(&stage_records(&state)[0].status).unwrap(),
        serde_json::json!("cancelled")
    );
}

/// A run converted from an earlier release lists what it recorded about
/// itself for good, and how it was doing as that release listed it while it
/// stands where it was converted. Once it moves on, how it is doing lists as
/// any run's does.
#[test]
fn a_converted_run_lists_as_its_release_did_until_it_moves() {
    let mut spec = spec();
    let mut listed = crate::spec::run_spec::tests::listed();
    listed.empty_output = false;
    spec.listed = Some(listed);
    let mut state = initial();
    state.seq = 4;
    state.status = RunStatus::Complete;
    let meta = summary_of(&spec, &state, 9);
    assert_eq!(meta.model.as_deref(), Some("mock/gpt-mock"));
    assert_eq!(meta.num_stages, 2);
    assert_eq!(meta.max_child_depth, 3);
    assert_eq!(meta.blueprint_digest, Some("ab".repeat(32)));
    assert_eq!(meta.last_progress_at, Some(5));
    assert_eq!(
        meta.active.map(|a| (a.banked_secs, a.since)),
        Some((6, Some(7)))
    );
    assert!(!meta.flags.empty_output);

    state.seq = 5;
    let moved = summary_of(&spec, &state, 9);
    assert_eq!(moved.last_progress_at, Some(9));
    assert!(
        moved.flags.empty_output,
        "it changed nothing and answered nothing"
    );
    assert_eq!(moved.active.map(|a| a.banked_secs), Some(0));
    assert_eq!(moved.model.as_deref(), Some("mock/gpt-mock"));

    // A record that kept no working clock lists none.
    state.seq = 4;
    spec.listed.as_mut().unwrap().clock = None;
    assert_eq!(summary_of(&spec, &state, 9).active, None);
}

/// A cost an earlier release's record never named lists as unknown, with no
/// call counted unpriced, for the run and for each stage.
#[test]
fn a_cost_never_named_lists_as_unknown() {
    let mut state = initial();
    state.totals.spend.priced_usd = 0.0;
    state.totals.spend.cost_unknown = true;
    let mut rec = crate::insert::place::pending_stage(state.cursor.stage.clone());
    rec.entered = true;
    rec.spend.cost_unknown = true;
    state.ledger = vec![rec];
    let meta = summary_of(&spec(), &state, 9);
    assert_eq!((meta.cost_usd, meta.unpriced_calls), (None, 0));
    assert!(!meta.cost_is_exact);
    let stages = stage_records(&state);
    assert_eq!((stages[0].cost_usd, stages[0].unpriced_calls), (None, 0));
    state.totals.spend.cost_unknown = false;
    assert_eq!(summary_of(&spec(), &state, 9).cost_usd, Some(0.0));
}

/// A run whose ledger says it entered its stage names that stage, though it
/// counted no visit to it: an earlier release did not count them.
#[test]
fn a_stage_entered_without_a_counted_visit_is_named() {
    let mut state = initial();
    state.visits.clear();
    let mut rec = crate::insert::place::pending_stage(state.cursor.stage.clone());
    rec.entered = true;
    state.ledger = vec![rec];
    let meta = summary_of(&spec(), &state, 9);
    assert_eq!(meta.current_stage, state.cursor.stage.as_str());
    assert_eq!(stage_records(&state)[0].visit_count, 0);
}
