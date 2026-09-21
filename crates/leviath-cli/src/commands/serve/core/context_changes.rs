//! Why a run's context window changed, one page at a time.
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

/// Default page size.
pub(crate) const CONTEXT_CHANGES_DEFAULT_LIMIT: usize = 50;

/// Largest page of changes.
///
/// The same cap as the run listing and the interactions page. It is well above
/// the history page's, because a change carries no window: a page of these is
/// six small fields apiece.
pub(crate) const CONTEXT_CHANGES_MAX_LIMIT: usize = 200;

/// Which page of a run's context changes to read.
#[derive(Debug)]
pub(crate) struct ContextChangesSpec {
    /// How many to return.
    pub(crate) limit: usize,
    /// How many to skip, from the previous page's cursor.
    pub(crate) after: Option<usize>,
    /// The digest the cursor was minted against.
    pub(crate) digest: String,
}

impl ContextChangesSpec {
    /// Read the request's own words into a spec, refusing what cannot be
    /// answered.
    pub(crate) fn resolve(
        run_id: &str,
        limit: Option<usize>,
        cursor: Option<&str>,
    ) -> Result<Self, ServeError> {
        let limit = match limit {
            None => CONTEXT_CHANGES_DEFAULT_LIMIT,
            Some(0) => {
                return Err(ServeError::BadRequest(
                    "`limit` must be at least 1; omit it for the default".to_string(),
                ));
            }
            Some(n) => n.min(CONTEXT_CHANGES_MAX_LIMIT),
        };
        // A digest of its own, so a cursor minted for the window history or for
        // any other of this run's listings is refused rather than followed into
        // a different sequence.
        let digest = super::super::cursor::filter_digest(&["context_changes", run_id]);
        let after = match cursor {
            None => None,
            Some(raw) => {
                let decoded = super::super::cursor::decode(raw, "index", "asc", &digest)
                    .map_err(|e| ServeError::BadRequest(e.message()))?;
                match decoded.key {
                    super::super::cursor::CursorKey::Int(i) => usize::try_from(i).ok(),
                    // This listing mints only an integer key, so anything else
                    // is a cursor from somewhere else.
                    _ => None,
                }
            }
        };
        Ok(Self {
            limit,
            after,
            digest,
        })
    }
}

/// One change, with the position it holds among the run's own.
///
/// The index is what a page cursor names. A timestamp would not do: several
/// changes can land on one tick, and a cursor that could not tell them apart
/// would either repeat a change or skip one.
#[derive(Debug)]
pub(crate) struct IndexedContextChange {
    /// Where this change sits among the run's changes, in the order they
    /// landed.
    pub(crate) index: usize,
    /// What the journal recorded, and where the record that carries it sits.
    pub(crate) change: IndexedChange,
}

/// One page of a run's context changes.
#[derive(Debug)]
pub(crate) struct ContextChangesPage {
    /// The changes themselves, in the order they landed.
    pub(crate) changes: Vec<IndexedContextChange>,
    /// Where the next page starts. Nothing when this page reached the end.
    pub(crate) next_cursor: Option<String>,
    /// How many the journal holds altogether.
    pub(crate) total: usize,
}

/// Read one page of why a run's regions changed.
///
/// Recorded order, always: the order the changes landed, which is the order a
/// region's story reads in.
pub(crate) fn page(
    run_id: &str,
    spec: &ContextChangesSpec,
) -> Result<ContextChangesPage, ServeError> {
    let changes = read(run_id)?;
    let total = changes.len();
    let start = spec.after.map(|i| i + 1).unwrap_or(0);
    let mut wanted: Vec<IndexedContextChange> = changes
        .into_iter()
        .enumerate()
        .skip(start)
        .take(spec.limit + 1)
        .map(|(index, change)| IndexedContextChange { index, change })
        .collect();
    let has_more = wanted.len() > spec.limit;
    wanted.truncate(spec.limit);
    let next_cursor = has_more.then(|| wanted.last()).flatten().map(|last| {
        super::super::cursor::encode(
            "index",
            "asc",
            &spec.digest,
            super::super::cursor::CursorKey::Int(last.index as i64),
            "",
        )
    });
    Ok(ContextChangesPage {
        changes: wanted,
        next_cursor,
        total,
    })
}

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
fn read(run_id: &str) -> Result<Vec<IndexedChange>, ServeError> {
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
