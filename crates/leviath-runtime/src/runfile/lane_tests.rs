use super::*;
use crate::runfile::reader::RunFileReader;
use crate::runfile::reader_tests::{image, initial, scripted_run, spec};
use crate::state::journal::{
    AttemptOutcomeState, AttemptState, CauseState, QuestionKind, RetryState, ToolOutcomeState,
};
use leviath_core::region::EntryContent;
use serde_json::json;

fn attempt(provider: &str, model: &str, outcome: serde_json::Value, finish: &str) -> RunRecord {
    RunRecord::InferenceAttempt(Box::new(
        serde_json::from_value(json!({
            "id": "a1", "stage": "plan", "attempt": 1, "provider": provider, "model": model,
            "outcome": outcome, "finish_reason": finish, "duration_ms": 5, "backoff_ms": 0,
            "digest": {"system_hash": 1, "messages": 1, "tools": 0, "max_tokens": 10, "temperature": 0.0},
            "at": 1
        }))
        .unwrap(),
    ))
}

fn call(id: &str, name: &str, arguments: &str) -> crate::runfile::record::ToolCallRecord {
    crate::runfile::record::ToolCallRecord {
        id: id.into(),
        execution_id: String::new(),
        name: name.into(),
        arguments: arguments.into(),
        result: None,
        thought_signature: None,
    }
}

fn usage(model: &str, cost: Option<f64>, reported: Option<bool>) -> RunRecord {
    RunRecord::InferenceUsage {
        kind: Default::default(),
        stage: "plan".into(),
        iteration: 1,
        provider: "mock".into(),
        model: model.into(),
        prompt_tokens: 10,
        completion_tokens: 2,
        cached_tokens: 1,
        cache_write_tokens: 0,
        cost_usd: cost,
        cost_reported_by_provider: reported,
        at: 1,
    }
}

fn events_of(records: &[RunRecord]) -> Vec<RunEvent> {
    let mut events = Vec::new();
    let mut answered = Answered::default();
    for r in records {
        push_events(&mut events, &mut answered, r);
    }
    events
}

fn mock(model: &str) -> ModelRef {
    ModelRef::parse(&format!("mock/{model}")).unwrap()
}

#[test]
fn a_model_call_and_its_usage_become_one_inference_event() {
    let events = events_of(&[
        attempt("mock", "m", json!("succeeded"), "stop"),
        usage("m", Some(0.5), Some(true)),
    ]);
    assert_eq!(events.len(), 2, "the call kept whole, then its bill");
    let RunEvent::Inference {
        attempt,
        model,
        spend,
        finish_reason,
        kind,
        stage,
        iteration,
    } = &events[1]
    else {
        unreachable!()
    };
    assert_eq!(*kind, crate::state::journal::CallKind::Stage);
    assert_eq!(stage.as_ref().map(|s| s.as_str()), Some("plan"));
    assert_eq!(*iteration, 1);
    let RunEvent::Attempt(whole) = &events[0] else {
        unreachable!()
    };
    assert_eq!(whole.id, "a1");
    assert_eq!(whole.duration_ms, 5);
    assert_eq!(whole.finish_reason.as_deref(), Some("stop"));
    assert_eq!(attempt, "a1");
    assert_eq!(*model, mock("m"));
    assert_eq!(spend.prompt_tokens, 10);
    assert_eq!(spend.reported_calls, 1);
    assert_eq!(finish_reason.as_deref(), Some("stop"));
}

