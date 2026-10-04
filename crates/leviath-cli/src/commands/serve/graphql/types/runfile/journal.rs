//! The steps' events that keep the work around a run whole: each model call,
//! each execution's identity and ending, the files it produced, a settled
//! question, and a context change with its cause.

use async_graphql::{ID, SimpleObject};
use leviath_graphql_derive::mirror;
use leviath_runtime::state::journal::{
    AttemptOutcomeState, AttemptState, CaptureState, CauseState, ContextCommitState,
    ContextNoteState, ModelInputState, QuestionKind, RegionCommitState, RequestDigestState,
    RetryState, SettledState, ToolOutcomeState,
};

use super::super::super::scalars::{BigInt, Json, Timestamp};
use super::super::context_change::ContextCause;
use super::super::execution::ToolOutcome;
use super::super::inference::{AttemptOutcomeKind, CaptureStatus, RetryDecision};
use super::super::interaction::InteractionKind;
use super::{big, saturating};

/// What one model request was, in brief.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct RequestBrief {
    /// A hash of the system prompt.
    pub(crate) system_hash: BigInt,
    /// How many messages it carried.
    pub(crate) messages: i32,
    /// How many tools it offered.
    pub(crate) tools: i32,
    /// The reply cap it asked for.
    pub(crate) max_tokens: i32,
    /// The sampling temperature.
    pub(crate) temperature: f64,
}

impl From<&RequestDigestState> for RequestBrief {
    fn from(d: &RequestDigestState) -> Self {
        Self {
            system_hash: big(d.system_hash),
            messages: saturating(d.messages),
            tools: saturating(d.tools),
            max_tokens: saturating(d.max_tokens),
            temperature: f64::from(d.temperature),
        }
    }
}

/// A captured model request.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct CapturedRequest {
    /// Whether the body was kept.
    pub(crate) capture: CaptureStatus,
    /// The request as sent, when it was kept.
    pub(crate) request: Option<Json>,
    /// The body's size in bytes.
    pub(crate) bytes: BigInt,
    /// A digest of the context it was assembled from.
    pub(crate) source_context_digest: String,
    /// The sampling parameters it was sent with, by name.
    pub(crate) parameters: Json,
    /// The version of the tool catalog it offered.
    pub(crate) tool_catalog_version: String,
    /// The version of the assembly that built it.
    pub(crate) assembly_version: String,
}

impl From<&ModelInputState> for CapturedRequest {
    fn from(m: &ModelInputState) -> Self {
        Self {
            capture: match m.capture {
                CaptureState::Retained => CaptureStatus::Retained,
                CaptureState::NotCaptured => CaptureStatus::NotCaptured,
                CaptureState::Redacted => CaptureStatus::Redacted,
                CaptureState::Expired => CaptureStatus::Expired,
            },
            request: m.request.as_ref().map(|doc| Json(doc.value().clone())),
            bytes: big(m.bytes),
            source_context_digest: m.source_context_digest.clone(),
            parameters: Json(serde_json::Value::Object(
                m.parameters
                    .iter()
                    .map(|(k, v)| (k.clone(), v.value().clone()))
                    .collect(),
            )),
            tool_catalog_version: m.tool_catalog_version.clone(),
            assembly_version: m.assembly_version.clone(),
        }
    }
}

/// How a failed model call failed, and what happened next.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct CallFailure {
    /// The kind of failure, as the provider layer names it.
    pub(crate) kind: String,
    /// Whether a retry could work.
    pub(crate) transient: bool,
    /// Whether the provider was out of capacity.
    pub(crate) capacity: bool,
    /// What happened next.
    pub(crate) next: RetryDecision,
}

/// A model call, in full.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct AttemptStep {
    /// The attempt's id.
    pub(crate) attempt_id: ID,
    /// Which try at this request it was, from 1.
    pub(crate) number: i32,
    /// The provider asked.
    pub(crate) provider: String,
    /// The model asked.
    pub(crate) model: String,
    /// Whether it answered.
    pub(crate) ended: AttemptOutcomeKind,
    /// How it failed, when it did.
    pub(crate) failure: Option<CallFailure>,
    /// Why the model stopped, in this build's words.
    pub(crate) stop_reason: Option<String>,
    /// Why it stopped, in the provider's words, when this build has no name
    /// for it.
    pub(crate) stopped_for: Option<String>,
    /// How long it took.
    pub(crate) duration_ms: BigInt,
    /// How long it waited before it was sent.
    pub(crate) backoff_ms: BigInt,
    /// What was sent, in brief.
    pub(crate) digest: RequestBrief,
    /// The request itself, when it was captured.
    pub(crate) model_input: Option<CapturedRequest>,
}

