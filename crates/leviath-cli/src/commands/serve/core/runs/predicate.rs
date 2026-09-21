//! The seam between a run listing and the predicate it keeps runs by.
//!
//! The listing walks the index, sorts and pages. What counts as a match is
//! somebody else's question: `GET /api/runs` answers it with the flat query
//! parameters the listing already holds, and the GraphQL `runs` field answers
//! it with a composable filter tree. The trait below is what the tree arrives
//! as, so the listing gains no knowledge of either surface's vocabulary.
//!
//! A predicate is consulted once per run, in the same place every other filter
//! is applied: before the sort and before the count, so `total` describes what
//! was asked for.

use std::collections::{HashMap, HashSet};

use crate::runstate::RunMeta;

/// What a predicate may consult beyond the run's own record.
pub(crate) struct MatchContext {
    /// Daemon time the page was built at, so every age in one answer is
    /// measured from one instant rather than drifting run by run.
    pub(crate) now: i64,
    /// The descendants of each run a subtree predicate names, keyed by that
    /// run's id.
    ///
    /// A grandchild names its parent and not its ancestor, so a subtree cannot
    /// be decided from one record. The tree is walked once, here, rather than
    /// per run.
    pub(crate) subtrees: HashMap<String, HashSet<String>>,
}

/// Whether a run belongs in a listing.
pub(crate) trait RunPredicate: std::fmt::Debug + Send + Sync {
    /// Whether this run is kept.
    fn matches(&self, meta: &RunMeta, ctx: &MatchContext) -> bool;

    /// This predicate's contribution to the cursor's filter digest.
    ///
    /// A keyset cursor is a promise about one walk. Folding the predicate into
    /// the digest is what turns "the filter changed halfway through" into a
    /// refusal the client can act on rather than a page of quietly wrong runs.
    fn digest_part(&self) -> String;

    /// Every run whose subtree this predicate asks about, appended to `out`.
    ///
    /// Read before the walk, so the listing knows which trees to resolve.
    fn subtree_roots(&self, out: &mut Vec<String>);
}
