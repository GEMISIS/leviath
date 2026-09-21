//! The `bulkExportRuns` field: starting a whole-store export a client polls
//! rather than pages through.

use async_graphql::Context;

use super::super::super::types::AppState;
use super::super::error::IntoGraphql;
use super::super::query::BulkExport;
use super::super::run_filter::RunFilter;

/// Export the run store to a file, and hand back the job.
///
/// Paging ten thousand runs through a connection is two hundred requests,
/// and a client that wants everything wants it once. This returns
/// immediately; poll `bulkExport(id:)` and fetch `downloadUrl` when it is
/// complete.
///
/// The filter is the run listing's own, so a client builds the predicate
/// once and uses it for both. `fields` narrows each row, and an unknown name
/// is refused rather than dropped: a column quietly missing from an export
/// is discovered downstream, by somebody else.
pub(crate) async fn bulk_export_runs(
    ctx: &Context<'_>,
    filter: Option<RunFilter>,
    fields: Option<Vec<String>>,
) -> async_graphql::Result<BulkExport> {
    let state = ctx.data_unchecked::<AppState>();
    // An export is not a page, so the page cap does not apply: the whole
    // point is everything at once. The listing's own scan bounds still do.
    let mut selection = filter.unwrap_or_default().everything(state).await.gql()?;
    selection.fields = fields.map(|named| named.into_iter().collect());
    // No cursor to decode: an export is not a page, so the one failure
    // `resolve` has here cannot happen.
    let spec = selection
        .resolve(None)
        .expect("an unpaged selection has no cursor to refuse");
    let job = super::super::super::core::export::start(
        state,
        spec,
        super::super::super::runs::known_fields,
    )
    .await
    .gql()?;
    Ok(BulkExport::from_job(state, &job))
}
