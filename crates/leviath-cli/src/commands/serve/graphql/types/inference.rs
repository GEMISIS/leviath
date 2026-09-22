//! Every trip a run made to a provider, and the moves between providers.
//!
//! Read from the journal, because the answer is not in the run's totals: the
//! usage a run reports is per call that worked, so a call refused three times
//! and answered on the fourth is billed once and reads here as the four trips it
//! was.
//!
//! A move to another provider is a field on the attempt it followed rather than
//! a listing of its own. The journal could not do that - a failover is decided a
//! tick later, by the tick loop rather than by the lane that made the call, and
//! an append-only journal cannot amend a record it has already written - but a
//! reader holds both records at once and can put them back together, which is
//! the join a client would otherwise be left to guess at.

use async_graphql::{Enum, Object, SimpleObject};
use leviath_core::run_archive::{AttemptRecord, FailoverRecord};
use leviath_graphql_derive::mirror;

use super::super::connection::{
    Connection, Paged, PositionQuery, Total, position_order, position_page,
};
use super::super::error::IntoGraphql;
use super::super::filter::MatchCx;
use super::super::paging::page::page;
use super::super::scalars::{BigInt, Cursor, Json, Timestamp};
use super::manifest::model::ModelParameters;
use crate::commands::serve::blocking::blocking;
use crate::commands::serve::core::inferences;
use crate::commands::serve::cursor;

/// What the retry loop did after an attempt failed.
#[mirror]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum RetryDecision {
    /// Nothing. The failure went back to the run, either because it was
    /// permanent or because the attempts and the backoff budget were spent.
    Reported,
    /// The same provider and model again, after a wait. The next attempt's
    /// `backoffMs` says how long that wait really was.
    SameModel,
    /// The same provider and model again, at once, with every file the request
    /// named uploaded afresh because the provider said one of them was gone. It
    /// spends no wait and none of the retry budget, so it is the one case where
    /// two attempts can share a `backoffMs` of zero.
    RenewedFiles,
}

impl From<leviath_core::run_archive::Retry> for RetryDecision {
    fn from(retry: leviath_core::run_archive::Retry) -> Self {
        use leviath_core::run_archive::Retry as Core;
        match retry {
            Core::Reported => Self::Reported,
            Core::SameModel => Self::SameModel,
            Core::RenewedFiles => Self::RenewedFiles,
        }
    }
}

/// How one trip to a provider ended.
#[mirror]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum AttemptOutcomeKind {
    /// The provider answered.
    Succeeded,
    /// It produced no answer.
    Failed,
}

/// How one trip to a provider ended, and how the failure was judged when there
/// was one.
///
/// `failureKind`, `transient`, `capacity` and `retry` are null unless `kind` is
/// `FAILED`: an attempt that worked has no failure to classify. What the answer
/// cost is on the run's `usage` and `cost`, not here.
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct AttemptOutcome {
    /// Whether the provider answered.
    pub(crate) kind: AttemptOutcomeKind,
    /// A stable label for what went wrong. Null for an attempt that worked, and
    /// for a failure the provider gave no classification for at all.
    pub(crate) failure_kind: Option<String>,
    /// Whether a retry could plausibly have cleared it. Recorded as judged at
    /// the time rather than inferred now: what counts as transient is a policy
    /// that moves between releases.
    pub(crate) transient: Option<bool>,
    /// Whether the provider said it was at capacity, which is what buys the slow
    /// backoff schedule rather than the blip-sized one.
    pub(crate) capacity: Option<bool>,
    /// What the loop did next.
    pub(crate) retry: Option<RetryDecision>,
}

impl From<&leviath_core::run_archive::AttemptOutcome> for AttemptOutcome {
    fn from(outcome: &leviath_core::run_archive::AttemptOutcome) -> Self {
        use leviath_core::run_archive::AttemptOutcome as Core;
        // Every field but `kind` stays null on the arm that carries no failure,
        // so a client switching on `kind` first never has to check whether an
        // unrelated field is meaningful before reading it.
        match outcome {
            Core::Succeeded => Self {
                kind: AttemptOutcomeKind::Succeeded,
                failure_kind: None,
                transient: None,
                capacity: None,
                retry: None,
            },
            Core::Failed {
                kind,
                transient,
                capacity,
                next,
            } => Self {
                kind: AttemptOutcomeKind::Failed,
                failure_kind: label(kind),
                transient: Some(*transient),
                capacity: Some(*capacity),
                retry: Some(RetryDecision::from(*next)),
            },
        }
    }
}