#[test]
fn usage_with_no_call_to_attach_to_stands_on_its_own() {
    let events = events_of(&[
        attempt("mock", "m", json!("succeeded"), ""),
        usage("other", None, None),
        usage("other", Some(0.1), Some(false)),
        usage("", None, None),
    ]);
    let spends: Vec<(u32, u32, u32)> = events
        .iter()
        .filter_map(|e| match e {
            RunEvent::Inference { spend, .. } => Some((
                spend.reported_calls,
                spend.computed_calls,
                spend.unpriced_calls,
            )),
            _ => None,
        })
        .collect();
    assert_eq!(spends, vec![(0, 0, 1), (0, 1, 0)]);
    let RunEvent::Inference {
        attempt,
        finish_reason,
        ..
    } = &events[1]
    else {
        unreachable!()
    };
    assert_eq!(attempt, "", "billed to no attempt");
    assert_eq!(*finish_reason, None);
}

/// The worker that made a call records its attempt, and the system that read
/// the answer records its bill, so the two can land in different steps. Each
/// call is still one `Inference`, never a second one with nothing spent, and
/// a title call is a title call with no stage.
#[test]
fn an_attempt_and_its_bill_in_different_steps_are_one_call() {
    let mut answered = Answered::default();
    let first = journal_events_with(
        &attempt("mock", "m", json!("succeeded"), "stop"),
        &mut answered,
    );
    assert!(
        first
            .iter()
            .all(|e| !matches!(e, RunEvent::Inference { .. })),
        "{first:?}"
    );
    let mut title = usage("m", None, None);
    if let RunRecord::InferenceUsage {
        kind,
        stage,
        iteration,
        ..
    } = &mut title
    {
        *kind = crate::runfile::record::InferenceKind::Title;
        stage.clear();
        *iteration = 0;
    }
    let second = journal_events_with(&title, &mut answered);
    let [
        RunEvent::Inference {
            attempt,
            kind,
            stage,
            finish_reason,
            ..
        },
    ] = second.as_slice()
    else {
        panic!("one call: {second:?}")
    };
    assert_eq!(attempt, "a1");
    assert_eq!(finish_reason.as_deref(), Some("stop"));
    assert_eq!(*kind, crate::state::journal::CallKind::Title);
    assert_eq!(*stage, None);
    // The attempt is billed once.
    let third = journal_events_with(&usage("m", None, None), &mut answered);
    let RunEvent::Inference { attempt, .. } = &third[0] else {
        unreachable!()
    };
    assert_eq!(attempt, "");
}

/// Each edge a run takes is its own event, whatever else the step holds.
#[test]
fn a_transition_is_an_event_of_its_own() {
    let taken = crate::state::TransitionRecord {
        from: crate::spec::names::StageName::new("a").unwrap(),
        to: crate::spec::names::StageName::new("b").unwrap(),
        edge: None,
        reason: crate::state::TransitionReason::Forced,
        visit: "v1".into(),
    };
    assert_eq!(
        journal_events(&RunRecord::Transition(taken.clone())),
        vec![RunEvent::Transition(taken)]
    );
}

#[test]
fn a_failed_call_is_a_log_line_and_a_nameless_model_is_nothing() {
    let failed = json!({"failed": {"kind": "rate_limit", "transient": true, "capacity": false, "next": "same_model"}});
    let events = events_of(&[
        attempt("mock", "m", failed, ""),
        attempt("mock", "", json!("succeeded"), ""),
    ]);
    assert_eq!(
        events[0],
        RunEvent::Log("model call a1 on mock/m failed: rate_limit".into())
    );
    let kept: Vec<&AttemptState> = events
        .iter()
        .filter_map(|e| match e {
            RunEvent::Attempt(a) => Some(&**a),
            _ => None,
        })
        .collect();
    assert_eq!(kept.len(), 2, "both calls are kept whole, named or not");
    assert_eq!(
        kept[0].outcome,
        AttemptOutcomeState::Failed {
            kind: "rate_limit".into(),
            transient: true,
            capacity: false,
            next: RetryState::SameModel,
        }
    );
    assert_eq!(kept[1].model, "");
    assert_eq!(events.len(), 3);
}

