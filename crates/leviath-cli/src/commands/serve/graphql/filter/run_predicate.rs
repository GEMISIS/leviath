//! The adapter between the run mirror and the run listing.
//!
//! `RunInput` is an input object shaped like `RunOutput`; the listing in
//! `core::runs` knows nothing about GraphQL and asks its filters through
//! [`RunPredicate`]. This file is the whole of the translation: a filter, the
//! `Run` the value being tested becomes, and the three-valued answer turned
//! into the verdict the walk reads.
//!
//! [`selection`] is the one entry point for everything that is not a page. A
//! bulk mutation and a subscription both need "every run this filter matches",
//! and both get it from here rather than from a second reading of the filter.

use std::sync::Arc;

use futures_util::future::BoxFuture;

use super::super::super::core::error::ServeError;
use super::super::super::core::runs::predicate::{MatchContext, RunPredicate};
use super::super::super::core::runs::{self as run_core, ParentFilter, RunSelection, SortKey};
use super::super::super::types::AppState;
use super::super::paging::digest::canonical;
use super::super::paging::walk::Verdict;
use super::super::types::run::{Run, RunFilter};
use super::{Filterable, MatchCx, Tri, verdict};
use crate::runstate::RunMeta;

/// One run filter, as the listing consults it.
///
/// Holds the filter the client wrote and the text it digests to, because the
/// digest is taken over what was written rather than over anything compiled:
/// that is what keeps a cursor readable across a build that adds a field to
/// the mirror.
#[derive(Debug)]
struct Mirrored {
    /// What the client asked for.
    filter: RunFilter,
    /// The canonical rendering of it, for the cursor's digest.
    digest: String,
}

impl Mirrored {
    /// The filter context one comparison runs in.
    ///
    /// The clock and the run tree are the listing's, so every age in one answer
    /// is measured from one instant and every relation is read from one
    /// snapshot.
    fn cx<'a>(&self, ctx: &'a MatchContext) -> MatchCx<'a> {
        MatchCx::at(ctx.now).with_parents(ctx.tree.as_ref())
    }

    /// The record as the object the mirror is a mirror of.
    ///
    /// A pointer copy: the run holds the same `Arc` the index does, so a page
    /// of fifty comparisons allocates nothing per run.
    fn object(&self, meta: &Arc<RunMeta>, ctx: &MatchContext) -> Run {
        Run {
            meta: Arc::clone(meta),
            now: ctx.now,
        }
    }
}

impl RunPredicate for Mirrored {
    fn matches(&self, meta: &Arc<RunMeta>, ctx: &MatchContext) -> bool {
        // Only what memory settles. A filter that names a file is undecided
        // here, and the walk settles it in `confirm`.
        self.object(meta, ctx).test(&self.filter, &self.cx(ctx)) == Tri::Yes
    }

    fn verdict(&self, meta: &Arc<RunMeta>, ctx: &MatchContext) -> Verdict {
        verdict(self.object(meta, ctx).test(&self.filter, &self.cx(ctx)))
    }

    fn confirm<'a>(&'a self, meta: &'a Arc<RunMeta>, ctx: &'a MatchContext) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let run = self.object(meta, ctx);
            let cx = self.cx(ctx);
            run.confirm(&self.filter, &cx).await
        })
    }

    fn digest_part(&self) -> String {
        self.digest.clone()
    }
}

/// Compile one run filter into the predicate a listing consults.
///
/// Nothing for a filter that asks nothing. That is not an optimisation: an
/// empty filter has to contribute nothing at all to the cursor's digest, and a
/// predicate that is present contributes a part. It is what keeps an unfiltered
/// GraphQL cursor byte-identical to the one `GET /api/runs` mints.
pub(crate) fn compile(filter: RunFilter) -> Result<Option<Arc<dyn RunPredicate>>, ServeError> {
    let digest = canonical(&filter)?;
    match digest.is_empty() {
        true => Ok(None),
        false => Ok(Some(Arc::new(Mirrored { filter, digest }))),
    }
}

