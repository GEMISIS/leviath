//! What a step records about the work around it: each model call in full,
//! each tool execution's identity and ending, what an execution produced, how
//! a person settled a question, and why the context changed.
//!
//! The state says where a run is; these say how it got there, for the reads
//! that answer "what did this run do": the attempts a provider call took, a
//! request that was captured, the execution a context change came from. Each
//! is a [`RunEvent`](super::RunEvent) of the step it happened in, read from
//! the journal record the world hands the persistence lane.

use std::collections::BTreeMap;

use leviath_core::JsonDoc;
use serde::{Deserialize, Serialize};

/// What one model request was, in brief: enough to tell two requests apart
/// without keeping either.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RequestDigestState {
    /// A hash of the system prompt.
    pub system_hash: u64,
    /// How many messages the request carried.
    pub messages: u32,
    /// How many tools it offered.
    pub tools: u32,
    /// The reply cap it asked for.
    pub max_tokens: u32,
    /// The sampling temperature.
    pub temperature: f32,
}

/// Whether a request's body was kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum CaptureState {
    /// Kept, and readable.
    Retained,
    /// Capture was off.
    NotCaptured,
    /// Kept with its secrets taken out.
    Redacted,
    /// Kept once and removed since.
    Expired,
}

/// A captured model request: the body when it was kept, and what it was
/// built from either way.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ModelInputState {
    /// Whether the body was kept.
    pub capture: CaptureState,
    /// The request as sent, when it was kept.
    pub request: Option<JsonDoc>,
    /// The body's size in bytes.
    pub bytes: u64,
    /// A digest of the context it was assembled from.
    pub source_context_digest: String,
    /// The sampling parameters it was sent with.
    pub parameters: BTreeMap<String, JsonDoc>,
    /// The version of the tool catalog it offered.
    pub tool_catalog_version: String,
    /// The version of the assembly that built it.
    pub assembly_version: String,
}

/// What a failed model call went on to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum RetryState {
    /// Nothing: the failure was reported.
    Reported,
    /// The same model was asked again.
    SameModel,
    /// The same model was asked again with its uploaded files renewed.
    RenewedFiles,
}

/// How a model call ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum AttemptOutcomeState {
    /// The provider answered.
    Succeeded,
    /// It did not.
    Failed {
        /// The kind of failure, as the provider layer names it.
        kind: String,
        /// Whether a retry could work.
        transient: bool,
        /// Whether the provider was out of capacity.
        capacity: bool,
        /// What happened next.
        next: RetryState,
    },
}

/// One trip to a provider, whether or not it produced an answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AttemptState {
    /// The attempt's id.
    pub id: String,
    /// Which try at this request it was, from 1.
    pub number: u32,
    /// The provider asked.
    pub provider: String,
    /// The model asked.
    pub model: String,
    /// How it ended.
    pub outcome: AttemptOutcomeState,
    /// Why the model stopped, in this build's words.
    pub finish_reason: Option<String>,
    /// Why it stopped, in the provider's words, when this build has no name
    /// for it.
    pub stopped_for: Option<String>,
    /// How long it took.
    pub duration_ms: u64,
    /// How long it waited before it was sent.
    pub backoff_ms: u64,
    /// What was sent, in brief.
    pub digest: RequestDigestState,
    /// The request itself, when it was captured.
    pub model_input: Option<ModelInputState>,
}

/// How a tool execution ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum ToolOutcomeState {
    /// The tool ran and answered.
    Succeeded,
    /// The tool ran and failed.
    Failed,
    /// A gate refused it before it ran.
    Blocked,
    /// A person refused it.
    Denied,
    /// Nobody saw how it ended.
    Indeterminate,
}

/// A file an execution produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ArtifactState {
    /// What it is called.
    pub name: String,
    /// Where it is, inside the run's workdir.
    pub path: String,
    /// Its mime type.
    pub mime_type: String,
    /// Its size in bytes.
    pub size: u64,
    /// The sha256 of its bytes, when it was taken.
    pub sha256: String,
}

/// What a question asked a person for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum QuestionKind {
    /// Free text.
    FreeText,
    /// One of a set of options.
    MultipleChoice,
    /// Yes or no.
    Confirm,
    /// Whether a tool call may run.
    ToolApproval,
    /// An edit to a document.
    EditText,
}

/// A question a person settled, whole.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SettledState {
    /// The question's id.
    pub id: String,
    /// What it asked for.
    pub kind: QuestionKind,
    /// The tool an approval was for.
    pub tool: Option<String>,
    /// The question as the person saw it.
    pub prompt: String,
    /// The stage the run was in when it asked.
    pub stage: String,
    /// How it ended, as the settlement's JSON.
    pub settlement: String,
    /// When it was asked, in unix seconds.
    pub asked_at: i64,
}

/// What changed a run's context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum CauseState {
    /// A seed at spawn.
    Seed,
    /// A message.
    Message,
    /// The model's reply.
    ModelReply,
    /// A tool's result.
    ToolResult,
    /// A part a tool produced.
    ProducedPart,
    /// A compaction.
    Compaction,
    /// A stage transform.
    Transform,
    /// A context tool the model called.
    ContextTool,
    /// A hook.
    Hook,
    /// A fan-out's results.
    FanOut,
    /// A person's answer.
    Interaction,
    /// A resume.
    Resume,
    /// The runtime's own bookkeeping: a nudge, a watchdog note.
    Framework,
}

/// One region's side of a context change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RegionCommitState {
    /// The region.
    pub region: String,
    /// Its content digest before.
    pub digest_before: String,
    /// Its content digest after.
    pub digest_after: String,
    /// Its tokens before.
    pub tokens_before: u64,
    /// Its tokens after.
    pub tokens_after: u64,
    /// Its entries before.
    pub entries_before: u64,
    /// Its entries after.
    pub entries_after: u64,
    /// Entries the change itself pushed, which the counts either side do not
    /// show when the region evicted one to make room.
    pub entries_added: u64,
}

/// A change to a run's context, with its cause: the regions it moved and the
/// window revisions on either side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ContextCommitState {
    /// What caused it.
    pub cause: CauseState,
    /// The execution it came from, when a tool's did.
    pub execution_id: Option<String>,
    /// The window's revision before.
    pub revision_before: String,
    /// The window's revision after.
    pub revision_after: String,
    /// Each region it moved.
    pub regions: Vec<RegionCommitState>,
}

/// A change to one region with its cause, where no revision was taken.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ContextNoteState {
    /// The region.
    pub region: String,
    /// What caused it.
    pub cause: CauseState,
    /// Entries it added.
    pub entries_added: u32,
    /// Entries it removed.
    pub entries_removed: u32,
    /// How many tokens it moved the region by.
    pub token_delta: i64,
}

#[cfg(test)]
#[path = "journal_tests.rs"]
pub(crate) mod tests;
