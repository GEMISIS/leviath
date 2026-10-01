//! A journal record's facts, as the run file's events keep them.
//!
//! The world hands the persistence lane one journal record per thing that
//! happened. [`push_events`](super::lane::push_events) turns each into the
//! events a step carries; the conversions here are the parts of that which
//! keep a record whole: a model call with its timing and request, an
//! execution's identity and ending, a settled question, a context change with
//! its cause.

use leviath_core::JsonDoc;
use leviath_core::context_cause::ContextCause;
use leviath_core::execution::ToolOutcome;
use leviath_core::interaction::InteractionKind;
use leviath_core::run_archive::{
    AttemptOutcome, AttemptRecord, CaptureStatus, ModelInput, RegionCommit, RequestDigest, Retry,
};

use crate::state::journal::{
    ArtifactState, AttemptOutcomeState, AttemptState, CaptureState, CauseState, ModelInputState,
    QuestionKind, RegionCommitState, RequestDigestState, RetryState, ToolOutcomeState,
};

/// A count the journal keeps as `usize`, as the run file keeps it.
fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Text that is empty as absent.
fn some(text: &str) -> Option<String> {
    (!text.is_empty()).then(|| text.to_string())
}

fn digest(d: &RequestDigest) -> RequestDigestState {
    RequestDigestState {
        system_hash: d.system_hash,
        messages: count(d.messages),
        tools: count(d.tools),
        max_tokens: count(d.max_tokens),
        temperature: d.temperature,
    }
}

fn capture(status: CaptureStatus) -> CaptureState {
    match status {
        CaptureStatus::Retained => CaptureState::Retained,
        CaptureStatus::NotCaptured => CaptureState::NotCaptured,
        CaptureStatus::Redacted => CaptureState::Redacted,
        CaptureStatus::Expired => CaptureState::Expired,
    }
}

fn model_input(input: &ModelInput) -> ModelInputState {
    ModelInputState {
        capture: capture(input.capture_status),
        request: input.request.clone().map(JsonDoc::new),
        bytes: input.bytes,
        source_context_digest: input.source_context_digest.clone(),
        parameters: input
            .parameters
            .iter()
            .map(|(k, v)| (k.clone(), JsonDoc::new(v.clone())))
            .collect(),
        tool_catalog_version: input.tool_catalog_version.clone(),
        assembly_version: input.assembly_version.clone(),
    }
}

fn retry(next: Retry) -> RetryState {
    match next {
        Retry::Reported => RetryState::Reported,
        Retry::SameModel => RetryState::SameModel,
        Retry::RenewedFiles => RetryState::RenewedFiles,
    }
}

/// A journal's attempt record, whole.
pub(crate) fn attempt(a: &AttemptRecord) -> AttemptState {
    AttemptState {
        id: a.id.clone(),
        number: a.attempt,
        provider: a.provider.clone(),
        model: a.model.clone(),
        outcome: match &a.outcome {
            AttemptOutcome::Succeeded => AttemptOutcomeState::Succeeded,
            AttemptOutcome::Failed {
                kind,
                transient,
                capacity,
                next,
            } => AttemptOutcomeState::Failed {
                kind: kind.clone(),
                transient: *transient,
                capacity: *capacity,
                next: retry(*next),
            },
        },
        finish_reason: some(&a.finish_reason),
        stopped_for: a.stopped_for.clone(),
        duration_ms: a.duration_ms,
        backoff_ms: a.backoff_ms,
        digest: digest(&a.digest),
        model_input: a.model_input.as_ref().map(model_input),
    }
}

/// How an execution ended.
pub(crate) fn outcome(o: ToolOutcome) -> ToolOutcomeState {
    match o {
        ToolOutcome::Succeeded => ToolOutcomeState::Succeeded,
        ToolOutcome::Failed => ToolOutcomeState::Failed,
        ToolOutcome::Blocked => ToolOutcomeState::Blocked,
        ToolOutcome::Denied => ToolOutcomeState::Denied,
        ToolOutcome::Indeterminate => ToolOutcomeState::Indeterminate,
    }
}

/// A file an execution produced.
pub(crate) fn artifact(a: &leviath_core::output::Artifact) -> ArtifactState {
    ArtifactState {
        name: a.name.clone(),
        path: a.path.clone(),
        mime_type: a.mime_type.as_str().to_string(),
        size: a.size,
        sha256: a.sha256.clone(),
    }
}

/// What a question asked for.
pub(crate) fn question_kind(kind: &InteractionKind) -> QuestionKind {
    match kind {
        InteractionKind::FreeText => QuestionKind::FreeText,
        InteractionKind::MultipleChoice => QuestionKind::MultipleChoice,
        InteractionKind::Confirm => QuestionKind::Confirm,
        InteractionKind::ToolApproval => QuestionKind::ToolApproval,
        InteractionKind::EditText => QuestionKind::EditText,
    }
}

/// What caused a context change.
pub(crate) fn cause(c: ContextCause) -> CauseState {
    match c {
        ContextCause::Seed => CauseState::Seed,
        ContextCause::Message => CauseState::Message,
        ContextCause::ModelReply => CauseState::ModelReply,
        ContextCause::ToolResult => CauseState::ToolResult,
        ContextCause::ProducedPart => CauseState::ProducedPart,
        ContextCause::Compaction => CauseState::Compaction,
        ContextCause::Transform => CauseState::Transform,
        ContextCause::ContextTool => CauseState::ContextTool,
        ContextCause::Hook => CauseState::Hook,
        ContextCause::FanOut => CauseState::FanOut,
        ContextCause::Interaction => CauseState::Interaction,
        ContextCause::Resume => CauseState::Resume,
        ContextCause::Framework => CauseState::Framework,
    }
}

/// One region's side of a committed context change.
pub(crate) fn region_commit(r: &RegionCommit) -> RegionCommitState {
    RegionCommitState {
        region: r.region.clone(),
        digest_before: r.digest_before.clone(),
        digest_after: r.digest_after.clone(),
        tokens_before: r.tokens_before as u64,
        tokens_after: r.tokens_after as u64,
        entries_before: r.entries_before as u64,
        entries_after: r.entries_after as u64,
        entries_added: r.entries_added as u64,
    }
}

#[cfg(test)]
#[path = "recorded_tests.rs"]
mod tests;
