//! What a run's provider calls actually took, read whole.
//!
//! The usage records say what the calls that worked cost, which is the right
//! shape for an invoice and the wrong shape for a post-mortem: a call that
//! moved to another provider leaves nothing behind in the bill. This reads the
//! other half back from the run file: one entry per model call it records,
//! with the move to another model that followed it where there was one.
//!
//! A run file keeps every model call whole, as an attempt event: how it
//! ended, how long it took and waited, the digest of what was sent, and the
//! request itself when it was captured. A move to another model follows the
//! failed call it gave up on. A file written without attempt events (one
//! converted from an older layout) is read from its answered calls and its
//! moves alone, and the digest and timings it never kept read as zero.

use std::ops::ControlFlow;

use leviath_core::run_archive::{
    AttemptOutcome, AttemptRecord, CaptureStatus, FailoverRecord, ModelInput, RequestDigest, Retry,
};
use leviath_runtime::spec::names::ModelRef;
use leviath_runtime::state::RunEvent;
use leviath_runtime::state::journal::{
    AttemptOutcomeState, AttemptState, CaptureState, ModelInputState, RetryState,
};

use super::error::ServeError;
use super::run_file;

/// Largest page of attempts the GraphQL listing takes.
///
/// The same cap as the run listing and the interactions listing: an attempt is
/// a handful of fields plus a fixed-size digest, never a request body.
pub(crate) const INFERENCES_MAX_LIMIT: usize = 200;

/// One trip to a provider, with the move that followed it.
#[derive(Debug)]
pub(crate) struct Attempt {
    /// What the run file recorded about the attempt itself.
    pub(crate) record: AttemptRecord,
    /// The move to another provider recorded after it. Nothing for an attempt
    /// the stage did not give up on.
    pub(crate) failover: Option<FailoverRecord>,
}

/// One attempt of a run's, by the id it was minted under.
///
/// `None` when the run's file holds no attempt under that id, which is what an
/// id from another run looks like. An empty id matches nothing rather than
/// matching the calls recorded without one.
pub(crate) fn attempt(run_id: &str, attempt_id: &str) -> Result<Option<Attempt>, ServeError> {
    if attempt_id.is_empty() {
        return Ok(None);
    }
    Ok(read(run_id)?
        .into_iter()
        .find(|held| held.record.id == attempt_id))
}

/// The digest of a request the run file did not keep.
fn no_digest() -> RequestDigest {
    RequestDigest {
        system_hash: 0,
        messages: 0,
        tools: 0,
        max_tokens: 0,
        temperature: 0.0,
    }
}

/// A model reference's provider and model, as the records spell them.
fn names(model: &ModelRef) -> (String, String) {
    (
        model
            .provider
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default(),
        model.model.to_string(),
    )
}

/// A model call the run file kept whole, as the journal's record of it.
fn kept(a: &AttemptState, stage: &str, at: i64) -> AttemptRecord {
    AttemptRecord {
        id: a.id.clone(),
        stage: stage.to_string(),
        attempt: a.number,
        provider: a.provider.clone(),
        model: a.model.clone(),
        outcome: match &a.outcome {
            AttemptOutcomeState::Succeeded => AttemptOutcome::Succeeded,
            AttemptOutcomeState::Failed {
                kind,
                transient,
                capacity,
                next,
            } => AttemptOutcome::Failed {
                kind: kind.clone(),
                transient: *transient,
                capacity: *capacity,
                next: match next {
                    RetryState::Reported => Retry::Reported,
                    RetryState::SameModel => Retry::SameModel,
                    RetryState::RenewedFiles => Retry::RenewedFiles,
                },
            },
        },
        finish_reason: a.finish_reason.clone().unwrap_or_default(),
        stopped_for: a.stopped_for.clone(),
        duration_ms: a.duration_ms,
        backoff_ms: a.backoff_ms,
        digest: RequestDigest {
            system_hash: a.digest.system_hash,
            messages: a.digest.messages as usize,
            tools: a.digest.tools as usize,
            max_tokens: a.digest.max_tokens as usize,
            temperature: a.digest.temperature,
        },
        model_input: a.model_input.as_ref().map(input),
        at,
    }
}

