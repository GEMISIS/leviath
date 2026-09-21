//! What one provider call actually took: every attempt at it, and every move to
//! a different provider when one would not serve.
//!
//! A run bills one [`RunRecord::InferenceUsage`](super::RunRecord::InferenceUsage)
//! per call that *worked*, which is the right shape for an invoice and the wrong
//! shape for a post-mortem. A call that was refused three times and answered on
//! the fourth is journaled identically to one that was answered at once, and a
//! call that moved from one provider to another leaves nothing behind at all. The
//! records here are the missing half: one per trip to a provider, plus one per
//! failover, so "why did this turn take ninety seconds" has an answer that does
//! not depend on the daemon's log still being around.
//!
//! Small by default. Timing, classification, and enough identity
//! ([`RequestDigest`]) to answer whether two attempts sent the same thing: no
//! response body, no error message, and no request body unless the operator
//! asked for one. A record that grew with the prompt would put a copy of the
//! whole window in the journal once per retry, which is why [`ModelInput`]
//! carries a body only where [`CaptureStatus::Retained`] says it does.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Cheap identity for a request: enough to tell whether two attempts sent the
/// same thing, and nothing more.
///
/// Every field is something the assembly already computed, so producing this
/// costs no hashing of its own. `system_hash` is the digest the prefix-cache
/// decision is made from, which is the one number that moves when the system
/// blocks change; the counts and the sampling knobs cover the rest of what an
/// attempt could differ by.
///
/// The per-block digests are left out on purpose. They are a vector as long as
/// the stage has blocks, and this record is written once per attempt: the whole
/// point of a digest here is that it is a fixed hundred bytes whatever the run
/// is doing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestDigest {
    /// The digest of the assembled system prefix.
    pub system_hash: u64,
    /// How many conversation messages went out.
    pub messages: usize,
    /// How many tools the request advertised.
    pub tools: usize,
    /// The completion budget it asked for.
    pub max_tokens: usize,
    /// The sampling temperature it asked for.
    pub temperature: f32,
}

/// Whether an attempt's exact request is in the journal, and where it went if
/// not.
///
/// Every variant describes the state of the *record*, not the intent behind it,
/// because that is what a reader can act on: a body that is here can be read, a
/// body that was never taken cannot be recovered, and a body that was taken and
/// then removed is a different fact from one that never existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureStatus {
    /// The request is in this record, byte for byte as it was handed to the
    /// provider adapter.
    Retained,
    /// Capture was off for this run, so no body was ever taken. The rest of
    /// [`ModelInput`] still describes the attempt.
    #[default]
    NotCaptured,
    /// A body was captured and then deliberately scrubbed.
    Redacted,
    /// A body was captured and then aged out.
    Expired,
}

/// What one attempt sent, and what the request was assembled from.
///
/// Written per attempt whether or not capture is on, because everything here
/// except `request` is cheap and answers questions a digest cannot: which
/// sampling knobs were really in force after resolution, which tools the model
/// was offered, and which build of the assembly produced the shape.
///
/// `request` is the whole prompt. It holds whatever the run's context held -
/// file contents, command output, credentials a tool read - so it is written
/// only for a run whose operator asked for it, and there is no size cap on it:
/// every call re-sends the window, so a captured run's journal grows by roughly
/// the context size per attempt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelInput {
    /// Whether `request` is here, and where it went if it is not.
    pub capture_status: CaptureStatus,
    /// The request as it was handed to the provider adapter, serialized exactly
    /// as the adapter received it. Absent unless `capture_status` is
    /// [`CaptureStatus::Retained`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<serde_json::Value>,
    /// Bytes the captured body took, so the cost of capture is readable from a
    /// record whose body has since been removed. Zero where no body was ever
    /// taken.
    pub bytes: u64,
    /// The fingerprint of the context this request was assembled from, as
    /// [`ContextDigest::fingerprint`](super::ContextDigest::fingerprint)
    /// computes it, so an attempt joins to the window it came from. Empty where
    /// no body was taken: the fingerprint costs a walk of the whole window, and
    /// a run that is not being captured should not pay for one.
    pub source_context_digest: String,
    /// The parameters the request really carried, after every override and
    /// clamp: the sampling temperature, the completion budget the window left
    /// room for, and any provider-specific keys. What a stage *declared* is in
    /// its blueprint and can differ from all of these.
    pub parameters: BTreeMap<String, serde_json::Value>,
    /// An identifier for the tool set this attempt offered the model. Two
    /// attempts offering the same tools share it; nothing else is promised
    /// about the value.
    pub tool_catalog_version: String,
    /// The version of the prompt-assembly logic that produced the request, so a
    /// captured body stays interpretable once assembly changes.
    pub assembly_version: String,
}

