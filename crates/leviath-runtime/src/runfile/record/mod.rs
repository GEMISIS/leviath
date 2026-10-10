//! What the world is told about a run besides its state: one record per
//! thing that happened.
//!
//! A step's state says where the run is; it cannot say what it did on the way.
//! The systems and tasks that make a model call, dispatch tools, settle a
//! question or commit a change to the window each send a [`RunRecord`] to the
//! world's journal as it happens, and the persist system folds it into the
//! [`RunEvent`](crate::state::RunEvent)s of the run's next step (see
//! [`journal_events`](super::journal_events)).

use serde::{Deserialize, Serialize};

use leviath_core::region::EntryContent;
use leviath_core::run_meta::{ContextSnapshot, RegionEntrySnapshot};

mod attempt;

pub use attempt::{
    AttemptOutcome, AttemptRecord, CaptureStatus, FailoverRecord, ModelInput, RequestDigest, Retry,
};

/// A single tool call and (once executed) its result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallRecord {
    /// The tool-call id, as the provider assigned it.
    ///
    /// Correlation, not identity: a provider may reuse one across a retry or a
    /// reissue, so two attempts can arrive under one id. What tells them apart
    /// is [`execution_id`](Self::execution_id).
    pub id: String,
    /// This attempt's own id, minted at dispatch. Empty where the run recorded
    /// none, which a reader treats as "this attempt was not identified" rather
    /// than as an attempt with a blank name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub execution_id: String,
    /// The tool name.
    pub name: String,
    /// The JSON arguments, stringified.
    pub arguments: String,
    /// The result, once the tool has run (`None` while pending): text and any
    /// stored parts the tool produced.
    pub result: Option<EntryContent>,
    /// Opaque provider token that must be replayed with this call (Gemini's
    /// `thought_signature`). Carried so a restored batch can rebuild the exact
    /// assistant turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
}

/// Which provider call a [`RunRecord::InferenceUsage`] belongs to.
///
/// A run bills for more than its stage turns, and the three auxiliary kinds are
/// invisible in every other surface: they do not appear in the stage ledger and
/// nothing else names them. Recording which kind spent the tokens is what turns
/// a total into an explanation: "this run cost double what its stages did
/// because its edges compact".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceKind {
    /// An ordinary stage turn: the agent thinking or calling tools. The default,
    /// so a usage record that names no kind reads as stage work.
    #[default]
    Stage,
    /// A region-summarizing call, from memory pressure or an edge transform.
    Compaction,
    /// The one-off call that names the run.
    Title,
    /// A call asking the model which stage to move to next.
    Routing,
}

impl InferenceKind {
    /// A short stable label, for logs and wire formats that want a string.
    pub fn label(&self) -> &'static str {
        match self {
            InferenceKind::Stage => "stage",
            InferenceKind::Compaction => "compaction",
            InferenceKind::Title => "title",
            InferenceKind::Routing => "routing",
        }
    }
}

/// One region's part in a committed transaction.
///
/// Every field is something the write path already had in hand: the shape of the
/// region either side of the change, and the digest of what it held. Nothing here
/// is derived, so nothing here can disagree with the window beside it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegionCommit {
    /// The region this part of the transaction touched.
    pub region: String,
    /// The digest of its contents before the change.
    pub digest_before: String,
    /// The digest of its contents after it.
    pub digest_after: String,
    /// What it held before, in tokens.
    pub tokens_before: usize,
    /// What it held after.
    pub tokens_after: usize,
    /// How many entries it held before.
    pub entries_before: usize,
    /// How many it held after.
    pub entries_after: usize,
    /// How many entries the change itself pushed.
    ///
    /// Not derivable from the counts either side: a write that appends one entry
    /// into a full sliding region leaves the count where it was, and the entry
    /// that left is the region's own eviction rather than part of the write.
    pub entries_added: usize,
}

/// One question this run asked a person, and how it ended.
///
/// The record that a run stopped for somebody. A reader listing these can say
/// which calls a person allowed, at what scope, and which ones nobody answered,
/// none of which is recoverable from the tool results alone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InteractionRecord {
    /// The request id the hub minted.
    pub request_id: String,
    /// What was asked for.
    pub kind: leviath_core::interaction::InteractionKind,
    /// The tool an approval was for.
    pub tool: Option<String>,
    /// The question as the person saw it.
    pub prompt: String,
    /// The stage the run was in when it asked.
    pub stage: String,
    /// How it ended.
    pub settlement: leviath_core::interaction::Settlement,
    /// Unix seconds when it was asked.
    pub asked_at: i64,
    /// Unix seconds when it settled.
    pub at: i64,
}