/// What went out on one attempt, in the little of it worth keeping.
///
/// Two attempts carrying the same digest sent the same request, which is the
/// question a retry raises: the same provider refusing repeatedly reads
/// differently from a request that kept changing underneath the run. No bodies,
/// because a digest that grew with the prompt would put a copy of the whole
/// window in the journal once per retry.
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct RequestDigest {
    /// The assembled system prefix, as an opaque lowercase-hex digest. Compare
    /// it between attempts; nothing else is promised about the value.
    pub(crate) system_hash: String,
    /// How many conversation messages went out.
    pub(crate) messages: i32,
    /// How many tools the request advertised.
    pub(crate) tools: i32,
    /// The completion budget it asked for.
    pub(crate) max_tokens: i32,
    /// The sampling temperature it asked for.
    pub(crate) temperature: f64,
}

impl From<&leviath_core::run_archive::RequestDigest> for RequestDigest {
    fn from(digest: &leviath_core::run_archive::RequestDigest) -> Self {
        Self {
            system_hash: format!("{:016x}", digest.system_hash),
            messages: count(digest.messages),
            tools: count(digest.tools),
            max_tokens: count(digest.max_tokens),
            temperature: f64::from(digest.temperature),
        }
    }
}

/// Whether an attempt's exact request is in the journal, and where it went if
/// not.
///
/// Each value describes the state of the record rather than the intent behind
/// it: a body that is here can be read, a body that was never taken cannot be
/// recovered, and a body that was taken and then removed is a different fact
/// from one that never existed.
#[mirror]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum CaptureStatus {
    /// The request is on this record, as the provider adapter received it.
    Retained,
    /// Capture was off for this run, so no body was ever taken. Everything else
    /// on `modelInput` still describes the attempt.
    NotCaptured,
    /// A body was captured and then deliberately scrubbed.
    Redacted,
    /// A body was captured and then aged out.
    Expired,
}

impl From<leviath_core::run_archive::CaptureStatus> for CaptureStatus {
    fn from(status: leviath_core::run_archive::CaptureStatus) -> Self {
        use leviath_core::run_archive::CaptureStatus as Core;
        match status {
            Core::Retained => Self::Retained,
            Core::NotCaptured => Self::NotCaptured,
            Core::Redacted => Self::Redacted,
            Core::Expired => Self::Expired,
        }
    }
}

/// The resolver state behind the `ModelRequest` type.
pub(crate) struct ModelRequest {
    /// What the journal recorded about the request.
    pub(crate) record: leviath_core::run_archive::ModelInput,
}

/// What one attempt sent the model, and what the request was assembled from.
///
/// The body itself is here only for a run whose operator asked for it, because a
/// captured request is the whole prompt: whatever the run's context held at that
/// moment, including file contents a tool read and anything somebody pasted.
/// Turn it on with `[observability] capture_model_input` for a machine, or
/// `captureModelInput` on one `spawnRun`.
///
/// Everything beside the body is recorded whether capture is on or off, and
/// answers what a digest cannot: which parameters were really in force after
/// resolution, which tools the model was offered, and which build of the
/// assembly produced the shape.
#[mirror]
#[Object]
impl ModelRequest {
    /// Whether `request` is here, and where it went if it is not.
    async fn capture_status(&self) -> CaptureStatus {
        CaptureStatus::from(self.record.capture_status)
    }

    /// The assembled request, exactly as the provider adapter received it.
    ///
    /// Null unless `captureStatus` is `RETAINED`. This is Leviath's own request
    /// shape rather than one vendor's wire body: the adapter turns it into the
    /// vendor's JSON and never hands that back, so serving the vendor shape
    /// would mean rebuilding it, and a rebuilt prompt is not the request that
    /// was sent.
    async fn request(&self) -> Option<Json> {
        self.record.request.clone().map(Json)
    }

    /// Bytes the captured body took, so the cost of capture is readable even
    /// from a record whose body has since been removed. Zero where no body was
    /// ever taken.
    async fn bytes(&self) -> BigInt {
        BigInt(i64::try_from(self.record.bytes).unwrap_or(i64::MAX))
    }