/// What the retry loop did after an attempt failed.
///
/// The distinction a reader needs is "was the same thing sent again, and why":
/// three records with the same [`RequestDigest`] are a provider that kept
/// refusing, while three with a different one are a request that kept changing
/// underneath the run. A move to a *different* provider is not here, because the
/// loop never makes one: see [`FailoverRecord`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Retry {
    /// Nothing. The failure was handed back to the run, either because it was
    /// permanent or because the attempts or the backoff budget ran out.
    Reported,
    /// The same provider and model again, after a backoff. The next attempt's
    /// record says how long that wait really was.
    SameModel,
    /// The same provider and model again, at once, with every file the request
    /// named uploaded afresh because the vendor said one of them was gone. It
    /// spends no backoff and no attempt of the retry budget, so it is the one
    /// case where two attempts can share a wait of zero.
    RenewedFiles,
}

/// How one attempt ended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptOutcome {
    /// The provider answered. What the answer cost is journaled separately, as
    /// the call's usage record.
    Succeeded,
    /// The call did not produce an answer.
    Failed {
        /// A stable label for what went wrong
        /// (`FailureKind::label`), or empty when the error carried no
        /// classification at all.
        kind: String,
        /// Whether a retry could plausibly clear it. The two halves of the
        /// retry decision are recorded rather than inferred: what counts as
        /// transient is a policy that changes between releases, and a record
        /// that only said which error occurred would be read against whatever
        /// the policy says today.
        transient: bool,
        /// Whether the provider said it was at capacity, which is what buys the
        /// slow backoff schedule rather than the blip-sized one.
        capacity: bool,
        /// What the loop did next.
        next: Retry,
    },
}

/// One attempt at one provider call.
///
/// Written per trip to the provider, including the first, and including the ones
/// that failed. This is the record that was missing: a retried call and a
/// first-time success were indistinguishable in the journal, so the time a run
/// spent being refused was invisible and a failover looked like a run that had
/// simply always used the second provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttemptRecord {
    /// This attempt's own id, minted before the request went out.
    ///
    /// What anything the attempt produced names it by: a tool batch the model
    /// asked for in its answer records the attempt that carried the answer, and
    /// the stage and the attempt number cannot serve for that - a stage makes
    /// hundreds of attempts and the number restarts at every call.
    ///
    /// Empty in a journal written before attempts had identity, where the number
    /// within a call was all there was.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// The stage the run was in. Empty for a lane that has no stage of its own.
    pub stage: String,
    /// Which attempt this was, from 1, counting every trip to the provider. The
    /// file-renewal retry gets its own number even though it spends none of the
    /// retry budget, because the point of the number is that two records for one
    /// call can be told apart.
    pub attempt: u32,
    /// The provider that was called.
    pub provider: String,
    /// The model it was asked for.
    pub model: String,
    /// How it ended.
    pub outcome: AttemptOutcome,
    /// How long this attempt itself took, excluding the wait before it.
    pub duration_ms: u64,
    /// How long the loop slept before making this attempt. Zero for the first,
    /// and for a retry taken at once.
    pub backoff_ms: u64,
    /// What went out, as much of it as is worth keeping.
    pub digest: RequestDigest,
    /// What went out exactly, when the run was asked to keep it, and what the
    /// request was assembled from either way. Absent in a journal whose writer
    /// recorded no model input at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_input: Option<ModelInput>,
    /// Unix seconds when the attempt finished.
    pub at: i64,
}

/// One provider was unusable, so the next configured model is being tried.
///
/// Its own record rather than a field on [`AttemptRecord`], because the decision
/// is made somewhere else and later: the job reports its failure, the tick loop
/// collects it, and only then does the stage look at what else it was given. By
/// that point the attempt that failed has already been journaled, and an
/// append-only journal cannot go back and amend it.
///
/// Paired with the attempts it sits between, this is what separates "the same
/// provider refused four times" from "four providers each refused once": the
/// attempt records say what was tried, and these say when the target moved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FailoverRecord {
    /// The stage whose call failed over.
    pub stage: String,
    /// The stage-local iteration. Unchanged by the failover, because the agent
    /// still has not had a turn.
    pub iteration: usize,
    /// The provider that would not serve.
    pub from_provider: String,
    /// The model it was asked for.
    pub from_model: String,
    /// The provider being tried instead.
    pub to_provider: String,
    /// The model being asked of it.
    pub to_model: String,
    /// Why the first provider was judged unusable
    /// (`UnavailableReason::label`).
    pub reason: String,
    /// A stable label for the failure itself (`FailureKind::label`), or empty
    /// when the error carried no classification.
    pub kind: String,
    /// Unix seconds when the move was made.
    pub at: i64,
}
