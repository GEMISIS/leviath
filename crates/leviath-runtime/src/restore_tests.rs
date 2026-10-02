use super::*;
use crate::runfile::reader_tests::{scripted_run, spec, write_run};
use crate::state::{PipelinePhase, RunStatus};

#[test]
fn a_directory_without_a_run_file_has_nothing_to_resume() {
    let dir = tempfile::tempdir().unwrap();
    assert!(read_for_resume(dir.path()).unwrap().is_none());
}

#[test]
fn a_run_file_that_cannot_be_read_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(leviath_core::files::RUN_FILE), b"junk").unwrap();
    let err = read_for_resume(dir.path()).unwrap_err();
    assert!(err.to_string().contains("not a run file"));
    // A file whose spec names code it does not hold cannot be bound.
    let path = dir.path().join(leviath_core::files::RUN_FILE);
    let states = scripted_run(1);
    crate::runfile::RunFileWriter::create(
        &path,
        &spec(),
        &Default::default(),
        &states[0],
        Default::default(),
    )
    .unwrap();
    assert!(read_for_resume(dir.path()).is_err());
    // Nor can one with no state to start from.
    let mut bytes = crate::runfile::codec::header(crate::runfile::fingerprint());
    bytes.extend(
        crate::runfile::codec::encode(crate::runfile::codec::FrameKind::Spec, &spec()).unwrap(),
    );
    std::fs::write(&path, bytes).unwrap();
    assert!(read_for_resume(dir.path()).is_err());
}

#[test]
fn a_run_comes_back_from_its_file_at_its_last_step() {
    let dir = tempfile::tempdir().unwrap();
    let states = scripted_run(4);
    let path = dir.path().join(leviath_core::files::RUN_FILE);
    let written = write_run(&path, &states, Default::default());
    let run = read_for_resume(dir.path()).unwrap().expect("a run file");
    assert_eq!(run.state, *written.state());
    assert_eq!(*run.spec, spec());
    assert_eq!(run.code, crate::runfile::reader_tests::code());
    let mut world = World::new();
    let entity = resume(&mut world, run, crate::spec::env::Bindings::new());
    let placed = world.get::<crate::insert::RunSpecC>(entity).unwrap();
    assert_eq!(placed.0.run_id, spec().run_id);
}

fn resumable(depth: u8, created_at: i64, status: RunStatus, phase: PipelinePhase) -> Resumable {
    let mut spec = spec();
    spec.placement.depth = depth;
    spec.created_at = created_at;
    let mut state = scripted_run(1).remove(0);
    state.status = status;
    state.phase = phase;
    Resumable {
        spec: std::sync::Arc::new(spec),
        state,
        code: Default::default(),
        blobs: Default::default(),
        asked: 0,
    }
}

#[test]
fn every_status_and_phase_is_classified() {
    use RestorePriority::{Active, Blocked};
    let cases = [
        (RunStatus::Complete, PipelinePhase::Done, None),
        (RunStatus::Error("x".into()), PipelinePhase::Done, None),
        (RunStatus::Cancelled, PipelinePhase::Done, None),
        (RunStatus::Paused, PipelinePhase::Paused, Some(Blocked)),
        (
            RunStatus::Waiting,
            PipelinePhase::AwaitingPerson,
            Some(Blocked),
        ),
        (RunStatus::Active, PipelinePhase::FanOut, Some(Blocked)),
        (
            RunStatus::Active,
            PipelinePhase::AwaitingPerson,
            Some(Blocked),
        ),
        (RunStatus::Active, PipelinePhase::Paused, Some(Blocked)),
        (
            RunStatus::Active,
            PipelinePhase::WaitingForChildren,
            Some(Blocked),
        ),
        (
            RunStatus::Active,
            PipelinePhase::AwaitingTools,
            Some(Active),
        ),
        (RunStatus::Idle, PipelinePhase::ReadyToInfer, Some(Active)),
    ];
    for (status, phase, want) in cases {
        let run = resumable(0, 0, status.clone(), phase.clone());
        assert_eq!(classify(&run.state), want, "{status:?} / {phase:?}");
    }
}

