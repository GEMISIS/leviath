use super::*;
use crate::runfile::reader::RunFileReader;
use crate::runfile::reader_tests::{image, initial, scripted_run, spec};
use crate::state::journal::{
    AttemptOutcomeState, AttemptState, CauseState, QuestionKind, RetryState, ToolOutcomeState,
};
use leviath_core::region::EntryContent;
use serde_json::json;

fn record(value: serde_json::Value) -> RunRecord {
    serde_json::from_value(value).unwrap()
}

fn attempt(provider: &str, model: &str, outcome: serde_json::Value, finish: &str) -> RunRecord {
    record(json!({"InferenceAttempt": {
        "id": "a1", "stage": "plan", "attempt": 1, "provider": provider, "model": model,
        "outcome": outcome, "finish_reason": finish, "duration_ms": 5, "backoff_ms": 0,
        "digest": {"system_hash": 1, "messages": 1, "tools": 0, "max_tokens": 10, "temperature": 0.0},
        "at": 1
    }}))
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
    for r in records {
        push_events(&mut events, r);
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
    assert_eq!(events.len(), 2, "the answer, and the call kept whole");
    let RunEvent::Inference {
        attempt,
        model,
        spend,
        finish_reason,
    } = &events[0]
    else {
        unreachable!()
    };
    let RunEvent::Attempt(whole) = &events[1] else {
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
    assert_eq!(spends, vec![(0, 0, 0), (0, 0, 1), (0, 1, 0)]);
    let RunEvent::Inference { finish_reason, .. } = &events[0] else {
        unreachable!()
    };
    assert_eq!(*finish_reason, None);
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
fn failovers_tools_answers_and_messages_become_events() {
    let failover = |to: &str| {
        record(json!({"InferenceFailover": {
            "stage": "plan", "iteration": 1, "from_provider": "mock", "from_model": "m",
            "to_provider": "mock", "to_model": to, "reason": "credits", "kind": "billing", "at": 1
        }}))
    };
    let batch = record(json!({"ToolBatch": {"calls": [
        {"id": "c1", "name": "read_file", "arguments": "{\"path\":\"a\"}"},
        {"id": "c2", "name": "shell", "arguments": "not json"}
    ], "at": 1}}));
    let done = |outcome, text: &str| RunRecord::ToolCallDone {
        iteration: 1,
        call_id: "c1".into(),
        execution_id: String::new(),
        result: EntryContent::text(text),
        outcome,
        at: 1,
    };
    let answered = record(json!({"Interaction": {
        "request_id": "q1", "kind": "free_text", "prompt": "?", "stage": "plan",
        "settlement": {"answered": {"text": "yes"}}, "asked_at": 1, "at": 2
    }}));
    let message =
        record(json!({"Message": {"message": {"role": "user", "content": "hi"}, "at": 1}}));
    let status = record(json!({"StatusChanged": {"status": "complete", "at": 1}}));
    let all = events_of(&[
        failover("n"),
        failover(""),
        batch,
        done(Some(leviath_core::execution::ToolOutcome::Succeeded), "ok"),
        done(Some(leviath_core::execution::ToolOutcome::Failed), "no"),
        done(None, "[error] gone"),
        done(None, "fine"),
        answered,
        message,
        status,
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
    assert_eq!(events.len(), 9);
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
    assert_eq!(
        events[8],
        RunEvent::Message(MessageState {
            from: "user".into(),
            text: "hi".into(),
            region: None
        })
    );
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
    let log = RunRecord::StatusChanged {
        status: leviath_core::run_meta::RunStatus::Running,
        at: 1,
    };
    let message = || RunRecord::Message {
        message: leviath_core::run_archive::MessageRecord {
            role: "user".into(),
            content: "hi".into(),
        },
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
    lane.note("r1", &log);
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
        &RunRecord::Message {
            message: leviath_core::run_archive::MessageRecord {
                role: "user".into(),
                content: "too late?".into(),
            },
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
    let mut refused = leviath_core::run_archive::ToolCallRecord {
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
        regions: vec![leviath_core::run_archive::RegionCommit {
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

    let noted = journal_events(&RunRecord::ContextChange {
        region: "plan".into(),
        cause: leviath_core::ContextCause::Compaction,
        entries_added: 1,
        entries_removed: 3,
        token_delta: -40,
        at: 1,
    });
    let RunEvent::ContextNoted(note) = &noted[0] else {
        unreachable!()
    };
    assert_eq!(note.cause, CauseState::Compaction);
    assert_eq!(note.entries_removed, 3);
    assert_eq!(note.token_delta, -40);
}
