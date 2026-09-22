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

/// The point at which this run held the window named by `revision`.
///
/// Immutable by construction, and that is the property the whole debugger rests
/// on. A revision is derived from a window's contents, and the journal it is
/// looked up in is append-only, so a revision resolves to the content it was
/// minted from and to nothing else: no later write can change what it means, and
/// a read of one can never come back with what the run holds now. A run that never
/// held that window answers `None` rather than answering with something near it.
///
/// The earliest point holding the content, where a run held it more than once.
/// The content is the same either way - that is what content addressing means -
/// and the first time it appeared is the answer to "where did this come from".
///
/// Streamed, so the whole journal is never materialized, and stopped at the
/// match.
pub(crate) fn at_revision(run_id: &str, revision: &str) -> Option<RunPoint> {
    let mut found = None;
    runstate::visit_run_archive(run_id, &mut |point| {
        if leviath_core::run_meta::revision::context_revision(point.context) != revision {
            return ControlFlow::Continue(());
        }
        found = Some(RunPoint {
            // Redacted for the same reason the paged read redacts: the journal
            // stores the run's record whole, secret and all.
            meta: point.meta.redacted(),
            context: point.context.clone(),
            at: point.at,
        });
        ControlFlow::Break(())
    })?;
    found
}

/// What a read of one point's context window is reported to.
///
/// A window is the largest thing this API materializes, and the promise of
/// every listing over them is that a page of two reads two of them. That is
/// only observable as work that did not happen, so the reads report here and a
/// test counts them.
pub(crate) type WindowReadRecorder = Box<dyn Fn(&str) + Send + Sync>;

/// Where a window read is reported, when anything is listening.
///
/// Nothing installs a recorder in a server: the list would grow for ever and
/// nothing but a test has any use for it, so a read costs a load of this and a
/// call it does not make.
static RECORDER: std::sync::OnceLock<WindowReadRecorder> = std::sync::OnceLock::new();

/// Report every window read to `record`, for as long as this process lives.
///
/// Once, deliberately: a second call is a no-op, so each test that wants the
/// log can ask for it rather than arranging to be the one that installs it.
#[cfg(test)]
pub(crate) fn record_window_reads(record: WindowReadRecorder) {
    drop(RECORDER.set(record));
}

/// Note that one point's window is being materialized.
fn noted(run_id: &str) {
    if let Some(record) = RECORDER.get() {
        record(run_id);
    }
}

/// How many points a run's history holds, or nothing where it has no readable
/// archive.
///
/// One streamed pass that folds the deltas and materializes none of them, so a
/// listing knows how far it can walk before it decides what to read.
pub(crate) fn point_count(run_id: &str) -> Option<usize> {
    let mut total = 0usize;
    runstate::visit_run_archive(run_id, &mut |_| {
        total += 1;
        ControlFlow::Continue(())
    })?;
    Some(total)
}

/// The points at `wanted`, oldest first, with their windows.
///
/// The one place a window is materialized for a listing, so a page reads its
/// own items and nothing else. The replay stops at the last index asked for
/// rather than running to the end of the journal.
pub(crate) fn windows_at(run_id: &str, wanted: &[usize]) -> Vec<(usize, RunPoint)> {
    let stop_at = wanted.iter().copied().max();
    let mut collected: Vec<(usize, RunPoint)> = Vec::new();
    runstate::visit_run_archive(run_id, &mut |point| {
        if wanted.contains(&point.index) {
            noted(run_id);
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
    collected
}

/// Every point of a run's history, windows and all.
///
/// What a filter that looks inside a window costs: there is no way to know
/// which points match without reading each one. Everything else pages over
/// [`point_count`] and reads through [`windows_at`].
pub(crate) fn every_window(run_id: &str) -> Vec<RunPoint> {
    let points = runstate::context_history(run_id);
    for _ in &points {
        noted(run_id);
    }
    points
}

/// Read one page of a run's history.
pub(crate) fn page(run_id: &str, spec: &HistorySpec) -> Result<HistoryPage, ServeError> {
    // One streamed pass to count, so `total` is honest and a descending window
    // knows where to start. Counting folds the deltas but materializes nothing.
    let counted = point_count(run_id);
    let total = counted.unwrap_or_default();
    if counted.is_none() || (total == 0 && !spec.resuming()) {
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

    let mut collected = windows_at(run_id, &wanted);
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

#[cfg(test)]
mod tests {
    use super::{HISTORY_DEFAULT_LIMIT, HISTORY_MAX_LIMIT, HistorySpec};
    use crate::commands::serve::cursor;

    /// The page size is bounded at both ends, and the order is one of two words.
    #[test]
    fn a_history_request_is_bounded_and_ordered() {
        let spec = HistorySpec::resolve("run-a", None, None, None).expect("the defaults");
        assert_eq!(spec.limit, HISTORY_DEFAULT_LIMIT);
        assert!(spec.ascending, "chronological by default");
        assert!(spec.after.is_none());

        // Clamped rather than refused: this cap protects the server from a page
        // of whole context windows, and a smaller page is still an answer.
        let capped = HistorySpec::resolve("run-a", Some(10_000), None, None).expect("clamped");
        assert_eq!(capped.limit, HISTORY_MAX_LIMIT);

        let refused = HistorySpec::resolve("run-a", Some(0), None, None)
            .expect_err("zero is not a page size");
        assert_eq!(refused.code(), "BAD_USER_INPUT");
        let order = HistorySpec::resolve("run-a", None, Some("sideways"), None)
            .expect_err("a word that is not an order");
        assert!(order.to_string().contains("asc"), "{order}");
        assert!(
            !HistorySpec::resolve("run-a", None, Some("desc"), None)
                .expect("newest first")
                .ascending
        );
    }

    /// A cursor is read for its index, and one carrying anything else is read as
    /// no cursor.
    ///
    /// This listing only ever mints an integer key, so a token with another kind
    /// of key did not come from here. Starting the page from the beginning is the
    /// answer a client can act on; resuming from a key this listing cannot use
    /// would be guessing.
    #[test]
    fn a_cursor_is_read_for_its_index_and_nothing_else() {
        let digest = cursor::filter_digest(&["run-a"]);
        let numbered = cursor::encode("index", "asc", &digest, cursor::CursorKey::Int(7), "");
        let spec = HistorySpec::resolve("run-a", None, None, Some(&numbered)).expect("a cursor");
        assert_eq!(spec.after, Some(7));

        let lettered = cursor::encode(
            "index",
            "asc",
            &digest,
            cursor::CursorKey::Text("seven".to_string()),
            "",
        );
        let spec = HistorySpec::resolve("run-a", None, None, Some(&lettered)).expect("readable");
        assert!(spec.after.is_none(), "a key this listing cannot use");

        // A cursor minted for another run is refused rather than resumed: the
        // digest binds it to the run it was made for.
        let elsewhere = cursor::encode(
            "index",
            "asc",
            &cursor::filter_digest(&["run-b"]),
            cursor::CursorKey::Int(1),
            "",
        );
        let refused = HistorySpec::resolve("run-a", None, None, Some(&elsewhere))
            .expect_err("a cursor from another run");
        assert_eq!(refused.code(), "BAD_USER_INPUT");
    }
}