#[test]
fn children_come_back_first_then_active_runs_then_the_newest() {
    let runs = vec![
        resumable(0, 1, RunStatus::Paused, PipelinePhase::Paused),
        resumable(0, 2, RunStatus::Active, PipelinePhase::ReadyToInfer),
        resumable(0, 3, RunStatus::Complete, PipelinePhase::Done),
        resumable(1, 0, RunStatus::Waiting, PipelinePhase::AwaitingPerson),
        resumable(0, 5, RunStatus::Active, PipelinePhase::AwaitingTools),
    ];
    let order: Vec<(u8, i64)> = triage(runs, |r| r)
        .iter()
        .map(|r| (r.spec.placement.depth, r.spec.created_at))
        .collect();
    assert_eq!(order, vec![(1, 0), (0, 5), (0, 2), (0, 1)]);
}

/// A run comes back with the files its file holds and a count of the
/// questions it put to a person: the ones answered in its history, the ones
/// still open, and the calls in flight that may ask one.
#[test]
fn a_run_comes_back_with_its_files_and_the_questions_it_asked() {
    use crate::runfile::{CheckpointPolicy, RunFileWriter};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(leviath_core::files::RUN_FILE);
    let first = scripted_run(1).remove(0);
    let mut writer = RunFileWriter::create(
        &path,
        &spec(),
        &crate::runfile::reader_tests::code(),
        &first,
        CheckpointPolicy::default(),
    )
    .unwrap();
    let digest = crate::spec::names::Digest::of(b"bytes");
    writer.add_blob(&digest, b"bytes").unwrap();
    let mut next = first.clone();
    next.interactions.push(crate::state::OpenInteraction {
        id: "q-2".into(),
        prompt: "again?".into(),
        options: Vec::new(),
    });
    next.pending = Some(crate::state::PendingBatch {
        calls: vec![crate::state::context::ToolCallState {
            id: "c1".into(),
            name: "ask_user_text".into(),
            args: Default::default(),
            thought_signature: None,
        }],
        done: Default::default(),
    });
    writer
        .record(
            next,
            1,
            vec![crate::state::RunEvent::Answered {
                id: "q-1".into(),
                answer: "yes".into(),
            }],
        )
        .unwrap();
    let run = read_for_resume(dir.path()).unwrap().expect("a run file");
    assert_eq!(run.blobs[&digest], b"bytes");
    assert_eq!(run.asked, 3);
}

/// A run file for `run` under `runs`, in `state`, whose stage requires an
/// output when `require_output` says so.
fn on_disk(
    runs: &std::path::Path,
    run: &str,
    state: &crate::state::RunState,
    require_output: bool,
) {
    let dir = runs.join(run);
    std::fs::create_dir_all(&dir).unwrap();
    let mut spec = spec();
    spec.graph.stages[0].require_output = require_output;
    crate::runfile::RunFileWriter::create(
        &dir.join(leviath_core::files::RUN_FILE),
        &spec,
        &crate::runfile::reader_tests::code(),
        state,
        Default::default(),
    )
    .unwrap();
}

