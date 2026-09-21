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

use super::super::error::IntoGraphql;
use super::super::scalars::{BigInt, Cursor, Timestamp};
use crate::commands::serve::blocking::blocking;
use crate::commands::serve::core::inferences;

/// What the retry loop did after an attempt failed.
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

/// One provider judged unusable, and the model tried in its place.
#[derive(Debug, SimpleObject)]
pub(crate) struct InferenceFailover {
    /// The stage whose call moved.
    pub(crate) stage: String,
    /// The stage-local iteration, which the move leaves alone: the agent still
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

/// One trip a run made to a provider, as the journal recorded it.
pub(crate) struct InferenceAttempt {
    /// What the journal recorded about the attempt.
    pub(crate) record: AttemptRecord,
    /// The move to another provider recorded after it, if any.
    pub(crate) failover: Option<FailoverRecord>,
}

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

/// One page of a run's provider attempts.
#[derive(SimpleObject)]
pub(crate) struct InferenceAttemptConnection {
    /// The attempts on this page, in the order they were made.
    pub(crate) edges: Vec<InferenceAttemptEdge>,
    /// Where the next page starts.
    pub(crate) page_info: super::super::connection::PageInfo,
    /// How many the run's journal holds altogether.
    pub(crate) total: i32,
}

/// One attempt and its cursor.
#[derive(SimpleObject)]
pub(crate) struct InferenceAttemptEdge {
    /// Where this attempt sits among the run's own.
    pub(crate) cursor: Cursor,
    /// The attempt.
    pub(crate) node: InferenceAttempt,
}

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
/// same way `interactions::page` is.
pub(crate) async fn page(
    run_id: String,
    first: i32,
    after: Option<Cursor>,
) -> async_graphql::Result<InferenceAttemptConnection> {
    use crate::commands::serve::core::error::ServeError;
    let limit = usize::try_from(first)
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| ServeError::BadRequest("`first` must be at least 1".to_string()))
        .gql()?;
    if limit > inferences::INFERENCES_MAX_LIMIT {
        return Err(ServeError::BadRequest(format!(
            "`first` may be at most {}, the inferences page cap",
            inferences::INFERENCES_MAX_LIMIT
        )))
        .gql();
    }
    let cursor = after.map(|cursor| cursor.0);
    let for_read = run_id.clone();
    let page = blocking(move || {
        let spec = inferences::InferencesSpec::resolve(&for_read, Some(limit), cursor.as_deref())?;
        inferences::page(&for_read, &spec)
    })
    .await
    .gql()?;
    let total = i32::try_from(page.total).unwrap_or(i32::MAX);
    let end_cursor = page.next_cursor.clone().map(Cursor);
    Ok(InferenceAttemptConnection {
        edges: page
            .attempts
            .into_iter()
            .map(|indexed| InferenceAttemptEdge {
                // The index among the run's own attempts: stable for as long as
                // the run exists, and the only handle one needs since nothing
                // about an attempt is ever fetched separately.
                cursor: Cursor(indexed.index.to_string()),
                node: InferenceAttempt {
                    record: indexed.attempt.record,
                    failover: indexed.attempt.failover,
                },
            })
            .collect(),
        page_info: super::super::connection::PageInfo {
            end_cursor: end_cursor.clone(),
            has_next_page: end_cursor.is_some(),
        },
        total,
    })
}

#[cfg(test)]
#[path = "inference_tests.rs"]
mod tests;
