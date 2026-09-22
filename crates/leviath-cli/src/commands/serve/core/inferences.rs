//! What a run's provider calls actually took, read whole.
//!
//! The usage records say what the calls that worked cost, which is the right
//! shape for an invoice and the wrong shape for a post-mortem: a call refused
//! three times and answered on the fourth is billed once, and a call that moved
//! to another provider leaves nothing behind at all. This reads the other half
//! back: one entry per trip to a provider, with the move that followed it where
//! there was one.
//!
//! Read record by record rather than through
//! [`leviath_core::run_archive::fold`], which is the one place this parts from
//! the interactions listing. The pairing of a failover with the attempt it
//! follows is the journal's own order, and the folded form keeps attempts and
//! failovers in two separate lists: the adjacency this needs is there in the
//! records and gone by the time they are folded. Neither record carries a
//! request or a response body, so reading them costs no more than the run's own
//! state already pays to load.

use leviath_core::run_archive::{AttemptRecord, FailoverRecord, RunRecord};

use super::error::ServeError;
use crate::runstate;

/// Largest page of attempts the GraphQL listing takes.
///
/// The same cap as the run listing and the interactions listing: an attempt is
/// a handful of fields plus a fixed-size digest, never a request body.
pub(crate) const INFERENCES_MAX_LIMIT: usize = 200;

/// One trip to a provider, with the move that followed it.
#[derive(Debug)]
pub(crate) struct Attempt {
    /// What the journal recorded about the attempt itself.
    pub(crate) record: AttemptRecord,
    /// The move to another provider recorded after it. Nothing for an attempt
    /// the stage did not give up on.
    pub(crate) failover: Option<FailoverRecord>,
}

/// One attempt of a run's, by the id it was minted under.
///
/// `None` when the run's journal holds no attempt under that id, which is what an
/// id from another run looks like and what every attempt in a journal written
/// before attempts had identity looks like. An empty id matches nothing rather
/// than matching the unidentified ones.
pub(crate) fn attempt(run_id: &str, attempt_id: &str) -> Result<Option<Attempt>, ServeError> {
    if attempt_id.is_empty() {
        return Ok(None);
    }
    Ok(read(run_id)?
        .into_iter()
        .find(|held| held.record.id == attempt_id))
}

/// Every trip to a provider a run's journal records, in the order it made them,
/// each carrying the move that followed it.
///
/// A run with no journal is not an error here: a run that never called a
/// provider made no trips, and an empty list says so. A journal with no header
/// yet reads the same way, for the same reason.
pub(crate) fn read(run_id: &str) -> Result<Vec<Attempt>, ServeError> {
    let path = runstate::run_dir(run_id).join(leviath_core::files::ARCHIVE_FILE);
    let Ok(file) = std::fs::File::open(&path) else {
        return Ok(Vec::new());
    };
    let mut reader = std::io::BufReader::new(file);
    let (_version, records) = leviath_core::run_archive::read_archive_lenient(&mut reader)
        .map_err(|e| {
            ServeError::Internal(format!("Run '{run_id}' has an unreadable journal: {e}"))
        })?;
    let mut attempts: Vec<Attempt> = Vec::new();
    for record in &records {
        match record {
            RunRecord::InferenceAttempt(attempt) => attempts.push(Attempt {
                record: attempt.clone(),
                failover: None,
            }),
            RunRecord::InferenceFailover(failover) => {
                // Matched on the target the move left rather than on position
                // alone. A lane with no stage of its own journals its attempts
                // into the same file, so the attempt written just before a
                // failover is not always the call that failed over. A move
                // whose attempt is not in the journal at all has nothing to
                // hang on and is left out: this listing is attempts, and an
                // entry naming none of them would be a row about nothing.
                if let Some(attempt) = attempts.iter_mut().rev().find(|held| {
                    (
                        held.record.stage.as_str(),
                        held.record.provider.as_str(),
                        held.record.model.as_str(),
                    ) == (
                        failover.stage.as_str(),
                        failover.from_provider.as_str(),
                        failover.from_model.as_str(),
                    )
                }) {
                    attempt.failover = Some(failover.clone());
                }
            }
            _ => {}
        }
    }
    Ok(attempts)
}

#[cfg(test)]
#[path = "inferences_tests.rs"]
mod tests;