/// A fan-out read back for resuming takes the workers that finished out of
/// the running ones, each with what it ended with as its own file says: its
/// output, else its last reply, unless its stage needed an output it did not
/// give; a failure or a cancel as a failure. A worker still going, or one
/// whose file does not read, stays running.
#[test]
fn a_fan_out_comes_back_with_the_workers_that_finished_settled() {
    use crate::spec::names::RunId;
    use crate::state::context::EntryKind;
    use crate::state::{FanOutState, FinalOutputState};
    let runs = tempfile::tempdir().unwrap();
    let ended = |status: RunStatus| {
        let mut s = crate::runfile::reader_tests::initial();
        s.status = status;
        s.phase = PipelinePhase::Done;
        s
    };
    let mut answered = ended(RunStatus::Complete);
    answered.final_output = Some(FinalOutputState {
        content: "the answer".into(),
        format: None,
        stage: answered.cursor.stage.clone(),
        submitted_at: 1,
        truncated: false,
        artifacts: Vec::new(),
    });
    on_disk(runs.path(), "w-answer", &answered, false);
    let mut replied = ended(RunStatus::Complete);
    let mut early = crate::runfile::reader_tests::entry("first", EntryKind::AssistantTurn(vec![]));
    early.timestamp = 1;
    let mut late = crate::runfile::reader_tests::entry("last", EntryKind::AssistantTurn(vec![]));
    late.timestamp = 2;
    replied.context.regions[1].entries = vec![late, early];
    on_disk(runs.path(), "w-reply", &replied, false);
    on_disk(runs.path(), "w-silent", &ended(RunStatus::Complete), false);
    on_disk(runs.path(), "w-required", &ended(RunStatus::Complete), true);
    on_disk(
        runs.path(),
        "w-broke",
        &ended(RunStatus::Error("boom".into())),
        false,
    );
    on_disk(
        runs.path(),
        "w-stopped",
        &ended(RunStatus::Cancelled),
        false,
    );
    on_disk(runs.path(), "w-going", &ended(RunStatus::Active), false);

    let ids = [
        "w-answer",
        "w-reply",
        "w-silent",
        "w-required",
        "w-broke",
        "w-stopped",
        "w-going",
        "w-gone",
    ];
    let mut parent = crate::runfile::reader_tests::initial();
    parent.fan_out = Some(FanOutState {
        stage: parent.cursor.stage.clone(),
        config: crate::spec::graph::FanOutDef::same_graph(parent.cursor.stage.clone()),
        max_workers: Some(8),
        queued: Vec::new(),
        active: ids
            .iter()
            .map(|id| (format!("i-{id}"), RunId::new(*id).unwrap()))
            .collect(),
        done: Vec::new(),
        failed: Vec::new(),
        paused: false,
        origin: Default::default(),
        parts: Vec::new(),
    });
    on_disk(runs.path(), "parent", &parent, false);

    let run = read_for_resume(&runs.path().join("parent"))
        .unwrap()
        .unwrap();
    let fan_out = run.state.fan_out.unwrap();
    let pairs = |v: &[(String, String)]| -> Vec<(String, String)> { v.to_vec() };
    assert_eq!(
        pairs(&fan_out.done),
        [
            ("i-w-answer".to_string(), "the answer".to_string()),
            ("i-w-reply".to_string(), "last".to_string()),
            ("i-w-silent".to_string(), String::new()),
        ]
    );
    assert_eq!(
        pairs(&fan_out.failed),
        [
            (
                "i-w-required".to_string(),
                "worker finished without the final output its stage requires".to_string()
            ),
            ("i-w-broke".to_string(), "boom".to_string()),
            ("i-w-stopped".to_string(), "worker cancelled".to_string()),
        ]
    );
    let still: Vec<&str> = fan_out.active.iter().map(|(_, r)| r.as_str()).collect();
    assert_eq!(still, ["w-going", "w-gone"]);
}

/// A call interrupted by a daemon dying gets the stand-in result; one that
/// starts sub-agents in a run that has some names them, so the model checks
/// them before starting more. A run with no batch, or one stopped on a
/// question, is left as it is.
#[test]
fn calls_interrupted_by_a_crash_get_a_result_that_says_to_check() {
    use crate::state::PendingBatch;
    use crate::state::context::ToolCallState;
    let call = |id: &str, name: &str| ToolCallState {
        id: id.to_string(),
        name: name.to_string(),
        args: Default::default(),
        thought_signature: None,
    };
    let mut state = crate::runfile::reader_tests::initial();
    interrupt_in_flight(&mut state);
    assert!(state.pending.is_none());

    state.children = vec![crate::spec::names::RunId::new("kid-1").unwrap()];
    state.pending = Some(PendingBatch {
        calls: vec![call("a", "shell"), call("b", "spawn_agent")],
        done: Default::default(),
    });
    interrupt_in_flight(&mut state);
    let done = &state.pending.as_ref().unwrap().done;
    assert_eq!(done["a"].text, INTERRUPTED_TOOL_RESULT);
    assert!(done["b"].text.starts_with(INTERRUPTED_TOOL_RESULT));
    assert!(done["b"].text.contains("kid-1"), "{}", done["b"].text);

    state.pending = Some(PendingBatch {
        calls: vec![call("q", "ask_user_text"), call("a", "shell")],
        done: Default::default(),
    });
    interrupt_in_flight(&mut state);
    assert!(state.pending.unwrap().done.is_empty());
}

/// A session marks the runs directory while it runs and clears the mark when
/// it ends cleanly; finding the mark when starting means the last one died.
#[test]
fn a_session_left_marked_was_one_that_died() {
    let runs = tempfile::tempdir().unwrap();
    let dir = runs.path().join("runs");
    assert!(!begin_session(&dir), "nothing ran here before");
    assert!(begin_session(&dir), "the last one never ended");
    end_session(&dir);
    assert!(!begin_session(&dir), "the last one ended cleanly");
}