#[test]
fn failovers_tools_and_answers_become_events() {
    let failover = |to: &str| {
        RunRecord::InferenceFailover(crate::runfile::record::FailoverRecord {
            stage: "plan".into(),
            iteration: 1,
            from_provider: "mock".into(),
            from_model: "m".into(),
            to_provider: "mock".into(),
            to_model: to.into(),
            reason: "credits".into(),
            kind: "billing".into(),
            at: 1,
        })
    };
    let batch = RunRecord::ToolBatch {
        calls: vec![
            call("c1", "read_file", "{\"path\":\"a\"}"),
            call("c2", "shell", "not json"),
        ],
        at: 1,
        stage_index: 0,
        iteration: 0,
        visit_id: String::new(),
        requested_by: String::new(),
        response: String::new(),
    };
    let done = |outcome, text: &str| RunRecord::ToolCallDone {
        iteration: 1,
        call_id: "c1".into(),
        execution_id: String::new(),
        result: EntryContent::text(text),
        outcome,
        at: 1,
    };
    let answered = RunRecord::Interaction {
        request_id: "q1".into(),
        kind: serde_json::from_value(json!("free_text")).unwrap(),
        tool: None,
        prompt: "?".into(),
        stage: "plan".into(),
        settlement: serde_json::from_value(json!({"answered": {"text": "yes"}})).unwrap(),
        asked_at: 1,
        at: 2,
    };
    let all = events_of(&[
        failover("n"),
        failover(""),
        batch,
        done(Some(leviath_core::execution::ToolOutcome::Succeeded), "ok"),
        done(Some(leviath_core::execution::ToolOutcome::Failed), "no"),
        done(None, "[error] gone"),
        done(None, "fine"),
        answered,
    ]);
    // The events that keep a record whole sit beside the ones it always
    // became: one dispatch per call, one ending per result, one settlement.
    let (whole, events): (Vec<RunEvent>, Vec<RunEvent>) = all.into_iter().partition(|e| {
        matches!(
            e,
            RunEvent::Dispatched { .. } | RunEvent::Completed { .. } | RunEvent::Settled(_)
        )
    });
    assert_eq!(whole.len(), 2 + 4 + 1);
    let RunEvent::Settled(settled) = &whole[6] else {
        unreachable!()
    };
    assert_eq!(settled.kind, QuestionKind::FreeText);
    assert_eq!(settled.stage, "plan");
    let outcomes: Vec<Option<ToolOutcomeState>> = whole
        .iter()
        .filter_map(|e| match e {
            RunEvent::Completed { outcome, .. } => Some(*outcome),
            _ => None,
        })
        .collect();
    assert_eq!(
        outcomes,
        vec![
            Some(ToolOutcomeState::Succeeded),
            Some(ToolOutcomeState::Failed),
            None,
            None
        ]
    );
    assert_eq!(events.len(), 8);
    assert_eq!(
        events[0],
        RunEvent::Failover {
            from: mock("m"),
            to: mock("n"),
            reason: "credits".into()
        }
    );
    let RunEvent::ToolStarted(first) = &events[1] else {
        unreachable!()
    };
    assert_eq!(first.args.value(), &json!({"path": "a"}));
    let RunEvent::ToolStarted(second) = &events[2] else {
        unreachable!()
    };
    assert_eq!(second.args.value(), &json!("not json"));
    let errors: Vec<bool> = events[3..7]
        .iter()
        .filter_map(|e| match e {
            RunEvent::ToolFinished { result, .. } => Some(result.is_error),
            _ => None,
        })
        .collect();
    assert_eq!(errors, vec![false, true, true, false]);
    let RunEvent::Answered { id, answer } = &events[7] else {
        unreachable!()
    };
    assert_eq!(id, "q1");
    assert!(answer.contains("yes"));
}

fn step(run_id: &str, state: RunState) -> RunFileStep {
    RunFileStep {
        run_id: run_id.into(),
        spec: Arc::new(spec()),
        state,
        at: 7,
    }
}

