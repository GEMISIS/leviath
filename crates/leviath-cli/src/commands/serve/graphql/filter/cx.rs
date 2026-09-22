//! What a filter knows besides the value in front of it.
//!
//! One of these is built per request and handed to every test. It exists so
//! that a filter is a pure function of the value and this context: two runs
//! compared in the same request compare against the same clock, and a relation
//! a filter walks is read from the same snapshot.

use std::sync::Arc;

use crate::runstate::RunMeta;

/// The run tree a relation filter walks.
///
/// `parentId` is on the run itself; the chain above it is not, and the record
/// of the run that started it is not either, so walking either per run would
/// re-read the index once per comparison. The listing resolves the tree once
/// and lends it out through here.
pub(crate) trait Ancestry: Sync {
    /// The ids above this one, root first, or nothing for a run this tree
    /// does not know.
    fn ancestors(&self, id: &str) -> Vec<String>;

    /// One run's own record, for a relation that filters on the run rather
    /// than on its id.
    fn run(&self, id: &str) -> Option<Arc<RunMeta>>;
}

/// An ancestry that knows nothing, for a listing with no run tree behind it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Unattached;

impl Ancestry for Unattached {
    fn ancestors(&self, _id: &str) -> Vec<String> {
        Vec::new()
    }

    fn run(&self, _id: &str) -> Option<Arc<RunMeta>> {
        None
    }
}

/// The one [`Unattached`] every context with no run tree borrows.
static UNATTACHED: Unattached = Unattached;

/// What every test in one request shares.
pub(crate) struct MatchCx<'a> {
    /// The clock this whole request compares against, in unix seconds.
    ///
    /// Read once so that two runs in the same listing are aged against the
    /// same instant, and so that a test is reproducible from its inputs.
    pub(crate) now: i64,
    /// The ancestry a relation filter walks.
    pub(crate) parents: &'a dyn Ancestry,
}

impl<'a> MatchCx<'a> {
    /// A context with a clock and no run tree.
    pub(crate) fn at(now: i64) -> Self {
        Self {
            now,
            parents: &UNATTACHED,
        }
    }

    /// The same context, with an ancestry a relation filter can walk.
    pub(crate) fn with_parents(self, parents: &'a dyn Ancestry) -> Self {
        Self { parents, ..self }
    }
}

impl std::fmt::Debug for MatchCx<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MatchCx")
            .field("now", &self.now)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
#[path = "cx_tests.rs"]
mod tests;