    /// An opaque fingerprint of the context window this request was assembled
    /// from, so an attempt joins to the window it came from. Compare it between
    /// attempts; nothing else is promised about the value.
    ///
    /// Empty where no body was taken. Computing it walks the whole window, which
    /// is a cost a run nobody asked to capture does not pay.
    async fn source_context_digest(&self) -> &str {
        &self.record.source_context_digest
    }

    /// The parameters the request really carried, after every override and
    /// clamp: the sampling temperature, the completion budget the window left
    /// room for, and any provider-specific keys. A stage's *declared*
    /// parameters are on its blueprint and can differ from all of these.
    async fn parameters(&self) -> ModelParameters {
        // The blueprint reader's own type, over the same vocabulary: a
        // declared cap and an effective one should not need two shapes.
        let table: std::collections::HashMap<String, serde_json::Value> = self
            .record
            .parameters
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        ModelParameters::from_table(&table)
    }

    /// An identifier for the tool set this attempt offered the model. Two
    /// attempts offering the same tools in the same order share it; nothing else
    /// is promised about the value.
    async fn tool_catalog_version(&self) -> &str {
        &self.record.tool_catalog_version
    }

    /// The version of the prompt-assembly logic that produced the request, so a
    /// captured body stays interpretable once assembly changes. It moves when
    /// what a request means changes, not when a field is added.
    async fn assembly_version(&self) -> &str {
        &self.record.assembly_version
    }
}

/// One provider judged unusable, and the model tried in its place.
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct InferenceFailover {
    /// The stage whose call moved.
    pub(crate) stage: String,
    /// The stage-local iteration, which the move leaves alone: the run still
    /// has not had a turn.
    pub(crate) iteration: i32,
    /// The provider that would not serve.
    pub(crate) from_provider: String,
    /// The model it was asked for.
    pub(crate) from_model: String,
    /// The provider tried instead.
    pub(crate) to_provider: String,
    /// The model asked of it.
    pub(crate) to_model: String,
    /// Why the first provider was judged unusable.
    pub(crate) reason: String,
    /// A stable label for the failure itself, the same vocabulary the attempt's
    /// `failureKind` uses. Null when the error carried no classification.
    pub(crate) failure_kind: Option<String>,
    /// When the move was made.
    pub(crate) at: Timestamp,
}

impl From<&FailoverRecord> for InferenceFailover {
    fn from(record: &FailoverRecord) -> Self {
        Self {
            stage: record.stage.clone(),
            iteration: count(record.iteration),
            from_provider: record.from_provider.clone(),
            from_model: record.from_model.clone(),
            to_provider: record.to_provider.clone(),
            to_model: record.to_model.clone(),
            reason: record.reason.clone(),
            failure_kind: label(&record.kind),
            at: Timestamp(record.at),
        }
    }
}

/// The resolver state behind the `InferenceAttempt` type.
pub(crate) struct InferenceAttempt {
    /// What the journal recorded about the attempt.
    pub(crate) record: AttemptRecord,
    /// The move to another provider recorded after it, if any.
    pub(crate) failover: Option<FailoverRecord>,
}

/// One trip a run made to a provider, as the journal recorded it.
///
/// Every trip is here, not just the ones that worked: a call answered on the
/// third try leaves three attempts, and reading them in order is how a run's
/// latency, backoff and moves between providers become visible. An attempt is
/// a record of what happened, so nothing about it changes after the fact.
#[mirror]
#[Object]
impl InferenceAttempt {
    /// The stage the run was in. Empty for a lane that has no stage of its own,
    /// such as the pass that titles a run.
    async fn stage(&self) -> &str {
        &self.record.stage
    }

    /// Which trip to the provider this was, from 1, counting every trip. A
    /// retry that spends none of the retry budget still gets its own number, so
    /// that two attempts at one call can always be told apart.
    async fn attempt(&self) -> i32 {
        i32::try_from(self.record.attempt).unwrap_or(i32::MAX)
    }

    /// The provider that was called, named as the run's configuration names it
    /// rather than as the provider names itself, so this joins to the run's
    /// spend.
    async fn provider(&self) -> &str {
        &self.record.provider
    }

    /// The model it was asked for, likewise as configured.
    async fn model(&self) -> &str {
        &self.record.model
    }

    /// How the attempt ended.
    async fn outcome(&self) -> AttemptOutcome {
        AttemptOutcome::from(&self.record.outcome)
    }