/// The ids a filter names outright on its own top level.
///
/// `id: { eq }` and `id: { in }` say which runs to read as well as which to
/// keep, so a listing that carries one reads those records directly instead of
/// walking the index. Everything else in the filter still applies to what comes
/// back, which is what makes "these ids, and only the ones that failed" one
/// request.
pub(crate) fn named_ids(filter: &RunFilter) -> Result<Option<Vec<String>>, ServeError> {
    let Some(id) = filter.id.as_deref() else {
        return Ok(None);
    };
    let mut ids: Vec<String> = Vec::new();
    if let Some(ref one) = id.eq {
        ids.push(one.as_str().to_string());
    }
    if let Some(ref set) = id.within {
        ids.extend(set.iter().map(|id| id.as_str().to_string()));
    }
    if ids.is_empty() {
        return Ok(None);
    }
    if ids.len() > run_core::MAX_IDS {
        return Err(ServeError::BadRequest(format!(
            "`id` names {} runs; at most {} may be named at once",
            ids.len(),
            run_core::MAX_IDS
        )));
    }
    Ok(Some(ids))
}

/// What one filter asks of the listing, before a page or a cursor.
///
/// The three search fields are left empty here: searching is an argument of the
/// `runs` field rather than part of a filter, and everything that calls this
/// wants the filter alone.
pub(crate) fn asking(filter: RunFilter, limit: usize) -> Result<RunSelection, ServeError> {
    let ids = named_ids(&filter)?;
    Ok(RunSelection {
        ids,
        predicate: compile(filter)?,
        ..anything(limit)
    })
}

/// The selection that asks nothing at all, which is every run.
///
/// A plain value rather than an empty filter run through [`asking`]: a filter
/// with nothing in it cannot be refused, and a refusal no request can reach is
/// a branch nobody can read the meaning of.
fn anything(limit: usize) -> RunSelection {
    RunSelection {
        limit,
        blueprint: None,
        statuses: Vec::new(),
        sort: SortKey::Started,
        descending: true,
        order: None,
        q: None,
        sources: Vec::new(),
        sources_raw: String::new(),
        fields: None,
        ids: None,
        since: None,
        parent: ParentFilter::Any,
        predicate: None,
        preloaded: None,
    }
}

/// Every run one filter matches, whatever it takes to know.
///
/// The one function the bulk mutations and the subscriptions resolve a
/// `RunInput` with, so a run a page would have listed is exactly a run a bulk
/// pause acts on. The walk runs in full: a filter that has to read a file reads
/// it, for every run rather than for a page of them, so this costs what the
/// filter costs and is not something to call per frame.
///
/// The order is the listing's default, newest first, and there is no page: the
/// caller asked for a set rather than a slice of one.
pub(crate) async fn selection(
    filter: Option<RunFilter>,
    state: &AppState,
) -> Result<Vec<Arc<RunMeta>>, ServeError> {
    Ok(walked(asking(filter.unwrap_or_default(), 1)?, state).await)
}

/// Every run one selection names, with no page to stop at.
///
/// The limit a selection carries is the page's, and this walk has no page, so
/// the one the callers pass is only what a spec needs to exist at all.
async fn walked(asked: RunSelection, state: &AppState) -> Vec<Arc<RunMeta>> {
    run_core::walk_all(state, &asked.unpaged()).await
}

/// Every run one filter matches, for an act that refuses to match every run.
///
/// The refusal reads the compiled filter rather than rendering the filter a
/// second time to ask the same question: a destructive act over an empty
/// filter is almost always a client that built its query wrong, and the one
/// time it is not, saying so costs one field.
pub(crate) async fn selection_for(
    filter: RunFilter,
    act: &str,
    state: &AppState,
) -> Result<Vec<Arc<RunMeta>>, ServeError> {
    let asked = asking(filter, 1)?;
    if asked.predicate.is_none() {
        return Err(ServeError::BadRequest(format!(
            "an empty filter names every run on this machine; refusing to {act} all of them"
        )));
    }
    Ok(walked(asked, state).await)
}

/// The same set, as the listing spec an export runs on.
///
/// An export's answer is a file rather than a response, so it has no page cap
/// to respect; what it does need is the runs named outright, because writing
/// the file is a second pass and a filter that reads files must not be answered
/// twice.
///
/// The records travel with the ids. The walk above already holds every one of
/// them, and an export that handed over only the ids would have the listing
/// open the whole store again to get back what it had just let go.
pub(crate) async fn everything(
    filter: Option<RunFilter>,
    state: &AppState,
) -> Result<RunSelection, ServeError> {
    let matched = selection(filter, state).await?;
    Ok(RunSelection {
        ids: Some(matched.iter().map(|meta| meta.run_id.clone()).collect()),
        preloaded: Some(matched),
        ..anything(usize::MAX)
    })
}

#[cfg(test)]
#[path = "run_predicate_tests.rs"]
mod tests;
