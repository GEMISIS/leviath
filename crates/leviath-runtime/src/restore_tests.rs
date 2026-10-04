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

/// The issues a run was held for, as a held run's file records them.
fn held_issues() -> crate::spec::issues::SpawnIssues {
    crate::spec::issues::SpawnIssue::new(
        crate::spec::issues::SpecPath::root()
            .field("stages")
            .key("ask")
            .field("provider"),
        crate::spec::issues::IssueCode::Unavailable,
        "provider 'openai' is no longer configured on this machine",
    )
    .into()
}

/// One problem found at several places (a provider gone from every stage)
/// is said once, naming each place; a different problem has a line of its
/// own.
#[test]
fn a_problem_held_at_several_places_is_said_once() {
    use crate::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
    let gone = |stage: &str| {
        SpawnIssue::new(
            SpecPath::root()
                .field("stages")
                .key(stage)
                .field("provider"),
            IssueCode::Unavailable,
            "provider 'openai' is no longer configured on this machine",
        )
    };
    let mut issues = SpawnIssues::new();
    for issue in [
        gone("ask"),
        gone("red"),
        SpawnIssue::new(
            SpecPath::root().field("stages").key("ask").field("tools"),
            IssueCode::Changed,
            "MCP server 'tiny' offers a different tool list",
        ),
        gone("blue"),
    ] {
        issues.push(issue);
    }
    let leviath_core::run_meta::WaitReason::NeedsSetup { remedy, .. } = held_reason(&issues) else {
        panic!("a held run needs the machine put back");
    };
    assert_eq!(
        remedy,
        "stages.ask.provider, stages.red.provider, stages.blue.provider: unavailable: provider \
         'openai' is no longer configured on this machine; stages.ask.tools: changed: MCP \
         server 'tiny' offers a different tool list; put that back, then `lev resume` this run \
         or restart the daemon"
    );
}

