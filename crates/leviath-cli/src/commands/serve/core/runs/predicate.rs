//! The seam between a run listing and the predicate it keeps runs by.
//!
//! The listing walks the index, sorts and pages. What counts as a match is
//! somebody else's question: `GET /api/runs` answers it with the flat query
//! parameters the listing already holds, and the GraphQL `runs` field answers
//! it with a composable filter tree. The trait below is what the tree arrives
//! as, so the listing gains no knowledge of either surface's vocabulary.
//!
//! A filter that can only be answered by reading a file splits in two:
//! [`RunPredicate::verdict`] answers the half that is free, and
//! [`RunPredicate::confirm`] settles the rest. That split is what lets the lazy
//! walk sort and seek before it opens anything, and read only for the runs the
//! page it was asked for actually reaches. It is also why a predicate is
//! consulted by [`walk`](super::walk) alone: the flat REST listing is
//! synchronous from end to end, and a filter that may need a file has no way to
//! be answered there.
//!
//! A predicate that reads nothing implements neither of the two, and its
//! `matches` answers both.

use std::collections::HashMap;
use std::sync::Arc;

use futures_util::future::BoxFuture;

use crate::commands::serve::graphql::paging::walk::Verdict;
use crate::runstate::RunMeta;

/// What a predicate may consult beyond the run's own record.
pub(crate) struct MatchContext {
    /// Daemon time the page was built at, so every age in one answer is
    /// measured from one instant rather than drifting run by run.
    pub(crate) now: i64,
    /// The runs this listing started from, linked child to parent.
    ///
    /// A grandchild names its parent and not its ancestor, so a filter that
    /// asks about the tree cannot be answered from one record. The links are
    /// built once, here, rather than walked per run.
    pub(crate) tree: Arc<RunTree>,
}

impl MatchContext {
    /// A context with a clock and no tree.
    ///
    /// For a listing whose filter asks about nothing above the run in front of
    /// it, which is every listing `GET /api/runs` serves.
    pub(crate) fn at(now: i64) -> Self {
        Self {
            now,
            tree: Arc::new(RunTree::default()),
        }
    }
}

/// One reading of the run store, by id.
///
/// Owned rather than borrowed, and shared behind an `Arc`, because a page can
/// outlive the request that built it: a connection holds on to the walk so it
/// can count the listing later, only if a client asks for the count.
#[derive(Debug, Default)]
pub(crate) struct RunTree {
    /// Every run the listing started from, by its own id.
    by_id: HashMap<String, Arc<RunMeta>>,
}

/// What a build of the run tree is reported to.
///
/// One tree per listing is what this costs; one per run in a listing is the
/// same work squared, and no answer would look any different for it. A promise
/// about work that does not happen is worth what a test can check, so builds
/// are reported here and a test counts them.
pub(crate) type TreeBuildRecorder = Box<dyn Fn(&[Arc<RunMeta>]) + Send + Sync>;

/// Where a tree build is reported, when anything is listening.
///
/// Nothing installs a recorder in a server: the list would grow for ever and
/// nothing but a test has any use for it, so a build costs a load of this and
/// a call it does not make.
static RECORDER: std::sync::OnceLock<TreeBuildRecorder> = std::sync::OnceLock::new();

/// Report every tree build to `record`, for as long as this process lives.
///
/// Once, deliberately: a second call is a no-op, so each test that wants the
/// log can ask for it rather than arranging to be the one that installs it.
#[cfg(test)]
pub(crate) fn record_tree_builds(record: TreeBuildRecorder) {
    drop(RECORDER.set(record));
}

/// Note that a tree is being linked, for the tests that count them.
fn noted(runs: &[Arc<RunMeta>]) {
    if let Some(record) = RECORDER.get() {
        record(runs);
    }
}

impl RunTree {
    /// Link these runs together, so a filter can walk from one to its parent.
    pub(crate) fn of(runs: &[Arc<RunMeta>]) -> Self {
        noted(runs);
        Self {
            by_id: runs
                .iter()
                .map(|meta| (meta.run_id.clone(), Arc::clone(meta)))
                .collect(),
        }
    }
}

impl RunTree {
    /// The ids above this one, root first, or nothing for a run this tree does
    /// not know.
    pub(crate) fn ancestors(&self, id: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut above = self.parent_of(id);
        while let Some(parent) = above {
            // A chain cannot be longer than the store, so a record that somehow
            // names an ancestor of itself stops here rather than walking for
            // ever.
            if out.len() >= self.by_id.len() {
                break;
            }
            above = self.parent_of(&parent);
            out.push(parent);
        }
        // Walked upwards, reported downwards: root first, so the list reads as
        // a breadcrumb and `has` finds an ancestor wherever it sits.
        out.reverse();
        out
    }

    /// One run's own record, for a relation that filters on the run rather
    /// than on its id.
    pub(crate) fn run(&self, id: &str) -> Option<Arc<RunMeta>> {
        self.by_id.get(id).cloned()
    }

    /// The id of the run that started this one, when this tree holds it.
    fn parent_of(&self, id: &str) -> Option<String> {
        self.by_id
            .get(id)
            .and_then(|meta| meta.parent_run_id.clone())
    }
}

/// Whether a run belongs in a listing.
///
/// The record arrives as the shared `Arc` the index holds rather than as a
/// reference, because a predicate that mirrors an output type has to hand the
/// record to an object that owns it, and a page of fifty is fifty pointer
/// copies either way.
pub(crate) trait RunPredicate: std::fmt::Debug + Send + Sync {
    /// Whether this run is kept, answered from the record alone.
    fn matches(&self, meta: &Arc<RunMeta>, ctx: &MatchContext) -> bool;

    /// Whether this run is kept, where the answer may still need a file.
    ///
    /// Defaulted to [`matches`](Self::matches), which is the whole answer for a
    /// predicate that reads nothing. One that does read overrides this with the
    /// half that is free and says [`Verdict::NeedsIo`] for the rest, so a walk
    /// can order and seek before it opens anything.
    fn verdict(&self, meta: &Arc<RunMeta>, ctx: &MatchContext) -> Verdict {
        match self.matches(meta, ctx) {
            true => Verdict::Keep,
            false => Verdict::Drop,
        }
    }

    /// Settle a run this predicate answered [`Verdict::NeedsIo`] for.
    ///
    /// Reached only for the runs a page actually walks to, and never for one a
    /// cursor already skipped past. The default is never called, because a
    /// predicate that does not say `NeedsIo` has nothing left to settle.
    fn confirm<'a>(
        &'a self,
        _meta: &'a Arc<RunMeta>,
        _ctx: &'a MatchContext,
    ) -> BoxFuture<'a, bool> {
        Box::pin(std::future::ready(false))
    }

    /// This predicate's contribution to the cursor's filter digest.
    ///
    /// A keyset cursor is a promise about one walk. Folding the predicate into
    /// the digest is what turns "the filter changed halfway through" into a
    /// refusal the client can act on rather than a page of quietly wrong runs.
    fn digest_part(&self) -> String;
}

#[cfg(test)]
#[path = "predicate_tests.rs"]
mod tests;