/// One thing a run did, sent to the world's journal as it happens.
#[derive(Debug, Clone, PartialEq)]
pub enum RunRecord {
    /// What one provider call cost, sent as it lands.
    ///
    /// Per call rather than cumulative, so two calls between two steps stay two
    /// calls, and "no request ever exceeded the window" is provable from the run
    /// file rather than inferred.
    InferenceUsage {
        /// Which kind of call this was.
        kind: InferenceKind,
        /// The stage the run was in. Empty for a call with no stage of its own
        /// (the title call, which runs once at spawn).
        stage: String,
        /// The stage-local iteration index.
        iteration: usize,
        /// The provider that served the call.
        provider: String,
        /// The model the call targeted.
        model: String,
        /// Prompt tokens billed.
        prompt_tokens: usize,
        /// Completion tokens billed.
        completion_tokens: usize,
        /// Tokens read from provider cache.
        cached_tokens: usize,
        /// Tokens written to provider cache.
        cache_write_tokens: usize,
        /// What this one call cost in USD, when it could be established.
        ///
        /// Per call rather than only per run, because the model can change
        /// mid-run: a stage that fails over to a second provider has spent at
        /// two different rates, and a run-level total cannot be re-derived
        /// afterwards.
        ///
        /// `None` means unpriced (the provider reported no cost and no rates
        /// were known), never that the call was free.
        cost_usd: Option<f64>,
        /// Whether `cost_usd` is the provider's own figure rather than one
        /// computed from published rates. Absent when there is no cost.
        cost_reported_by_provider: Option<bool>,
        /// Unix seconds.
        at: i64,
    },
    /// A batch of tool calls, sent when the batch is dispatched to the tool
    /// lane, before anything runs. Calls the dispatcher already resolved inline
    /// (context tools, refusals, gate denials) carry `result: Some(..)`; lane
    /// calls start at `result: None` and are completed by matching
    /// [`RunRecord::ToolCallDone`] records as each call finishes.
    ToolBatch {
        /// The calls (inline results pre-filled; lane calls pending).
        calls: Vec<ToolCallRecord>,
        /// Unix seconds.
        at: i64,
        /// The stage index the batch was dispatched in.
        stage_index: usize,
        /// The stage-local iteration that produced the batch: the batch key
        /// (one batch per iteration).
        iteration: usize,
        /// The stay in that stage the batch was dispatched during, as minted
        /// when the run entered it. Empty in a world with no stage ledger.
        visit_id: String,
        /// The provider attempt whose answer asked for these calls, as minted
        /// before that request went out. Empty on a batch no answer asked for.
        requested_by: String,
        /// The assistant text of the turn that issued the calls.
        response: String,
    },
    /// One tool call of the pending batch finished; its result.
    ToolCallDone {
        /// The iteration of the [`RunRecord::ToolBatch`] this belongs to.
        iteration: usize,
        /// The tool-call id, as the provider assigned it. Correlation; see
        /// [`ToolCallRecord::id`].
        call_id: String,
        /// The attempt this completes, as minted at dispatch.
        execution_id: String,
        /// The result: text and any stored parts.
        ///
        /// For an indeterminate outcome this is the stand-in a resume put in the
        /// window, not something the tool returned. The outcome is what tells
        /// the two apart, and a reader showing this text as the tool's answer
        /// would be inventing one.
        result: EntryContent,
        /// How the attempt ended, where it cannot be read off the result: an
        /// execution a resume gave up on, which no later reader could
        /// distinguish from one that finished. `None` otherwise.
        outcome: Option<leviath_core::execution::ToolOutcome>,
        /// Unix seconds.
        at: i64,
    },
    /// Calls of a batch the run's file already records, sent to the tool lane
    /// again after the run came back: a question nobody had answered when the
    /// daemon stopped is asked again. Each keeps the execution it was first
    /// dispatched as, so a reader sees one execution sent twice rather than
    /// two executions.
    ToolCallsResent {
        /// The calls, as (provider call id, execution id).
        calls: Vec<(String, String)>,
        /// The provider attempt whose answer asked for them, when the run
        /// still knows it. Empty otherwise.
        requested_by: String,
        /// Unix seconds.
        at: i64,
    },
    /// Files one tool execution produced, sent as it produced them.
    ///
    /// The run's answer names the artifacts of its latest submission only, and
    /// says nothing of which call made any of them. This record is sent by the
    /// dispatcher that was handling the call, so an artifact stays attributable
    /// for as long as the run file exists, superseded submissions included.
    ArtifactsProduced {
        /// The execution that produced them, as minted at dispatch.
        execution_id: String,
        /// The files, exactly as the answer recorded them.
        artifacts: Vec<leviath_core::output::Artifact>,
        /// Unix seconds.
        at: i64,
    },
    /// A question this run put to a person, and what came back.
    ///
    /// The only record that a run stopped for someone. Without it an approved
    /// call is indistinguishable from one no policy ever stopped, and the scope
    /// a person chose (this call, this stage, the rest of the run) is gone the
    /// moment the tool reads its answer.
    Interaction {
        /// The request id the hub minted, which is what an answer arriving over
        /// the API or from `lev respond` carries.
        request_id: String,
        /// What was asked for.
        kind: leviath_core::interaction::InteractionKind,
        /// The tool an approval was for. `None` for every other kind.
        tool: Option<String>,
        /// The question as the person saw it.
        prompt: String,
        /// The stage the run was in when it asked.
        stage: String,
        /// How it ended.
        settlement: leviath_core::interaction::Settlement,
        /// Unix seconds when the question was asked.
        asked_at: i64,
        /// Unix seconds when it settled.
        at: i64,
    },
    /// One trip to a provider, whether or not it produced an answer.
    ///
    /// The usage records say what the calls that worked cost. These say what the
    /// run spent getting them, which is the half a retry or a failover otherwise
    /// leaves no trace of at all.
    InferenceAttempt(Box<AttemptRecord>),
    /// The run took an edge, sent as it is taken so a step that moves the run
    /// twice keeps both moves.
    Transition(crate::state::TransitionRecord),
    /// One provider judged unusable, and the model being tried instead.
    InferenceFailover(FailoverRecord),
    /// One committed transaction against the context window: what moved it, the
    /// window it started from and the window it produced, and every region it
    /// touched.
    ///
    /// The record a debugger joins on. A change carries the window's
    /// [revision](leviath_core::run_meta::revision) either side, so it is
    /// anchored to exact content rather than to a moment, and it carries every
    /// region of the transaction at once: a compaction that summarised one
    /// region and emptied another is one record, not two events that share a
    /// second.
    ///
    /// Carries no content: the state recorded in the same step holds the text,
    /// and the per-region digests here are what tell a reader whether it needs
    /// to go and read it.
    ContextTransaction {
        /// The window's revision before the transaction.
        revision_before: String,
        /// The window's revision after it.
        revision_after: String,
        /// What made the change.
        cause: leviath_core::ContextCause,
        /// Every region the transaction touched, in the order the write path
        /// named them.
        regions: Vec<RegionCommit>,
        /// The tool execution that committed it, as minted at dispatch. Empty
        /// where nothing knew of one: a write outside any tool call, and a tool
        /// whose results the batch applies rather than the call itself.
        execution_id: String,
        /// Unix seconds.
        at: i64,
    },
}

