//! What a run's mirror reads that is not on the run in front of it.
//!
//! Two kinds of field are read through here. The tree ones - the run that
//! started this one, and the chain above that - reach for the shared index,
//! which a filter cannot do: a filter has to be answerable from memory, once
//! per run, with no reads and no `await`. They are read off the tree the
//! listing resolved once and lent to every test in the request.
//!
//! The child lists - the stage ledger, the stored parts, the submitted files -
//! are answered by a resolver as a page, because that is the shape a client
//! reads them in. A filter is about the things themselves rather than about one
//! page of them, so it reads the whole list through here and the resolver pages
//! that same list.
//!
//! Each function below is the same value its resolver answers with, which is
//! what makes the mirror a mirror: `parent` on the filter selects exactly the
//! runs `parent` on the object would have answered for.

use std::sync::Arc;

use super::super::super::blocking::blocking;
use super::super::super::core::runs::predicate::RunTree;
use super::super::types::run::{Run, counted};
use super::super::types::run_detail::{Artifact, BlobEntry, StageRecord};
use super::{Ancestry, MatchCx};
use crate::runstate::RunMeta;

/// The run tree the listing resolved, as the filter system asks about it.
///
/// Delegation only. The links themselves are the listing's, built once from
/// the snapshot it walked, and this is the face a filter sees them through.
impl Ancestry for RunTree {
    fn ancestors(&self, id: &str) -> Vec<String> {
        RunTree::ancestors(self, id)
    }

    fn run(&self, id: &str) -> Option<Arc<RunMeta>> {
        RunTree::run(self, id)
    }
}

/// The run that started this one, from the tree the listing lent the request.
///
/// Null for a run nobody started, and for one whose parent this listing did not
/// read - a parent whose record has since been deleted, or a listing that named
/// its runs by id and so never read the one above them.
pub(crate) fn parent_of(run: &Run, cx: &MatchCx<'_>) -> Option<Run> {
    let parent_id = run.meta.parent_run_id.as_deref()?;
    cx.parents
        .run(parent_id)
        .map(|meta| Run { meta, now: cx.now })
}

/// Every run above this one, root first.
///
/// The ids rather than the runs, because that is what the field answers with
/// and what `has` compares against.
pub(crate) fn ancestor_ids(run: &Run, cx: &MatchCx<'_>) -> Vec<String> {
    cx.parents.ancestors(&run.meta.run_id)
}

/// One run's whole stage ledger, in the order the blueprint declares.
///
/// The `stages` resolver pages this; a filter quantifies over it. One file
/// either way, which is why the read is noted for the lazy walk's own tests.
pub(crate) async fn stage_records(run_id: &str) -> Vec<StageRecord> {
    counted(run_id);
    let owned = run_id.to_string();
    let records = blocking(move || crate::runstate::read_stages_index(&owned)).await;
    records.iter().map(StageRecord::from).collect()
}

/// The stage ledger, for the filter mirror.
pub(crate) async fn stages_of(run: &Run, _cx: &MatchCx<'_>) -> Vec<StageRecord> {
    stage_records(&run.meta.run_id).await
}

/// Every part one run stored, as the store lists them.
///
/// The bytes stay where they are: this is the record of each part, which is
/// what the `blobs` resolver pages and what a filter quantifies over.
pub(crate) async fn stored_parts(run_id: &str) -> Vec<crate::blobs::BlobEntry> {
    let owned = run_id.to_string();
    blocking(move || crate::blobs::list(&owned))
        .await
        .unwrap_or_default()
}

/// The stored parts, for the filter mirror.
///
/// Without the signed links the resolver mints: `url` is out of this mirror, so
/// there is nothing here for a link to be compared against.
pub(crate) async fn blobs_of(run: &Run, _cx: &MatchCx<'_>) -> Vec<BlobEntry> {
    stored_parts(&run.meta.run_id)
        .await
        .into_iter()
        .map(|stored| BlobEntry::of(stored, None))
        .collect()
}

/// The files one submission handed back, for the filter mirror.
///
/// The run's own record holds these, so this reads nothing: a filter on
/// `artifacts` is settled in the same phase as one on `status`.
pub(crate) fn artifacts_of(run: &Run, _cx: &MatchCx<'_>) -> Vec<Artifact> {
    run.meta
        .final_output
        .as_ref()
        .map(|output| output.artifacts.as_slice())
        .unwrap_or_default()
        .iter()
        .map(Artifact::unlinked)
        .collect()
}

#[cfg(test)]
#[path = "run_relations_tests.rs"]
mod tests;