/// A held run is listed paused, saying the machine changed and what to put
/// back; once it is resumed it is held no longer.
#[test]
fn a_held_run_is_listed_with_why_and_resumed_unheld() {
    let mut run = resumable(0, 1, RunStatus::Active, PipelinePhase::ReadyToInfer);
    let issues = held_issues();
    let row = held_entry(&run.spec, &run.state, &issues);
    assert_eq!(row.run_id, spec().run_id.as_str());
    assert_eq!(row.status, crate::components::AgentStatus::Paused);
    assert_eq!(row.num_stages, Some(run.spec.graph.stages.len()));
    assert!(!row.has_final_output);
    let Some(leviath_core::run_meta::WaitReason::NeedsSetup { blocker, remedy }) = row.wait_reason
    else {
        panic!("a held row says why");
    };
    assert_eq!(
        blocker,
        leviath_core::run_meta::SetupBlocker::MachineChanged
    );
    assert!(remedy.contains("stages.ask.provider"), "{remedy}");
    assert!(remedy.contains("lev resume"), "{remedy}");

    run.state.held = Some(issues);
    let mut world = World::new();
    let entity = resume(&mut world, run, crate::spec::env::Bindings::new());
    let read = crate::state::inspect::inspect(&world, entity).unwrap();
    assert_eq!(read.held, None);
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
        answer: None,
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

/// A run comes back with its answer, read from the file its run file names,
/// and a count of the questions it put to a person: the ones answered in its
/// history, the ones still open, and the calls in flight that may ask one.
/// A stored part it names whose file is gone, or an answer whose file
/// changed, is refused by name rather than read as nothing.
#[test]
fn a_run_comes_back_with_its_answer_and_the_questions_it_asked() {
    use crate::runfile::{CheckpointPolicy, RunFileErrorKind, RunFileWriter};
    use crate::state::files::FileRef;
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
        executions: Default::default(),
        held: None,
    });
    next.final_output = Some(crate::state::FinalOutputState {
        bytes: 10,
        format: None,
        stage: next.cursor.stage.clone(),
        submitted_at: 1,
        truncated: false,
        artifacts: Vec::new(),
    });
    next.files.final_output = Some(FileRef::whole("final_output", b"the answer"));
    next.blobs.push(crate::state::BlobFile {
        digest: digest.clone(),
        mime_type: "image/png".into(),
        size: 5,
        name: None,
        region: None,
        tool: None,
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
    let kind = |dir: &std::path::Path| read_for_resume(dir).unwrap_err().kind;
    assert_eq!(
        kind(dir.path()),
        RunFileErrorKind::MissingBlob(digest.clone())
    );
    let blobs = dir.path().join(leviath_core::files::BLOBS_DIR);
    std::fs::create_dir_all(&blobs).unwrap();
    std::fs::write(blobs.join(digest.as_str()), b"bytes").unwrap();
    let unread = read_for_resume(dir.path()).unwrap_err();
    assert!(matches!(unread.kind, RunFileErrorKind::Beside(_)));
    assert!(
        unread
            .to_string()
            .contains("names a file beside it that does not read"),
        "{unread}"
    );
    std::fs::write(dir.path().join("final_output"), b"the answer").unwrap();
    let run = read_for_resume(dir.path()).unwrap().expect("a run file");
    assert_eq!(run.answer.as_deref(), Some("the answer"));
    assert_eq!(run.asked, 3);
    // Placed, the run holds its answer again.
    let mut world = World::new();
    let entity = resume(&mut world, run, crate::spec::env::Bindings::new());
    let out = &world
        .get::<crate::persistence::FinalOutput>(entity)
        .unwrap()
        .0;
    assert_eq!(out.content, "the answer");
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
        bytes: 10,
        format: None,
        stage: answered.cursor.stage.clone(),
        submitted_at: 1,
        truncated: false,
        artifacts: Vec::new(),
    });
    let unread = answered.clone();
    answered.files.final_output = Some(crate::state::files::FileRef::whole(
        "final_output",
        b"the answer",
    ));
    on_disk(runs.path(), "w-answer", &answered, false);
    std::fs::write(runs.path().join("w-answer/final_output"), b"the answer").unwrap();
    // A worker whose answer's file is gone, and one whose run file names none.
    on_disk(runs.path(), "w-lost", &answered, false);
    on_disk(runs.path(), "w-unnamed", &unread, false);
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
        "w-lost",
        "w-unnamed",
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
    // The reason a file does not read is the operating system's words.
    let failed: Vec<(String, String)> = fan_out
        .failed
        .iter()
        .map(|(item, why)| {
            (
                item.clone(),
                why.split(": final_output").next().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        failed,
        [
            (
                "i-w-lost".to_string(),
                "worker's answer cannot be read".to_string()
            ),
            (
                "i-w-unnamed".to_string(),
                "worker handed back an answer its run file names no file for".to_string()
            ),
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
/// them before starting more. A run with no batch, one stopped on a
/// question, or one held on a person before it was sent, is left as it is.
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
        executions: Default::default(),
        held: None,
    });
    interrupt_in_flight(&mut state);
    let done = &state.pending.as_ref().unwrap().done;
    assert_eq!(done["a"].text, INTERRUPTED_TOOL_RESULT);
    assert!(done["b"].text.starts_with(INTERRUPTED_TOOL_RESULT));
    assert!(done["b"].text.contains("kid-1"), "{}", done["b"].text);

    state.pending = Some(PendingBatch {
        calls: vec![call("q", "ask_user_text"), call("a", "shell")],
        done: Default::default(),
        executions: Default::default(),
        held: None,
    });
    interrupt_in_flight(&mut state);
    assert!(state.pending.unwrap().done.is_empty());

    // Held on a person before it was sent: nothing in it ran.
    state.pending = Some(PendingBatch {
        calls: vec![call("a", "shell")],
        done: Default::default(),
        executions: Default::default(),
        held: Some(Default::default()),
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