/// A fingerprint of a whole context window, as one opaque hex string, for a
/// record that needs to name a window rather than compare it.
///
/// Every region's name, kind, size and budget, and every field of every entry,
/// take part: two windows that differ anywhere fingerprint differently.
pub fn context_fingerprint(snapshot: &ContextSnapshot) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = leviath_core::hash::stable_hasher();
    for region in &snapshot.regions {
        region.name.hash(&mut hasher);
        region.kind.hash(&mut hasher);
        region.current_tokens.hash(&mut hasher);
        region.max_tokens.hash(&mut hasher);
        let entries: Vec<u64> = region.entries.iter().map(entry_digest).collect();
        entries.hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
}

/// Hash one region entry. Every field participates: two entries that differ
/// anywhere must digest differently.
fn entry_digest(entry: &RegionEntrySnapshot) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = leviath_core::hash::stable_hasher();
    entry.content.hash(&mut hasher);
    entry.tokens.hash(&mut hasher);
    entry.key.hash(&mut hasher);
    // kind / metadata / taint are small enums and values without a Hash impl;
    // their serialized form is tiny next to `content` and hashes faithfully.
    serde_json::to_string(&entry.kind)
        .expect("EntryKind always serializes")
        .hash(&mut hasher);
    serde_json::to_string(&entry.metadata)
        .expect("entry metadata always serializes")
        .hash(&mut hasher);
    serde_json::to_string(&entry.taint)
        .expect("taint always serializes")
        .hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
