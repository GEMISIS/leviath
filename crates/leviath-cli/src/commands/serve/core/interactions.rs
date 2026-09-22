//! What a run asked a person, read whole.
//!
//! The journal is the only record of this: the hub hands an answer to the
//! caller that was waiting and forgets it, so without the journal an approved
//! tool call is indistinguishable from one no policy ever stopped, and a run
//! that paused for somebody looks exactly like one that never asked. This reads
//! it back through [`leviath_core::run_archive::fold`], because an interaction
//! carries none of the payload weight an execution's result can (a prompt and
//! an answer, never a file), so folding the whole journal to list them costs
//! nothing extra beyond what the run's own state already pays to load.

use leviath_core::run_archive::InteractionRecord;

use super::error::ServeError;
use crate::runstate;

/// Largest page of interactions the GraphQL listing takes.
///
/// The same cap as the run listing and the executions listing: an interaction
/// is a handful of fields, none of them a file body.
pub(crate) const INTERACTIONS_MAX_LIMIT: usize = 200;

/// Every question a run's journal records it asking, in the order it asked
/// them.
///
/// A run with no journal is not an error here: a run that never asked anybody
/// anything did nothing of the kind, and an empty list says so. A journal with
/// no header yet (a run created a moment ago, before its first record landed)
/// reads the same way, for the same reason.
pub(crate) fn read(run_id: &str) -> Result<Vec<InteractionRecord>, ServeError> {
    let path = runstate::run_dir(run_id).join(leviath_core::files::ARCHIVE_FILE);
    let Ok(file) = std::fs::File::open(&path) else {
        return Ok(Vec::new());
    };
    let mut reader = std::io::BufReader::new(file);
    let (_version, records) = leviath_core::run_archive::read_archive_lenient(&mut reader)
        .map_err(|e| {
            ServeError::Internal(format!("Run '{run_id}' has an unreadable journal: {e}"))
        })?;
    Ok(leviath_core::run_archive::fold(&records)
        .map(|folded| folded.interactions)
        .unwrap_or_default())
}

#[cfg(test)]
#[path = "interactions_tests.rs"]
mod tests;