/// A captured request, as the journal's record of it.
fn input(m: &ModelInputState) -> ModelInput {
    ModelInput {
        capture_status: match m.capture {
            CaptureState::Retained => CaptureStatus::Retained,
            CaptureState::NotCaptured => CaptureStatus::NotCaptured,
            CaptureState::Redacted => CaptureStatus::Redacted,
            CaptureState::Expired => CaptureStatus::Expired,
        },
        request: m.request.as_ref().map(|doc| doc.value().clone()),
        bytes: m.bytes,
        source_context_digest: m.source_context_digest.clone(),
        parameters: m
            .parameters
            .iter()
            .map(|(k, v)| (k.clone(), v.value().clone()))
            .collect(),
        tool_catalog_version: m.tool_catalog_version.clone(),
        assembly_version: m.assembly_version.clone(),
    }
}

/// A call the file kept as its answer alone, as the journal's record of it.
fn answered(
    attempt: &str,
    model: &ModelRef,
    finish: &Option<String>,
    stage: &str,
    at: i64,
) -> AttemptRecord {
    let (provider, model) = names(model);
    AttemptRecord {
        id: attempt.to_string(),
        stage: stage.to_string(),
        attempt: 1,
        provider,
        model,
        outcome: AttemptOutcome::Succeeded,
        finish_reason: finish.clone().unwrap_or_default(),
        stopped_for: None,
        duration_ms: 0,
        backoff_ms: 0,
        digest: no_digest(),
        model_input: None,
        at,
    }
}

/// Put a move to another model on the failed call it gave up on, the last one
/// listed. A list holding no such call gets the move alone, standing for the
/// call it gave up on.
fn follow(attempts: &mut Vec<Attempt>, mut failover: FailoverRecord) {
    let last = attempts.last_mut().filter(|held| held.failover.is_none());
    let failed = last.as_ref().and_then(|held| match &held.record.outcome {
        AttemptOutcome::Failed { kind, .. } => Some(kind.clone()),
        AttemptOutcome::Succeeded => None,
    });
    match (last, failed) {
        (Some(held), Some(kind)) => {
            failover.kind = kind;
            held.failover = Some(failover);
        }
        _ => attempts.push(Attempt {
            record: AttemptRecord {
                id: String::new(),
                stage: failover.stage.clone(),
                attempt: 1,
                provider: failover.from_provider.clone(),
                model: failover.from_model.clone(),
                outcome: AttemptOutcome::Failed {
                    kind: String::new(),
                    transient: false,
                    capacity: false,
                    next: Retry::Reported,
                },
                finish_reason: String::new(),
                stopped_for: None,
                duration_ms: 0,
                backoff_ms: 0,
                digest: no_digest(),
                model_input: None,
                at: failover.at,
            },
            failover: Some(failover),
        }),
    }
}

/// Every model call a run's file records, in the order it made them, each
/// carrying the move that followed it.
///
/// A run with no run file is not an error here: a run that never called a
/// provider made no trips, and an empty list says so.
pub(crate) fn read(run_id: &str) -> Result<Vec<Attempt>, ServeError> {
    let Some(reader) = run_file::open(run_id)? else {
        return Ok(Vec::new());
    };
    // Both readings are taken in one pass: the calls kept whole, and the calls
    // read from their answers alone, for a file that kept none whole.
    let mut whole: Vec<Attempt> = Vec::new();
    let mut answers: Vec<Attempt> = Vec::new();
    let mut kept_whole = false;
    run_file::walk(run_id, &reader, &mut |step| {
        let stage = step.cursor.stage.to_string();
        let at = step.delta.at;
        for event in &step.delta.events {
            match event {
                RunEvent::Attempt(a) => {
                    kept_whole = true;
                    whole.push(Attempt {
                        record: kept(a, &stage, at),
                        failover: None,
                    });
                }
                RunEvent::Inference {
                    attempt,
                    model,
                    finish_reason,
                    ..
                } => answers.push(Attempt {
                    record: answered(attempt, model, finish_reason, &stage, at),
                    failover: None,
                }),
                RunEvent::Failover { from, to, reason } => {
                    let (from_provider, from_model) = names(from);
                    let (to_provider, to_model) = names(to);
                    let failover = FailoverRecord {
                        stage: stage.clone(),
                        iteration: step.cursor.iteration as usize,
                        from_provider,
                        from_model,
                        to_provider,
                        to_model,
                        reason: reason.clone(),
                        kind: String::new(),
                        at,
                    };
                    follow(&mut whole, failover.clone());
                    follow(&mut answers, failover);
                }
                _ => {}
            }
        }
        ControlFlow::Continue(())
    })?;
    Ok(match kept_whole {
        true => whole,
        false => answers,
    })
}

#[cfg(test)]
#[path = "inferences_tests.rs"]
mod tests;
