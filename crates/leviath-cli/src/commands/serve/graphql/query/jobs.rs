//! The `updateJob`, `updateJobs` and `runExport` fields: polling the
//! background jobs this server hands a caller an id for instead of holding the
//! request open.

use async_graphql::{Context, Enum, ID, SimpleObject};
use leviath_graphql_derive::mirror;

use super::super::super::types::AppState;
use super::super::connection::Connection;
use super::super::paging::order::{OrderDirection, Term};
use super::super::scalars::Cursor;
use super::super::types::update::{
    UpdateJob, UpdateJobFilter, UpdateJobOrder, UpdateJobOrderField,
};
use super::listing::{Window, connection, terms};

/// How many update runs one page may carry.
///
/// A server keeps the last few, so this is about a client that meant to page
/// and did not rather than about a store of any size.
const PAGE_CAP: usize = 200;

/// What the cap is called when a request goes over it.
const CAP_NAME: &str = "the update job page cap";

/// The order the update runs are read in when nothing says otherwise.
///
/// By id ascending, which is oldest first: an id carries the second its job
/// started, so this is the order they happened in.
fn by_id() -> Vec<Term<UpdateJobOrderField>> {
    vec![Term {
        field: UpdateJobOrderField::Id,
        direction: OrderDirection::Asc,
    }]
}

/// One update run, by id.
///
/// Null when no job carries that id. The last few runs are kept, so an
/// operator reading back after the fact finds the job rather than nothing.
pub(crate) async fn update_job(ctx: &Context<'_>, id: ID) -> Option<UpdateJob> {
    let state = ctx.data_unchecked::<AppState>();
    state.update_jobs.get(&id).map(UpdateJob::from)
}

/// Every update run this server has done.
pub(crate) async fn update_jobs(
    ctx: &Context<'_>,
    filter: Option<UpdateJobFilter>,
    order_by: Option<Vec<UpdateJobOrder>>,
    first: i32,
    after: Option<Cursor>,
) -> async_graphql::Result<Connection<UpdateJob>> {
    let state = ctx.data_unchecked::<AppState>();
    let jobs: Vec<UpdateJob> = state
        .update_jobs
        .all()
        .into_iter()
        .map(UpdateJob::from)
        .collect();
    connection(
        jobs,
        filter,
        terms(order_by, UpdateJobOrder::term, by_id),
        |job: &UpdateJob| job.id.to_string(),
        Window {
            first,
            after,
            cap: PAGE_CAP,
            cap_name: CAP_NAME,
        },
    )
    .await
}

/// Poll an export this server started.
///
/// Null when no export carries that id: it was never started, or it has
/// expired. An export's file is kept for an hour, and its record goes with
/// the file, so neither outlives the other.
pub(crate) async fn run_export(ctx: &Context<'_>, id: ID) -> Option<RunExport> {
    let state = ctx.data_unchecked::<AppState>();
    state
        .caches
        .exports
        .get(&id)
        .map(|job| RunExport::from_job(state, &job))
}

/// Where an export has got to.
#[mirror]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum ExportStatus {
    /// Enqueued; the worker has not started it.
    Queued,
    /// Writing.
    Running,
    /// Written, and the file is there to fetch.
    Complete,
    /// It broke. `error` says how.
    Failed,
}

impl From<super::super::super::core::export::ExportStatus> for ExportStatus {
    fn from(status: super::super::super::core::export::ExportStatus) -> Self {
        use super::super::super::core::export::ExportStatus as Core;
        match status {
            Core::Queued => Self::Queued,
            Core::Running => Self::Running,
            Core::Complete => Self::Complete,
            Core::Failed => Self::Failed,
        }
    }
}

/// An export of the run store, as a client polls it.
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct RunExport {
    /// The job's id, which `runExport` and `node` both take. Unique to this
    /// server: the jobs live in memory, so nothing answers to it after a
    /// restart.
    #[graphql(owned)]
    pub(crate) id: ID,
    /// Where it has got to.
    pub(crate) status: ExportStatus,
    /// How many runs have been written.
    pub(crate) written: i32,
    /// Why it failed, when it did.
    pub(crate) error: Option<String>,
    /// A short-lived signed link to the JSONL. Null until the export is
    /// complete, because there is nothing to fetch before then.
    pub(crate) download_url: Option<String>,
}

impl RunExport {
    /// Describe a job, minting its link once there is a file to fetch.
    pub(crate) fn from_job(
        state: &AppState,
        job: &super::super::super::core::export::ExportJob,
    ) -> Self {
        let complete = job.status == super::super::super::core::export::ExportStatus::Complete;
        Self {
            id: ID(job.id.clone()),
            status: ExportStatus::from(job.status),
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
