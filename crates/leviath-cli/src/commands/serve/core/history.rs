//! How a run's context window changed over the run, one page at a time.
//!
//! Each point carries a whole window, so this is paged harder than the run
//! listing is. The journal is walked in one streamed pass rather than read
//! whole: a mature run's journal is tens of megabytes on disk and several times
//! that as parsed structs, and materializing it per request was this API's
//! largest transient allocation.

use std::ops::ControlFlow;

use leviath_core::run_archive::RunPoint;

use super::error::ServeError;
use crate::runstate;

/// Default page size for the history.
pub(crate) const HISTORY_DEFAULT_LIMIT: usize = 50;

/// Largest page of history.
///
/// Lower than the run listing's cap because each item is a whole context
/// window rather than a record.
pub(crate) const HISTORY_MAX_LIMIT: usize = 100;

/// Which page of the history to read.
#[derive(Debug)]
pub(crate) struct HistorySpec {
    /// How many points to return.
    pub(crate) limit: usize,
    /// Chronological, or newest first.
    pub(crate) ascending: bool,
    /// Where the previous page left off, as an index into the journal.
    pub(crate) after: Option<usize>,
    /// The digest the cursor was minted against, for the next one.
    pub(crate) digest: String,
}

impl HistorySpec {
    /// Read the request's own words into a spec, refusing what cannot be
    /// answered.
    ///
    /// The cursor is checked here, against this run and this order, so a token
    /// carried over from a different listing is refused rather than quietly
    /// resuming somewhere else.
    pub(crate) fn resolve(
        run_id: &str,
        limit: Option<usize>,
        order: Option<&str>,
        cursor: Option<&str>,
    ) -> Result<Self, ServeError> {
        let ascending = match order {
            None | Some("asc") => true,
            Some("desc") => false,
            Some(other) => {
                return Err(ServeError::BadRequest(format!(
                    "Unknown order '{other}': expected asc or desc"
                )));
            }
        };
        let limit = match limit {
            None => HISTORY_DEFAULT_LIMIT,
            Some(0) => {
                return Err(ServeError::BadRequest(
                    "`limit` must be at least 1; omit it for the default".to_string(),
                ));
            }
            Some(n) => n.min(HISTORY_MAX_LIMIT),
        };
        let digest = super::super::cursor::filter_digest(&[run_id]);
        let after = match cursor {
            None => None,
            Some(raw) => {
                let decoded =
                    super::super::cursor::decode(raw, "index", order_name(ascending), &digest)
                        .map_err(|e| ServeError::BadRequest(e.message()))?;
                // This listing only ever mints an integer key, so anything else
                // means a cursor that did not come from here.
                match decoded.key {
                    super::super::cursor::CursorKey::Int(i) => usize::try_from(i).ok(),
                    _ => None,
                }
            }
        };
        Ok(Self {
            limit,
            ascending,
            after,
            digest,
        })
    }

    /// Whether this spec is resuming a page rather than starting one.
    fn resuming(&self) -> bool {
        self.after.is_some()
    }
}

/// The word an order goes on the wire as, which the cursor is bound to.
fn order_name(ascending: bool) -> &'static str {
    match ascending {
        true => "asc",
        false => "desc",
    }
}

/// One page of a run's history.
#[derive(Debug)]
pub(crate) struct HistoryPage {
    /// The points themselves, in the order asked for.
    pub(crate) points: Vec<RunPoint>,
    /// Where the next page starts. Nothing when this page reached the end.
    pub(crate) next_cursor: Option<String>,
    /// How many points the journal holds altogether.
    pub(crate) total: usize,
}

/// Read one page of a run's history.
pub(crate) fn page(run_id: &str, spec: &HistorySpec) -> Result<HistoryPage, ServeError> {
    // One streamed pass to count, so `total` is honest and a descending window
    // knows where to start. Counting folds the deltas but materializes nothing.
    let mut total = 0usize;
    let visited = runstate::visit_run_archive(run_id, &mut |_| {
        total += 1;
        ControlFlow::Continue(())
    });
    if visited.is_none() || (total == 0 && !spec.resuming()) {
        return Err(ServeError::NotFound(format!(
            "No context history for run '{run_id}'"
        )));
    }

    // Which indices this page wants, given the direction and where the cursor
    // left off. Computed up front so the replay can skip everything else.
    let wanted: Vec<usize> = match spec.ascending {
        true => {
            let start = spec.after.map(|i| i + 1).unwrap_or(0);
            (start..total).take(spec.limit + 1).collect()
        }
        false => {
            let start = spec
                .after
                .map(|i| i.saturating_sub(1))
                .unwrap_or_else(|| total.saturating_sub(1));
            (0..=start).rev().take(spec.limit + 1).collect()
        }
    };
    let stop_at = wanted.iter().copied().max();

    let mut collected: Vec<(usize, RunPoint)> = Vec::new();
    runstate::visit_run_archive(run_id, &mut |point| {
        if wanted.contains(&point.index) {
            collected.push((
                point.index,
                RunPoint {
                    // Redacted for the same reason `runstate::context_history`
                    // redacts: the journal stores the run's record whole, secret
                    // and all.
                    meta: point.meta.redacted(),
                    context: point.context.clone(),
                    at: point.at,
                },
            ));
        }
        match stop_at {
            Some(last) if point.index >= last => ControlFlow::Break(()),
            _ => ControlFlow::Continue(()),
        }
    });

    if !spec.ascending {
        collected.sort_by_key(|(index, _)| std::cmp::Reverse(*index));
    }

    let has_more = collected.len() > spec.limit;
    collected.truncate(spec.limit);
    let next_cursor = has_more
        .then(|| collected.last())
        .flatten()
        .map(|(index, _)| {
            super::super::cursor::encode(
                "index",
                order_name(spec.ascending),
                &spec.digest,
                super::super::cursor::CursorKey::Int(*index as i64),
                "",
            )
        });

    Ok(HistoryPage {
        points: collected.into_iter().map(|(_, point)| point).collect(),
        next_cursor,
        total,
    })
}