    /// How long this attempt itself took, in milliseconds, not counting the wait
    /// before it.
    async fn duration_ms(&self) -> BigInt {
        BigInt(self.record.duration_ms as i64)
    }

    /// How long the loop slept before making this attempt, in milliseconds.
    /// Zero for the first attempt at a call, and for a retry taken at once.
    async fn backoff_ms(&self) -> BigInt {
        BigInt(self.record.backoff_ms as i64)
    }

    /// What went out.
    async fn digest(&self) -> RequestDigest {
        RequestDigest::from(&self.record.digest)
    }

    /// What this attempt sent, and what the request was assembled from.
    ///
    /// Null for an attempt whose journal holds no record of one. Where it is
    /// set, `captureStatus` says whether the request body itself is there:
    /// capture is off unless an operator asked for it, and everything beside the
    /// body is recorded either way.
    async fn model_input(&self) -> Option<ModelRequest> {
        self.record
            .model_input
            .clone()
            .map(|record| ModelRequest { record })
    }

    /// When the attempt finished.
    async fn at(&self) -> Timestamp {
        Timestamp(self.record.at)
    }

    /// The move to a different provider that followed this attempt.
    ///
    /// Null for every attempt the stage did not give up on, which is most of
    /// them: a retry against the same provider is the next attempt, not a move.
    /// Where this is set, the attempt after it went to `toProvider` and
    /// `toModel`.
    async fn failover(&self) -> Option<InferenceFailover> {
        self.failover.as_ref().map(InferenceFailover::from)
    }
}

impl Paged for InferenceAttempt {
    const NAME: &'static str = "InferenceAttempt";
}

position_order!(
    InferenceAttemptOrder,
    InferenceAttemptOrderField,
    Sequence,
    "The one sort key `inferences` may be ordered by.",
    "Where this attempt sits among the run's own, in the order it was made."
);

/// A stable failure label, or nothing where the error carried none.
///
/// The journal records an unclassified failure as an empty label. Null says the
/// same thing without a client having to know that.
fn label(kind: &str) -> Option<String> {
    (!kind.is_empty()).then(|| kind.to_string())
}

/// Narrow a journal counter to the 32 bits GraphQL's `Int` carries.
///
/// Message counts, tool counts and iterations, none of which a run reaches the
/// thousands of. Saturating rather than wrapping: an implausible ceiling reads
/// as wrong, where a wrapped small number reads as fine.
fn count(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// Read one page of a run's provider attempts.
///
/// Shared by the field on a run and by anything else that grows one later, the
/// same way `interactions` is.
///
/// The whole journal is already read to answer this, so a file-backed filter
/// is confirmed across every attempt once, up front, the same way `executions`
/// does - though today no field on `InferenceAttempt` reads a second file.
pub(crate) async fn inferences(
    run_id: String,
    filter: Option<InferenceAttemptFilter>,
    order_by: Option<Vec<InferenceAttemptOrder>>,
    first: i32,
    after: Option<Cursor>,
) -> async_graphql::Result<Connection<InferenceAttempt>> {
    let limit = page(
        first,
        inferences::INFERENCES_MAX_LIMIT,
        "the inferences page cap",
    )
    .gql()?;
    let filter = filter.unwrap_or_default();
    let rendered = super::super::paging::digest::canonical(&filter).gql()?;
    let digest = cursor::filter_digest(&["inferences", &run_id, rendered.as_str()]);
    let descending = order_by
        .unwrap_or_default()
        .first()
        .is_some_and(|term| term.direction.descending());

    let for_read = run_id.clone();
    let attempts = blocking(move || inferences::read(&for_read)).await.gql()?;
    let items: Vec<InferenceAttempt> = attempts
        .into_iter()
        .map(|attempt| InferenceAttempt {
            record: attempt.record,
            failover: attempt.failover,
        })
        .collect();
    let cx = MatchCx::at(leviath_core::duration::now_secs());
    let walked = position_page(
        items,
        &filter,
        &cx,
        PositionQuery {
            digest: &digest,
            after: after.as_ref().map(|token| token.0.as_str()),
            descending,
            limit,
        },
    )
    .await
    .gql()?;
    Ok(Connection::plain(
        walked.items,
        walked.cursor,
        Total::known(walked.total),
    ))
}

#[cfg(test)]
#[path = "inference_tests.rs"]
mod tests;
