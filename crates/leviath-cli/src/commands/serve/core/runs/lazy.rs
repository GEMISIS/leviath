//! The run listing, walked lazily.
//!
//! [`super::list`] answers `GET /api/runs`: it filters, sorts, searches under a
//! scan budget and then pages, and every one of those steps happens for every
//! run whether or not the page being asked for reaches it. That is the right
//! shape for a route whose filters are all answerable from the run index, and
//! it is what REST keeps.
//!
//! [`walk`] answers the same question the other way round. It runs the same
//! filters, but splits each one into the half that is free and the half that
//! costs a file, sorts and seeks past the cursor on the free half alone, and
//! only then reads - for the runs this page reaches, and no others. Nothing is
//! capped, so a selective filter over a large store takes as long as it takes
//! rather than quietly returning less than it found.
//!
//! The five steps themselves are not written here: they are
//! [`paging::walk`](crate::commands::serve::graphql::paging::walk), shared with
//! every other listing. What is here is what a *run* means by each of them.

use std::collections::HashSet;
use std::sync::Arc;

use futures_util::future::BoxFuture;

use super::matching;
use super::predicate::{MatchContext, RunPredicate};
use super::{ParentFilter, RunSpec, SortKey, Source};
use crate::commands::serve::cursor::CursorKey;
use crate::commands::serve::graphql::paging::order::{Order, OrderField, Orderable, Position};
use crate::commands::serve::graphql::paging::walk::{Page, Sift, Verdict};
use crate::commands::serve::types::status_matches;
use crate::runstate::RunMeta;

impl OrderField for SortKey {
    fn wire(self) -> &'static str {
        // The same word the REST query parameter uses, which is what keeps a
        // cursor from either surface readable by the other.
        self.as_str()
    }
}

impl Orderable<MatchContext> for RunMeta {
    type Field = SortKey;

    fn key(&self, field: SortKey, _cx: &MatchContext) -> CursorKey {
        field.key(self)
    }
}

/// What a run listing means by "matches" and "comes first".
///
/// Owns everything it consults rather than borrowing the spec, so a page can
/// outlive the request that built it - which is what lets a connection hold on
/// to the walk and count the listing later, only if a client asks.
pub(crate) struct RunSift {
    /// Which runs the listing is about.
    parent: ParentFilter,
    /// Every run under the tree the parent filter names, when it names one.
    descendants: HashSet<String>,
    /// Only runs of this blueprint, by recorded name.
    blueprint: Option<String>,
    /// Status filters, in the daemon's own spelling.
    statuses: Vec<String>,
    /// Inclusive lower bound on the sort value.
    since: Option<i64>,
    /// The surface's own filter tree.
    predicate: Option<Arc<dyn RunPredicate>>,
    /// What a predicate may consult beyond a run's own record.
    ctx: MatchContext,
    /// The search text, when there is one.
    q: Option<String>,
    /// Search sources answerable from the parsed record.
    cheap_sources: Vec<Source>,
    /// Search sources that cost a file read, which is the half that waits.
    deep_sources: Vec<Source>,
    /// Which timestamp the listing is ordered by.
    sort: SortKey,
    /// The compiled order, which also mints the cursor.
    order: Order<SortKey>,
}

impl RunSift {
    /// Take from a spec everything a walk consults.
    pub(crate) fn new(spec: &RunSpec, descendants: HashSet<String>, ctx: MatchContext) -> Self {
        let (deep_sources, cheap_sources) = spec
            .sources
            .iter()
            .copied()
            .partition(|source| source.reads_filesystem());
        Self {
            parent: spec.parent.clone(),
            descendants,
            blueprint: spec.blueprint.clone(),
            statuses: spec.statuses.clone(),
            since: spec.since,
            predicate: spec.predicate.clone(),
            ctx,
            q: spec.q.clone(),
            cheap_sources,
            deep_sources,
            sort: spec.sort,
            order: spec.order.clone(),
        }
    }

    /// The order this listing runs in, which is what mints its cursors.
    pub(crate) fn order(&self) -> &Order<SortKey> {
        &self.order
    }
}

impl Sift for RunSift {
    type Item = Arc<RunMeta>;

    fn test(&self, item: &Arc<RunMeta>) -> Verdict {
        let meta = item.as_ref();
        if !self.parent.keeps_in(meta, &self.descendants) {
            return Verdict::Drop;
        }
        if let Some(ref blueprint) = self.blueprint
            && &meta.agent_name != blueprint
        {
            return Verdict::Drop;
        }
        if !self.statuses.is_empty()
            && !self
                .statuses
                .iter()
                .any(|filter| status_matches(&meta.status, filter))
        {
            return Verdict::Drop;
        }
        if let Some(since) = self.since
            && self.sort.key(meta) < CursorKey::Int(since)
        {
            return Verdict::Drop;
        }

        let mut waits = false;
        if let Some(ref predicate) = self.predicate {
            match predicate.verdict(item, &self.ctx) {
                Verdict::Drop => return Verdict::Drop,
                Verdict::NeedsIo => waits = true,
                Verdict::Keep => {}
            }
        }
        // Sources are OR-ed, so a match in the parsed record settles the search
        // outright and no file is opened for it at all.
        if let Some(ref q) = self.q
            && !matching::matches_query(meta, q, &self.cheap_sources)
        {
            if self.deep_sources.is_empty() {
                return Verdict::Drop;
            }
            waits = true;
        }
        match waits {
            true => Verdict::NeedsIo,
            false => Verdict::Keep,
        }
    }

    fn confirm<'a>(&'a self, item: &'a Arc<RunMeta>) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let meta = item.as_ref();
            if let Some(ref predicate) = self.predicate
                && predicate.verdict(item, &self.ctx) == Verdict::NeedsIo
                && !predicate.confirm(item, &self.ctx).await
            {
                return false;
            }
            match self.q {
                None => true,
                Some(ref q) => {
                    matching::matches_query(meta, q, &self.cheap_sources)
                        || matching::matches_query(meta, q, &self.deep_sources)
                }
            }
        })
    }

    fn position(&self, item: &Arc<RunMeta>) -> Position {
        self.order
            .position(item.as_ref(), item.run_id.clone(), &self.ctx)
    }

    fn descending(&self) -> &[bool] {
        self.order.descending()
    }
}

/// The cursor a page of runs ends on, absent when it is the last page.
pub(crate) fn next_cursor(page: &Page<RunSift>, digest: &str) -> Option<String> {
    page.next
        .as_ref()
        .map(|position| page.sift().order().encode(digest, position))
}

#[cfg(test)]
#[path = "lazy_tests.rs"]
mod tests;
