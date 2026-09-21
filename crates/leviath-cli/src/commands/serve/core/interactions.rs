//! What a run asked a person, one page at a time.
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

/// Default page size.
pub(crate) const INTERACTIONS_DEFAULT_LIMIT: usize = 50;

/// Largest page of interactions.
///
/// The same cap as the run listing and the executions page: an interaction is
/// a handful of fields, none of them a file body.
pub(crate) const INTERACTIONS_MAX_LIMIT: usize = 200;

/// Which page of a run's interactions to read.
#[derive(Debug)]
pub(crate) struct InteractionsSpec {
    /// How many to return.
    pub(crate) limit: usize,
    /// How many to skip, from the previous page's cursor.
    pub(crate) after: Option<usize>,
    /// The digest the cursor was minted against.
    pub(crate) digest: String,
}

impl InteractionsSpec {
    /// Read the request's own words into a spec, refusing what cannot be
    /// answered.
    pub(crate) fn resolve(
        run_id: &str,
        limit: Option<usize>,
        cursor: Option<&str>,
    ) -> Result<Self, ServeError> {
        let limit = match limit {
            None => INTERACTIONS_DEFAULT_LIMIT,
            Some(0) => {
                return Err(ServeError::BadRequest(
                    "`limit` must be at least 1; omit it for the default".to_string(),
                ));
            }
            Some(n) => n.min(INTERACTIONS_MAX_LIMIT),
        };
        // A digest of its own, distinct from the executions listing's: a
        // cursor minted for one must not be accepted by the other, even though
        // both key on nothing but the run id today.
        let digest = super::super::cursor::filter_digest(&["interactions", run_id]);
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

/// One interaction, with the position it holds among the run's own.
///
/// The position is what a page cursor names. An [`InteractionRecord`] carries
/// no journal offset of its own the way an execution does, because nothing
/// about it is ever fetched separately: everything worth reading is already on
/// the record.
#[derive(Debug)]
pub(crate) struct IndexedInteraction {
    /// Where this interaction sits among the run's interactions, in the order
    /// it asked them.
    pub(crate) index: usize,
    /// What the journal recorded.
    pub(crate) record: InteractionRecord,
}

/// One page of a run's interactions.
#[derive(Debug)]
pub(crate) struct InteractionsPage {
    /// The interactions themselves, in the order the run asked them.
    pub(crate) interactions: Vec<IndexedInteraction>,
    /// Where the next page starts. Nothing when this page reached the end.
    pub(crate) next_cursor: Option<String>,
    /// How many the journal holds altogether.
    pub(crate) total: usize,
}

/// Read one page of what a run asked a person.
///
/// Asked order, always: the order the run put the questions in, which is also
/// the order a person answering a backlog of them would want to see.
pub(crate) fn page(run_id: &str, spec: &InteractionsSpec) -> Result<InteractionsPage, ServeError> {
    let interactions = read(run_id)?;
    let total = interactions.len();
    let start = spec.after.map(|i| i + 1).unwrap_or(0);
    let mut wanted: Vec<IndexedInteraction> = interactions
        .into_iter()
        .enumerate()
        .skip(start)
        .take(spec.limit + 1)
        .map(|(index, record)| IndexedInteraction { index, record })
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
    Ok(InteractionsPage {
        interactions: wanted,
        next_cursor,
        total,
    })
}

/// Every question a run's journal records it asking, in the order it asked
/// them.
///
/// A run with no journal is not an error here: a run that never asked anybody
/// anything did nothing of the kind, and an empty list says so. A journal with
/// no header yet (a run created a moment ago, before its first record landed)
/// reads the same way, for the same reason.
fn read(run_id: &str) -> Result<Vec<InteractionRecord>, ServeError> {
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
