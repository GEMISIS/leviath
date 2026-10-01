//! What a run's provider calls actually took, read whole.
//!
//! The usage records say what the calls that worked cost, which is the right
//! shape for an invoice and the wrong shape for a post-mortem: a call that
//! moved to another provider leaves nothing behind in the bill. This reads the
//! other half back from the run file: one entry per model call it records,
//! with the move to another model that followed it where there was one.
//!
//! A run file keeps the calls that answered, and a move between models as an
//! event of its own. A call that failed is kept only as a line in the run's
//! log, so the one listed for it is the call a move to another model followed.
//! The request's digest and timings are not kept either, and read as zero.

use std::ops::ControlFlow;

use leviath_core::run_archive::{
    AttemptOutcome, AttemptRecord, FailoverRecord, RequestDigest, Retry,
};
use leviath_runtime::spec::names::ModelRef;
use leviath_runtime::state::RunEvent;

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

/// Every model call a run's file records, in the order it made them, each
/// carrying the move that followed it.
///
/// A run with no run file is not an error here: a run that never called a
/// provider made no trips, and an empty list says so.
pub(crate) fn read(run_id: &str) -> Result<Vec<Attempt>, ServeError> {
    let Some(reader) = run_file::open(run_id)? else {
        return Ok(Vec::new());
    };
    let mut attempts: Vec<Attempt> = Vec::new();
    run_file::walk(run_id, &reader, &mut |step| {
        let stage = step.cursor.stage.to_string();
        for event in &step.delta.events {
            match event {
                RunEvent::Inference {
                    attempt,
                    model,
                    finish_reason,
                    ..
                } => {
                    let (provider, model) = names(model);
                    attempts.push(Attempt {
                        record: AttemptRecord {
                            id: attempt.clone(),
                            stage: stage.clone(),
                            attempt: 1,
                            provider,
                            model,
                            outcome: AttemptOutcome::Succeeded,
                            finish_reason: finish_reason.clone().unwrap_or_default(),
                            stopped_for: None,
                            duration_ms: 0,
                            backoff_ms: 0,
                            digest: no_digest(),
                            model_input: None,
                            at: step.delta.at,
                        },
                        failover: None,
                    });
                }
                RunEvent::Failover { from, to, reason } => {
                    let (from_provider, from_model) = names(from);
                    let (to_provider, to_model) = names(to);
                    // A move follows a call that failed on the model it left,
                    // and the run file keeps that call only as a log line, so
                    // the move is listed as that failed call.
                    attempts.push(Attempt {
                        record: AttemptRecord {
                            id: String::new(),
                            stage: stage.clone(),
                            attempt: 1,
                            provider: from_provider.clone(),
                            model: from_model.clone(),
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
                            at: step.delta.at,
                        },
                        failover: Some(FailoverRecord {
                            stage: stage.clone(),
                            iteration: step.cursor.iteration as usize,
                            from_provider,
                            from_model,
                            to_provider,
                            to_model,
                            reason: reason.clone(),
                            kind: String::new(),
                            at: step.delta.at,
                        }),
                    });
                }
                _ => {}
            }
        }
        ControlFlow::Continue(())
    })?;
    Ok(attempts)
}

#[cfg(test)]
#[path = "inferences_tests.rs"]
mod tests;