impl From<&AttemptState> for AttemptStep {
    fn from(a: &AttemptState) -> Self {
        let (ended, failure) = match &a.outcome {
            AttemptOutcomeState::Succeeded => (AttemptOutcomeKind::Succeeded, None),
            AttemptOutcomeState::Failed {
                kind,
                transient,
                capacity,
                next,
            } => (
                AttemptOutcomeKind::Failed,
                Some(CallFailure {
                    kind: kind.clone(),
                    transient: *transient,
                    capacity: *capacity,
                    next: match next {
                        RetryState::Reported => RetryDecision::Reported,
                        RetryState::SameModel => RetryDecision::SameModel,
                        RetryState::RenewedFiles => RetryDecision::RenewedFiles,
                    },
                }),
            ),
        };
        Self {
            attempt_id: ID(a.id.clone()),
            number: saturating(a.number),
            provider: a.provider.clone(),
            model: a.model.clone(),
            ended,
            failure,
            stop_reason: a.finish_reason.clone(),
            stopped_for: a.stopped_for.clone(),
            duration_ms: big(a.duration_ms),
            backoff_ms: big(a.backoff_ms),
            digest: RequestBrief::from(&a.digest),
            model_input: a.model_input.as_ref().map(CapturedRequest::from),
        }
    }
}

/// A tool call dispatched as one execution.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct DispatchedStep {
    /// The provider's id for the call.
    pub(crate) call_id: String,
    /// The execution's own id.
    pub(crate) execution_id: ID,
    /// The model call that asked for it.
    pub(crate) requested_by: String,
}

/// An execution that ended.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct CompletedStep {
    /// The provider's id for the call.
    pub(crate) call_id: String,
    /// The execution's own id.
    pub(crate) execution_id: ID,
    /// How it ended, when that was observed.
    pub(crate) outcome: Option<ToolOutcome>,
    /// The names of the stored parts its result carried.
    pub(crate) parts: Vec<String>,
}

/// How an execution ended, as this schema names it.
pub(crate) fn outcome_of(o: ToolOutcomeState) -> ToolOutcome {
    match o {
        ToolOutcomeState::Succeeded => ToolOutcome::Succeeded,
        ToolOutcomeState::Failed => ToolOutcome::Failed,
        ToolOutcomeState::Blocked => ToolOutcome::Blocked,
        ToolOutcomeState::Denied => ToolOutcome::Denied,
        ToolOutcomeState::Indeterminate => ToolOutcome::Indeterminate,
    }
}

/// A file an execution produced.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct ProducedFile {
    /// What it is called.
    pub(crate) name: String,
    /// Where it is, inside the run's working directory.
    pub(crate) path: String,
    /// Its mime type.
    pub(crate) mime_type: String,
    /// Its size in bytes.
    pub(crate) size: BigInt,
    /// The sha256 of its bytes, when it was taken.
    pub(crate) sha256: String,
}

/// The files an execution produced.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct ArtifactsStep {
    /// The execution.
    pub(crate) producer_id: ID,
    /// The files.
    pub(crate) files: Vec<ProducedFile>,
}

/// A question a person settled, whole.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct SettledStep {
    /// The question.
    pub(crate) settled_id: ID,
    /// What it asked for.
    pub(crate) kind: InteractionKind,
    /// The tool an approval was for.
    pub(crate) tool: Option<String>,
    /// The question as the person saw it.
    pub(crate) prompt: String,
    /// The stage the run was in when it asked.
    pub(crate) stage: String,
    /// How it ended, as the settlement's JSON.
    pub(crate) settlement: Json,
    /// When it was asked.
    pub(crate) asked_at: Timestamp,
}

