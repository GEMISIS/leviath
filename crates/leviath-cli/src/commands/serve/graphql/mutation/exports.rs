//! The `startRunExport` field: starting a whole-store export a client polls
//! rather than pages through.

use async_graphql::{Context, InputObject, SimpleObject};

use super::super::super::types::AppState;
use super::super::error::IntoGraphql;
use super::super::filter::run_predicate;
use super::super::query::RunExport;
use super::super::types::run::RunFilter;

/// Which runs to export, and which of their fields to keep.
#[derive(Debug, InputObject)]
pub(crate) struct StartRunExportRequest {
    /// Which runs to export. Omitted means all of them: an export is the one
    /// act where "everything" is the ordinary ask.
    pub(crate) filter: Option<RunFilter>,
    /// Which top-level run fields to keep in each row. An unknown name is
    /// refused rather than dropped.
    pub(crate) fields: Option<Vec<String>>,
}

/// What starting an export answers with.
#[derive(SimpleObject)]
pub(crate) struct StartRunExportResult {
    /// The job, to poll until it is complete.
    pub(crate) export: RunExport,
}

/// Export the run store to a file, and hand back the job.
///
/// Paging ten thousand runs through a connection is two hundred requests, and a
/// client that wants everything wants it once. This returns immediately; poll
/// `runExport(id:)` and fetch `downloadUrl` when it is complete.
///
/// The filter is the run listing's own, so a client builds the predicate once
/// and uses it for both. `fields` narrows each row, and an unknown name is
/// refused rather than dropped: a column quietly missing from an export is
/// discovered downstream, by somebody else.
pub(crate) async fn start_run_export(
    ctx: &Context<'_>,
    request: StartRunExportRequest,
) -> async_graphql::Result<StartRunExportResult> {
    let state = ctx.data_unchecked::<AppState>();
    // An export is not a page, so the page cap does not apply: the whole
    // point is everything at once. The listing's own scan bounds still do.
    let mut selection = run_predicate::everything(request.filter, state)
        .await
        .gql()?;
    selection.fields = request.fields.map(|named| named.into_iter().collect());
    // No cursor to decode: an export is not a page, and the spec it runs on
    // is the whole selection.
    let spec = selection.unpaged();
    let job = super::super::super::core::export::start(
        state,
        spec,
        super::super::super::runs::known_fields,
    )
    .await
    .gql()?;
    Ok(StartRunExportResult {
        export: RunExport::from_job(state, &job),
    })
}
