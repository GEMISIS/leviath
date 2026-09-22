//! Why a run's context window changed, read whole.
//!
//! The snapshots say what every region held at each point; they cannot say what
//! moved it, and a region that lost the plan it was holding looks identical
//! whether a compaction summarised it away, a stage-edge transform cleared it,
//! or the model called `context_delete` on it. This reads the other half back
//! through [`leviath_core::run_archive::fold`], which is cheap for the same
//! reason the interactions listing is: a change record is a region name, a
//! cause and three numbers, never the text the change moved.

use leviath_core::run_archive::IndexedChange;

use super::error::ServeError;
use crate::runstate;

/// Largest page of changes the GraphQL listing takes.
///
/// The same cap as the run listing and the interactions listing. It is well
/// above the history listing's, because a change carries no window: a page of
/// these is six small fields apiece.
pub(crate) const CONTEXT_CHANGES_MAX_LIMIT: usize = 200;

/// Every change one execution committed, in the order they landed.
///
/// Empty for an execution that committed none, which most are, and for an empty
/// id: an id that names nothing matches nothing rather than matching every change
/// that recorded no execution.
pub(crate) fn by_execution(
    run_id: &str,
    execution_id: &str,
) -> Result<Vec<IndexedChange>, ServeError> {
    if execution_id.is_empty() {
        return Ok(Vec::new());
    }
    Ok(read(run_id)?
        .into_iter()
        .filter(|change| change.record.execution_id.as_deref() == Some(execution_id))
        .collect())
}

/// Every change a run's journal records, in the order they landed.
///
/// Streamed one frame at a time rather than folded, for two reasons: a fold
/// materializes the whole parsed journal to answer a question about a handful of
/// small records, and the position of each record is what a client needs to name
/// one - which a fold does not carry.
///
/// A run with no journal is not an error here, and neither is a journal that
/// names no causes: a run whose writes all went through paths that cannot name
/// one has nothing to report, and an empty list says so.
pub(crate) fn read(run_id: &str) -> Result<Vec<IndexedChange>, ServeError> {
    let path = runstate::run_dir(run_id).join(leviath_core::files::ARCHIVE_FILE);
    let Ok(file) = std::fs::File::open(&path) else {
        return Ok(Vec::new());
    };
    let mut reader = std::io::BufReader::new(file);
    leviath_core::run_archive::read_archive_changes(&mut reader)
        .map_err(|e| ServeError::Internal(format!("Run '{run_id}' has an unreadable journal: {e}")))
}

#[cfg(test)]
#[path = "context_changes_tests.rs"]
mod tests;