fn run_dir(runs: &Path, run_id: &str) -> PathBuf {
    let dir = runs.join(run_id);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn file(runs: &Path, run_id: &str) -> RunFileReader {
    RunFileReader::open(&runs.join(run_id).join(leviath_core::files::RUN_FILE)).unwrap()
}

#[tokio::test]
async fn a_run_gets_a_file_on_its_first_step_and_events_on_its_next() {
    let runs = tempfile::tempdir().unwrap();
    run_dir(runs.path(), "r1");
    let mut lane = RunFileLane::new("machine", "world");
    let states = scripted_run(3);
    // A usage record on a model with no valid name becomes no event.
    let silent = usage("", None, None);
    let message = || RunRecord::ArtifactsProduced {
        execution_id: "x1".into(),
        artifacts: Vec::new(),
        at: 1,
    };
    // What is noted for a run with no file is dropped when it is flushed.
    lane.note("r1", &message());
    assert_eq!(lane.noted(), ["r1"]);
    assert_eq!(lane.flush(runs.path(), "r1").await.unwrap(), None);
    assert!(lane.events.is_empty());
    assert_eq!(
        lane.record(runs.path(), step("r1", states[0].clone()))
            .await
            .unwrap(),
        None
    );
    assert_eq!(lane.writers.len(), 1);
    lane.note("r1", &silent);
    lane.note("r1", &message());
    assert_eq!(
        lane.record(runs.path(), step("r1", states[1].clone()))
            .await
            .unwrap(),
        Some(1)
    );
    let r = file(runs.path(), "r1");
    let delta = &r.deltas(1, 1).unwrap()[0];
    assert_eq!(delta.events.len(), 1);
    assert_eq!(r.owners().unwrap()[0].machine_id, "machine");
    assert_eq!(r.latest_state().unwrap().seq, 1);
    // A deleted run is forgotten.
    lane.note("r1", &message());
    lane.forget("r1");
    assert!(lane.writers.is_empty() && lane.events.is_empty());
}

#[tokio::test]
async fn a_finished_run_closes_its_file_and_a_new_lane_carries_one_on() {
    let runs = tempfile::tempdir().unwrap();
    run_dir(runs.path(), "r1");
    let states = scripted_run(2);
    let mut lane = RunFileLane::new("m", "first");
    lane.record(runs.path(), step("r1", states[0].clone()))
        .await
        .unwrap();
    lane.record(runs.path(), step("r1", states[1].clone()))
        .await
        .unwrap();
    // A restarted daemon opens the file where it was left.
    let mut lane = RunFileLane::new("m", "second");
    let last = states.last().unwrap().clone();
    assert_eq!(
        lane.record(runs.path(), step("r1", last)).await.unwrap(),
        Some(2)
    );
    assert!(lane.writers.is_empty());
    let r = file(runs.path(), "r1");
    let owners: Vec<String> = r
        .owners()
        .unwrap()
        .into_iter()
        .map(|o| o.world_id)
        .collect();
    assert_eq!(owners, vec!["first", "second"]);
    assert_eq!(r.last_checkpoint().0, 2);
    // What happens after the run's last change of state still reaches its
    // file, which is opened again for it and closed again after.
    lane.note(
        "r1",
        &RunRecord::ArtifactsProduced {
            execution_id: "too late?".into(),
            artifacts: Vec::new(),
            at: 9,
        },
    );
    assert_eq!(lane.flush(runs.path(), "r1").await.unwrap(), Some(3));
    assert!(lane.writers.is_empty() && lane.noted().is_empty());
    let r = file(runs.path(), "r1");
    assert_eq!(r.deltas(3, 3).unwrap()[0].events.len(), 1);
}

#[tokio::test]
async fn stored_parts_are_copied_in_from_the_run_s_blobs_when_they_are_there() {
    let runs = tempfile::tempdir().unwrap();
    let dir = run_dir(runs.path(), "r1");
    let blobs = dir.join(leviath_core::files::BLOBS_DIR);
    std::fs::create_dir_all(&blobs).unwrap();
    std::fs::write(blobs.join(Digest::of(b"here").as_str()), b"here").unwrap();
    let mut s = initial();
    let mut shown =
        crate::runfile::reader_tests::entry("look", crate::state::context::EntryKind::Text);
    shown.parts = vec![
        image(b"here"),
        image(b"here"),
        image(b"gone"),
        inline_part(),
    ];
    s.context.regions[1].entries.push(shown);
    let mut lane = RunFileLane::new("m", "w");
    lane.record(runs.path(), step("r1", s)).await.unwrap();
    let r = file(runs.path(), "r1");
    assert_eq!(
        r.blob(&Digest::of(b"here")).unwrap(),
        Some(b"here".to_vec())
    );
    assert_eq!(r.blob(&Digest::of(b"gone")).unwrap(), None);
}

fn inline_part() -> crate::state::context::PartState {
    crate::state::context::PartState {
        mime_type: "text/plain".into(),
        body: PartBody::Inline("hi".into()),
        name: None,
        deliver: None,
    }
}

#[tokio::test]
async fn a_step_that_cannot_be_written_is_an_error_and_closes_the_file() {
    let runs = tempfile::tempdir().unwrap();
    let mut lane = RunFileLane::new("m", "w");
    // No run directory to make the file in.
    assert!(
        lane.record(runs.path(), step("nowhere", initial()))
            .await
            .is_err()
    );
    assert!(lane.writers.is_empty());
    // A file that is not a run file.
    let dir = run_dir(runs.path(), "junk");
    std::fs::write(dir.join(leviath_core::files::RUN_FILE), b"junk").unwrap();
    assert!(
        lane.record(runs.path(), step("junk", initial()))
            .await
            .is_err()
    );
    // A file whose writes fail, with and without a blob to store.
    let dir = run_dir(runs.path(), "r1");
    lane.record(runs.path(), step("r1", initial()))
        .await
        .unwrap();
    crate::runfile::writer::break_writes(lane.writers.get_mut("r1").unwrap());
    let changed = RunState {
        title: Some("t".into()),
        ..initial()
    };
    assert!(lane.record(runs.path(), step("r1", changed)).await.is_err());
    assert!(lane.writers.is_empty());
    let blobs = dir.join(leviath_core::files::BLOBS_DIR);
    std::fs::create_dir_all(&blobs).unwrap();
    std::fs::write(blobs.join(Digest::of(b"img").as_str()), b"img").unwrap();
    lane.record(runs.path(), step("r1", initial()))
        .await
        .unwrap();
    crate::runfile::writer::break_writes(lane.writers.get_mut("r1").unwrap());
    let mut with_image = initial();
    let mut shown =
        crate::runfile::reader_tests::entry("look", crate::state::context::EntryKind::Text);
    shown.parts = vec![image(b"img")];
    with_image.context.regions[1].entries.push(shown);
    assert!(
        lane.record(runs.path(), step("r1", with_image))
            .await
            .is_err()
    );
}

/// A call that came back in its batch record ends there, with the parts its
/// result carried named, and the dispatch names the execution and the call
/// that asked for it.
#[test]
fn a_batch_names_its_executions_and_ends_the_calls_it_carries() {
    let stored = leviath_core::mime::Part::stored(leviath_core::mime::BlobRef {
        sha256: "abc".into(),
        mime_type: leviath_core::mime::MimeType::parse("image/png").unwrap(),
        size: 3,
        width: None,
        height: None,
        duration_ms: None,
        tokens: 1,
        stand_in: "[image]".into(),
    })
    .named("chart.png");
    let mut refused = crate::runfile::record::ToolCallRecord {
        id: "c1".into(),
        execution_id: "x1".into(),
        name: "shell".into(),
        arguments: "{}".into(),
        result: Some(EntryContent::from_parts(vec![
            leviath_core::mime::Part::text("[blocked] no"),
            stored,
        ])),
        thought_signature: None,
    };
    let batch = RunRecord::ToolBatch {
        calls: vec![refused.clone()],
        at: 1,
        stage_index: 0,
        iteration: 1,
        visit_id: String::new(),
        requested_by: "a1".into(),
        response: String::new(),
    };
    let events = journal_events(&batch);
    assert!(events.contains(&RunEvent::Dispatched {
        call_id: "c1".into(),
        execution_id: "x1".into(),
        requested_by: "a1".into(),
    }));
    assert!(events.contains(&RunEvent::Completed {
        call_id: "c1".into(),
        execution_id: "x1".into(),
        outcome: None,
        parts: vec!["chart.png".into()],
    }));
    refused.result = None;
    let quiet = journal_events(&RunRecord::ToolBatch {
        calls: vec![refused],
        at: 1,
        stage_index: 0,
        iteration: 1,
        visit_id: String::new(),
        requested_by: String::new(),
        response: String::new(),
    });
    assert_eq!(quiet.len(), 2, "a start and a dispatch, and no ending");
}

/// What an execution produced, and a context change with its cause, are kept
/// as they were recorded.
#[test]
fn artifacts_and_context_changes_are_kept_whole() {
    let artifacts = journal_events(&RunRecord::ArtifactsProduced {
        execution_id: "x1".into(),
        artifacts: vec![leviath_core::output::Artifact {
            name: "report".into(),
            path: "out/r.md".into(),
            mime_type: leviath_core::mime::MimeType::parse("text/markdown").unwrap(),
            size: 9,
            sha256: "beef".into(),
        }],
        at: 1,
    });
    let RunEvent::Artifacts {
        execution_id,
        artifacts,
    } = &artifacts[0]
    else {
        unreachable!()
    };
    assert_eq!(execution_id, "x1");
    assert_eq!(artifacts[0].mime_type, "text/markdown");

    let commit = |execution: &str| RunRecord::ContextTransaction {
        revision_before: "cw1-a".into(),
        revision_after: "cw1-b".into(),
        cause: leviath_core::ContextCause::ToolResult,
        regions: vec![crate::runfile::record::RegionCommit {
            region: "plan".into(),
            digest_before: "d1".into(),
            digest_after: "d2".into(),
            tokens_before: 1,
            tokens_after: 5,
            entries_before: 0,
            entries_after: 1,
            entries_added: 1,
        }],
        execution_id: execution.into(),
        at: 1,
    };
    let RunEvent::ContextCommitted(kept) = &journal_events(&commit("x1"))[0] else {
        unreachable!()
    };
    assert_eq!(kept.execution_id.as_deref(), Some("x1"));
    assert_eq!(kept.cause, CauseState::ToolResult);
    assert_eq!(kept.regions[0].entries_added, 1);
    let RunEvent::ContextCommitted(anonymous) = &journal_events(&commit(""))[0] else {
        unreachable!()
    };
    assert_eq!(anonymous.execution_id, None);
}

/// A batch with calls `ids` and nothing back yet.
fn batch(ids: &[&str]) -> crate::state::PendingBatch {
    crate::state::PendingBatch {
        calls: ids
            .iter()
            .map(|id| ToolCallState {
                id: id.to_string(),
                name: "shell".into(),
                args: JsonDoc::default(),
                thought_signature: None,
            })
            .collect(),
        done: Default::default(),
    }
}

fn started(id: &str) -> RunEvent {
    RunEvent::ToolStarted(ToolCallState {
        id: id.to_string(),
        name: "shell".into(),
        args: JsonDoc::default(),
        thought_signature: None,
    })
}

fn finished(id: &str, text: &str) -> RunEvent {
    RunEvent::ToolFinished {
        call_id: id.to_string(),
        result: ToolResultState {
            text: text.to_string(),
            is_error: false,
        },
        millis: 0,
    }
}

/// A call that comes back mid-batch is held as done by the batch the state
/// holds: only a call of that batch, only a result after the last batch the
/// events start, and never over a result already there. A state written
/// before the events, while they start a batch, holds some other batch.
#[test]
fn a_result_that_lands_mid_batch_is_held_as_done() {
    let mut state = initial();
    fold_finished(&mut state, &[finished("c1", "x")], Started::InThisState);
    assert!(state.pending.is_none(), "no batch, nothing to hold");

    state.pending = Some(batch(&["c1", "c2"]));
    let events = [
        finished("c2", "the old batch's c2"),
        started("c1"),
        started("c2"),
        finished("c1", "ran c1"),
        finished("c9", "not this batch"),
    ];
    let mut elsewhere = state.clone();
    fold_finished(&mut elsewhere, &events, Started::Elsewhere);
    assert!(elsewhere.pending.unwrap().done.is_empty());

    fold_finished(&mut state, &events, Started::InThisState);
    let done = &state.pending.as_ref().unwrap().done;
    assert_eq!(done.keys().collect::<Vec<_>>(), ["c1"]);
    assert_eq!(done["c1"].text, "ran c1");

    // With no batch started, every result counts; one already held stays.
    fold_finished(
        &mut state,
        &[finished("c1", "again"), finished("c2", "ran c2")],
        Started::Elsewhere,
    );
    let done = &state.pending.as_ref().unwrap().done;
    assert_eq!(done["c1"].text, "ran c1");
    assert_eq!(done["c2"].text, "ran c2");
}

/// A call's completion noted on its own is written as a step that holds the
/// call as done, so a file read after a crash mid-batch knows it finished.
#[tokio::test]
async fn a_completion_noted_mid_batch_is_a_step_with_the_call_done() {
    let runs = tempfile::tempdir().unwrap();
    let mut lane = RunFileLane::new("m", "w");
    let mut busy = initial();
    busy.pending = Some(batch(&["c1", "c2"]));
    run_dir(runs.path(), "r1");
    lane.record(runs.path(), step("r1", busy)).await.unwrap();
    lane.note(
        "r1",
        &RunRecord::ToolCallDone {
            iteration: 1,
            call_id: "c1".into(),
            execution_id: "e1".into(),
            result: EntryContent::text("ran c1"),
            outcome: None,
            at: 1,
        },
    );
    lane.flush(runs.path(), "r1").await.unwrap();
    let state = file(runs.path(), "r1").latest_state().unwrap();
    let done = state.pending.unwrap().done;
    assert_eq!(done.keys().collect::<Vec<_>>(), ["c1"]);
}

/// A call a restart found still running ended with nobody watching: the
/// stand-in result it is carried back with is a failure whose outcome is not
/// known, not a success.
#[test]
fn a_call_carried_back_interrupted_ends_unobserved_and_failed() {
    let interrupted = crate::runfile::record::ToolCallRecord {
        id: "c1".into(),
        execution_id: "x1".into(),
        name: "shell".into(),
        arguments: "{}".into(),
        result: Some(EntryContent::from(
            crate::restore::INTERRUPTED_TOOL_RESULT.to_string(),
        )),
        thought_signature: None,
    };
    let events = journal_events(&RunRecord::ToolBatch {
        calls: vec![interrupted],
        at: 1,
        stage_index: 0,
        iteration: 1,
        visit_id: String::new(),
        requested_by: "a1".into(),
        response: String::new(),
    });
    let finished = events
        .iter()
        .find_map(|e| match e {
            RunEvent::ToolFinished { result, .. } => Some(result.is_error),
            _ => None,
        })
        .expect("the call ends");
    assert!(finished, "an interrupted call is an error");
    assert!(events.contains(&RunEvent::Completed {
        call_id: "c1".into(),
        execution_id: "x1".into(),
        outcome: Some(crate::state::journal::ToolOutcomeState::Indeterminate),
        parts: Vec::new(),
    }));
}
