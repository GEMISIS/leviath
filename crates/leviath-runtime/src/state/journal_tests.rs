//! Tests for the journal facts a step keeps: each survives the binary codec
//! inside a step, as the run file stores it.

use super::*;
use crate::state::{RunEvent, StateDelta};

/// One event of every kind a step can carry.
pub(crate) fn every_event() -> Vec<RunEvent> {
    let model = crate::spec::names::ModelRef::parse("mock/m").expect("a model");
    let call = crate::state::context::ToolCallState {
        id: "c1".into(),
        name: "shell".into(),
        args: JsonDoc::new(serde_json::json!({ "command": "ls" })),
        thought_signature: None,
    };
    let mut events = vec![
        RunEvent::Inference {
            attempt: "a1".into(),
            model: model.clone(),
            spend: crate::state::Spend::default(),
            finish_reason: Some("stop".into()),
            kind: Default::default(),
            stage: None,
            iteration: 0,
        },
        RunEvent::Failover {
            from: model.clone(),
            to: model,
            reason: "credits".into(),
        },
        RunEvent::ToolStarted(call),
        RunEvent::ToolFinished {
            call_id: "c1".into(),
            result: crate::state::ToolResultState {
                text: "ok".into(),
                is_error: false,
            },
            millis: 4,
        },
        RunEvent::Answered {
            id: "q1".into(),
            answer: "yes".into(),
        },
        RunEvent::Message(crate::state::MessageState {
            from: "user".into(),
            text: "hi".into(),
            region: None,
        }),
        RunEvent::Log("a line".into()),
    ];
    events.extend(kept_facts());
    events
}

/// One of every fact a step keeps whole.
fn kept_facts() -> Vec<RunEvent> {
    let attempt = AttemptState {
        id: "a1".into(),
        number: 1,
        provider: "mock".into(),
        model: "m".into(),
        outcome: AttemptOutcomeState::Failed {
            kind: "timeout".into(),
            transient: true,
            capacity: false,
            next: RetryState::SameModel,
        },
        finish_reason: Some("stop".into()),
        stopped_for: None,
        duration_ms: 5,
        backoff_ms: 1,
        digest: RequestDigestState {
            system_hash: 1,
            messages: 2,
            tools: 3,
            max_tokens: 4,
            temperature: 0.25,
        },
        model_input: Some(ModelInputState {
            capture: CaptureState::Redacted,
            request: Some(JsonDoc::new(serde_json::json!({ "a": [1, 2] }))),
            bytes: 10,
            source_context_digest: "c".into(),
            parameters: [("t".to_string(), JsonDoc::new(serde_json::json!(0.5)))].into(),
            tool_catalog_version: "v".into(),
            assembly_version: "1".into(),
        }),
    };
    vec![
        RunEvent::Attempt(Box::new(attempt)),
        RunEvent::Dispatched {
            call_id: "c1".into(),
            execution_id: "x1".into(),
            requested_by: "a1".into(),
        },
        RunEvent::Completed {
            call_id: "c1".into(),
            execution_id: "x1".into(),
            outcome: Some(ToolOutcomeState::Denied),
            parts: vec!["p.png".into()],
        },
        RunEvent::Artifacts {
            execution_id: "x1".into(),
            artifacts: vec![ArtifactState {
                name: "r".into(),
                path: "r.md".into(),
                mime_type: "text/markdown".into(),
                size: 1,
                sha256: String::new(),
            }],
        },
        RunEvent::Settled(Box::new(SettledState {
            id: "q1".into(),
            kind: QuestionKind::ToolApproval,
            tool: Some("shell".into()),
            prompt: "?".into(),
            stage: "plan".into(),
            settlement: "\"timed_out\"".into(),
            asked_at: 1,
        })),
        RunEvent::ContextCommitted(Box::new(ContextCommitState {
            cause: CauseState::Hook,
            execution_id: None,
            revision_before: "a".into(),
            revision_after: "b".into(),
            regions: vec![RegionCommitState {
                region: "plan".into(),
                digest_before: "d1".into(),
                digest_after: "d2".into(),
                tokens_before: 1,
                tokens_after: 2,
                entries_before: 0,
                entries_after: 1,
                entries_added: 1,
            }],
        })),
        RunEvent::ContextNoted(ContextNoteState {
            region: "plan".into(),
            cause: CauseState::Seed,
            entries_added: 1,
            entries_removed: 0,
            token_delta: 3,
        }),
    ]
}

/// A step carrying one of every event reads back exactly.
#[test]
fn every_kept_fact_survives_the_binary_codec() {
    let delta = StateDelta {
        seq: 1,
        at: 2,
        changes: Vec::new(),
        events: every_event(),
    };
    let bytes = postcard::to_stdvec(&delta).expect("a step encodes");
    assert_eq!(
        postcard::from_bytes::<StateDelta>(&bytes).expect("a step decodes"),
        delta
    );
    let json = serde_json::to_string(&delta).expect("a step is JSON too");
    assert_eq!(
        serde_json::from_str::<StateDelta>(&json).expect("it reads back"),
        delta
    );
}