impl From<&SettledState> for SettledStep {
    fn from(s: &SettledState) -> Self {
        Self {
            settled_id: ID(s.id.clone()),
            kind: match s.kind {
                QuestionKind::FreeText => InteractionKind::FreeText,
                QuestionKind::MultipleChoice => InteractionKind::MultipleChoice,
                QuestionKind::Confirm => InteractionKind::Confirm,
                QuestionKind::ToolApproval => InteractionKind::ToolApproval,
                QuestionKind::EditText => InteractionKind::EditText,
            },
            tool: s.tool.clone(),
            prompt: s.prompt.clone(),
            stage: s.stage.clone(),
            settlement: Json(
                serde_json::from_str(&s.settlement)
                    .unwrap_or_else(|_| serde_json::Value::String(s.settlement.clone())),
            ),
            asked_at: Timestamp(s.asked_at),
        }
    }
}

/// What caused a context change, as this schema names it.
pub(crate) fn cause_of(c: CauseState) -> ContextCause {
    match c {
        CauseState::Seed => ContextCause::Seed,
        CauseState::Message => ContextCause::Message,
        CauseState::ModelReply => ContextCause::ModelReply,
        CauseState::ToolResult => ContextCause::ToolResult,
        CauseState::ProducedPart => ContextCause::ProducedPart,
        CauseState::Compaction => ContextCause::Compaction,
        CauseState::Transform => ContextCause::Transform,
        CauseState::ContextTool => ContextCause::ContextTool,
        CauseState::Hook => ContextCause::Hook,
        CauseState::FanOut => ContextCause::FanOut,
        CauseState::Interaction => ContextCause::Interaction,
        CauseState::Resume => ContextCause::Resume,
        CauseState::Framework => ContextCause::Framework,
    }
}

/// One region's side of a committed context change.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct CommittedRegion {
    /// The region.
    pub(crate) region: String,
    /// Its content digest before.
    pub(crate) digest_before: String,
    /// Its content digest after.
    pub(crate) digest_after: String,
    /// Its tokens before.
    pub(crate) tokens_before: BigInt,
    /// Its tokens after.
    pub(crate) tokens_after: BigInt,
    /// Its entries before.
    pub(crate) entries_before: BigInt,
    /// Its entries after.
    pub(crate) entries_after: BigInt,
    /// Entries the change itself pushed.
    pub(crate) entries_added: BigInt,
}

impl From<&RegionCommitState> for CommittedRegion {
    fn from(r: &RegionCommitState) -> Self {
        Self {
            region: r.region.clone(),
            digest_before: r.digest_before.clone(),
            digest_after: r.digest_after.clone(),
            tokens_before: big(r.tokens_before),
            tokens_after: big(r.tokens_after),
            entries_before: big(r.entries_before),
            entries_after: big(r.entries_after),
            entries_added: big(r.entries_added),
        }
    }
}

/// A context change with its cause and the window revisions on either side.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct ContextCommittedStep {
    /// What caused it.
    pub(crate) cause: ContextCause,
    /// The execution it came from, when a tool's did.
    pub(crate) committed_by: Option<ID>,
    /// The window's revision before.
    pub(crate) revision_before: String,
    /// The window's revision after.
    pub(crate) revision_after: String,
    /// Each region it moved.
    pub(crate) regions: Vec<CommittedRegion>,
}

impl From<&ContextCommitState> for ContextCommittedStep {
    fn from(c: &ContextCommitState) -> Self {
        Self {
            cause: cause_of(c.cause),
            committed_by: c.execution_id.as_ref().map(|id| ID(id.clone())),
            revision_before: c.revision_before.clone(),
            revision_after: c.revision_after.clone(),
            regions: c.regions.iter().map(CommittedRegion::from).collect(),
        }
    }
}

/// A change to one region with its cause.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct ContextNotedStep {
    /// The region.
    pub(crate) region: String,
    /// What caused it.
    pub(crate) cause: ContextCause,
    /// Entries it added.
    pub(crate) entries_added: i32,
    /// Entries it removed.
    pub(crate) entries_removed: i32,
    /// How many tokens it moved the region by.
    pub(crate) token_delta: BigInt,
}

impl From<&ContextNoteState> for ContextNotedStep {
    fn from(n: &ContextNoteState) -> Self {
        Self {
            region: n.region.clone(),
            cause: cause_of(n.cause),
            entries_added: saturating(n.entries_added),
            entries_removed: saturating(n.entries_removed),
            token_delta: BigInt(n.token_delta),
        }
    }
}
