//! The `updateJob` and `bulkExport` fields: polling the background jobs this
//! server hands a caller an id for instead of holding the request open.

use async_graphql::Context;

use super::super::super::types::AppState;
use super::super::types::update::UpdateJob;

/// One update run, by id.
///
/// Null when no job carries that id. The last few runs are kept, so an
/// operator reading back after the fact finds the job rather than nothing.
pub(crate) async fn update_job(ctx: &Context<'_>, id: String) -> Option<UpdateJob> {
    let state = ctx.data_unchecked::<AppState>();
    state.update_jobs.get(&id).map(UpdateJob::from)
}

/// Poll an export this server started.
///
/// Null when no export carries that id: it was never started, or it has
/// expired. An export's file is kept for an hour, and its record goes with
/// the file, so neither outlives the other.
pub(crate) async fn bulk_export(ctx: &Context<'_>, id: String) -> Option<BulkExport> {
    let state = ctx.data_unchecked::<AppState>();
    state
        .caches
        .exports
        .get(&id)
        .map(|job| BulkExport::from_job(state, &job))
}

/// An export job, as a client polls it.
#[derive(async_graphql::SimpleObject)]
pub(crate) struct BulkExport {
    /// The job's id, which `bulkExport` and `node` both take. Unique to this
    /// server: the jobs live in memory, so nothing answers to it after a
    /// restart.
    #[graphql(owned)]
    pub(crate) id: async_graphql::ID,
    /// Where it has got to: `queued`, `running`, `complete` or `failed`.
    pub(crate) status: String,
    /// How many runs have been written.
    pub(crate) written: i32,
    /// Why it failed, when it did.
    pub(crate) error: Option<String>,
    /// A short-lived signed link to the JSONL. Null until the export is
    /// complete, because there is nothing to fetch before then.
    pub(crate) download_url: Option<String>,
}

impl BulkExport {
    /// Describe a job, minting its link once there is a file to fetch.
    pub(crate) fn from_job(
        state: &AppState,
        job: &super::super::super::core::export::ExportJob,
    ) -> Self {
        let complete = job.status == super::super::super::core::export::ExportStatus::Complete;
        Self {
            id: async_graphql::ID(job.id.clone()),
            status: job.status.wire().to_string(),
            written: count(job.written),
            error: job.error.clone(),
            download_url: complete.then(|| {
                super::super::super::signed_url::signed_path(
                    &state.signer,
                    &format!("/api/exports/{}", job.id),
                    &[],
                    leviath_core::duration::now_secs(),
                )
            }),
        }
    }
}

/// Narrow a count to the 32 bits GraphQL's `Int` carries.
fn count(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}
