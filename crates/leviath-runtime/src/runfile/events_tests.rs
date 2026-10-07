use super::*;
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
        done(None, "[denied] Tool 'shell' is not permitted."),
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
    assert_eq!(whole.len(), 2 + 5 + 1);
    let RunEvent::Settled(settled) = &whole[7] else {
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
            Some(ToolOutcomeState::Failed),
            Some(ToolOutcomeState::Succeeded),
            Some(ToolOutcomeState::Blocked),
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
    let errors: Vec<bool> = events[3..8]
        .iter()
        .filter_map(|e| match e {
            RunEvent::ToolFinished { result, .. } => Some(result.is_error),
            _ => None,
        })
        .collect();
    // A refusal is not a failure of the tool: only a call that ran and
    // failed, or one whose ending nobody saw, reads as an error.
    assert_eq!(errors, vec![false, true, true, false, false]);
    let RunEvent::Answered { id, answer } = &events[8] else {
        unreachable!()
    };
    assert_eq!(id, "q1");
    assert!(answer.contains("yes"));
}

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
        outcome: Some(ToolOutcomeState::Blocked),
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

/// Calls sent to the lane again are dispatched again as the executions they
/// already were, and nothing about them starts anew.
#[test]
fn calls_sent_again_are_the_executions_they_were() {
    let events = journal_events(&RunRecord::ToolCallsResent {
        calls: vec![("q".into(), "x1".into())],
        requested_by: "a1".into(),
        at: 1,
    });
    assert_eq!(
        events,
        [RunEvent::Dispatched {
            call_id: "q".into(),
            execution_id: "x1".into(),
            requested_by: "a1".into(),
        }]
    );
}

/// How a call ended, read off its result: the words every refusal and
/// failure starts with say which it was, and anything else ran and answered.
#[test]
fn a_result_says_how_its_call_ended() {
    use leviath_core::execution::ToolOutcome;
    let cases = [
        ("fn a() {}", ToolOutcome::Succeeded),
        ("", ToolOutcome::Succeeded),
        ("[error] no such file", ToolOutcome::Failed),
        (
            crate::restore::INTERRUPTED_TOOL_RESULT,
            ToolOutcome::Indeterminate,
        ),
        (
            "[blocked] Tool 'shell' would send data",
            ToolOutcome::Blocked,
        ),
        (
            "[unavailable] 'x' is not available in this stage.",
            ToolOutcome::Blocked,
        ),
        (
            "[denied] Tool 'shell' is not permitted.",
            ToolOutcome::Blocked,
        ),
        (
            &crate::approval_prompt::declined_result("shell", Some("no")),
            ToolOutcome::Denied,
        ),
        (
            &crate::approval_prompt::unanswered_approval_result("shell", Some(5)),
            ToolOutcome::Denied,
        ),
        (
            &crate::approval_prompt::unanswered_approval_result("shell", None),
            ToolOutcome::Denied,
        ),
        (
            &crate::approval_prompt::lost_approval_result("shell", "gone"),
            ToolOutcome::Denied,
        ),
    ];
    for (text, want) in cases {
        assert_eq!(outcome_of(text), want, "{text}");
    }
}
