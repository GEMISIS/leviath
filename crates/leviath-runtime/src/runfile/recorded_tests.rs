//! Tests for keeping a journal record whole: every word each record can hold
//! crosses over to its run-file counterpart.

use super::*;

fn record(model_input: Option<ModelInput>, outcome: AttemptOutcome) -> AttemptRecord {
    AttemptRecord {
        id: "a1".into(),
        stage: "plan".into(),
        attempt: 2,
        provider: "mock".into(),
        model: "m".into(),
        outcome,
        finish_reason: String::new(),
        stopped_for: Some("content_filter".into()),
        duration_ms: 9,
        backoff_ms: 3,
        digest: RequestDigest {
            system_hash: 7,
            messages: 4,
            tools: 2,
            max_tokens: 100,
            temperature: 0.5,
        },
        model_input,
        at: 1,
    }
}

/// A call is kept with its timing, its digest, its captured request and how
/// it failed, and an empty finish reason as no reason.
#[test]
fn a_call_is_kept_whole() {
    let input = ModelInput {
        capture_status: CaptureStatus::Retained,
        request: Some(serde_json::json!({ "model": "m" })),
        bytes: 15,
        source_context_digest: "ctx".into(),
        parameters: [("temperature".to_string(), serde_json::json!(0.5))].into(),
        tool_catalog_version: "t1".into(),
        assembly_version: "1".into(),
    };
    let kept = attempt(&record(Some(input), AttemptOutcome::Succeeded));
    assert_eq!(kept.number, 2);
    assert_eq!(kept.finish_reason, None);
    assert_eq!(kept.stopped_for.as_deref(), Some("content_filter"));
    assert_eq!(kept.digest.messages, 4);
    let captured = kept.model_input.expect("the request");
    assert_eq!(captured.capture, CaptureState::Retained);
    assert_eq!(
        captured.request.map(|doc| doc.value().clone()),
        Some(serde_json::json!({ "model": "m" }))
    );
    assert_eq!(
        captured.parameters["temperature"].value(),
        &serde_json::json!(0.5)
    );
    let failed = attempt(&record(
        None,
        AttemptOutcome::Failed {
            kind: "timeout".into(),
            transient: true,
            capacity: true,
            next: Retry::RenewedFiles,
        },
    ));
    assert_eq!(
        failed.outcome,
        AttemptOutcomeState::Failed {
            kind: "timeout".into(),
            transient: true,
            capacity: true,
            next: RetryState::RenewedFiles,
        }
    );
    assert_eq!(count(usize::MAX), u32::MAX);
}

/// Every word in each closed vocabulary has its counterpart.
#[test]
fn every_word_crosses_over() {
    for (status, state) in [
        (CaptureStatus::Retained, CaptureState::Retained),
        (CaptureStatus::NotCaptured, CaptureState::NotCaptured),
        (CaptureStatus::Redacted, CaptureState::Redacted),
        (CaptureStatus::Expired, CaptureState::Expired),
    ] {
        assert_eq!(capture(status), state);
    }
    for (next, state) in [
        (Retry::Reported, RetryState::Reported),
        (Retry::SameModel, RetryState::SameModel),
        (Retry::RenewedFiles, RetryState::RenewedFiles),
    ] {
        assert_eq!(retry(next), state);
    }
    for (o, state) in [
        (ToolOutcome::Succeeded, ToolOutcomeState::Succeeded),
        (ToolOutcome::Failed, ToolOutcomeState::Failed),
        (ToolOutcome::Blocked, ToolOutcomeState::Blocked),
        (ToolOutcome::Denied, ToolOutcomeState::Denied),
        (ToolOutcome::Indeterminate, ToolOutcomeState::Indeterminate),
    ] {
        assert_eq!(outcome(o), state);
    }
    for (kind, state) in [
        (InteractionKind::FreeText, QuestionKind::FreeText),
        (
            InteractionKind::MultipleChoice,
            QuestionKind::MultipleChoice,
        ),
        (InteractionKind::Confirm, QuestionKind::Confirm),
        (InteractionKind::ToolApproval, QuestionKind::ToolApproval),
        (InteractionKind::EditText, QuestionKind::EditText),
    ] {
        assert_eq!(question_kind(&kind), state);
    }
    for (c, state) in [
        (ContextCause::Seed, CauseState::Seed),
        (ContextCause::Message, CauseState::Message),
        (ContextCause::ModelReply, CauseState::ModelReply),
        (ContextCause::ToolResult, CauseState::ToolResult),
        (ContextCause::ProducedPart, CauseState::ProducedPart),
        (ContextCause::Compaction, CauseState::Compaction),
        (ContextCause::Transform, CauseState::Transform),
        (ContextCause::ContextTool, CauseState::ContextTool),
        (ContextCause::Hook, CauseState::Hook),
        (ContextCause::FanOut, CauseState::FanOut),
        (ContextCause::Interaction, CauseState::Interaction),
        (ContextCause::Resume, CauseState::Resume),
        (ContextCause::Framework, CauseState::Framework),
    ] {
        assert_eq!(CauseState::from(c), state);
    }
}

/// Each kind of billed call keeps its kind in the run file.
#[test]
fn every_kind_of_call_is_kept_as_itself() {
    use crate::runfile::record::InferenceKind;
    use crate::state::journal::CallKind;
    for (kind, kept) in [
        (InferenceKind::Stage, CallKind::Stage),
        (InferenceKind::Compaction, CallKind::Compaction),
        (InferenceKind::Title, CallKind::Title),
        (InferenceKind::Routing, CallKind::Routing),
    ] {
        assert_eq!(CallKind::from(kind), kept);
        assert_eq!(kept.label(), kind.label());
    }
}
