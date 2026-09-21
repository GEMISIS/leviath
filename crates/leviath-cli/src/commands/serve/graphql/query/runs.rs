//! The `runs`, `openInteractions` and `node` fields: the run listing, the
//! daemon's approval inbox, and the one lookup that answers from an id alone.

use async_graphql::Context;

use super::super::super::core::runs as run_core;
use super::super::super::cursor::{self, CursorKey};
use super::super::super::types::AppState;
use super::super::connection::{Highlight, PageInfo, RunConnection, RunEdge};
use super::super::error::IntoGraphql;
use super::super::node::{self, Node};
use super::super::run_filter::RunFilter;
use super::super::scalars::{Cursor, Timestamp};
use super::super::types::run::Run;

/// One open ask, with the run it is parked on.
#[derive(async_graphql::SimpleObject)]
pub(crate) struct OpenInteraction {
    /// The run waiting for this answer.
    pub(crate) run_id: String,
    /// What is being asked.
    pub(crate) request: super::super::events::InteractionRequest,
}

/// Keyset-paged run listing.
///
/// `filter.ids` fetches exact runs, which is also how a client reads one
/// run: `runs(filter: { ids: ["..."] })`. An id that names nothing lands
/// in `missing` rather than failing the request, so one dead id in a batch
/// of fifty does not cost the other forty-nine.
pub(crate) async fn runs(
    ctx: &Context<'_>,
    filter: Option<RunFilter>,
    first: i32,
    after: Option<Cursor>,
) -> async_graphql::Result<RunConnection> {
    let state = ctx.data_unchecked::<AppState>();
    let selection = filter
        .unwrap_or_default()
        .selection(state, first)
        .await
        .gql()?;
    let sort = selection.sort;
    let descending = selection.descending;
    let spec = selection
        .resolve(after.as_ref().map(|c| c.0.as_str()))
        .gql()?;
    let listing = run_core::list(state, &spec).await;

    let has_next_page = listing.next_cursor.is_some();
    let end_cursor = listing.next_cursor.map(Cursor);
    let now = listing.server_time;
    let edges = listing
        .hits
        .into_iter()
        .map(|hit| {
            let cursor = Cursor(cursor::encode(
                sort.as_str(),
                if descending { "desc" } else { "asc" },
                &spec.digest,
                CursorKey::Int(sort.value(&hit.meta)),
                &hit.meta.run_id,
            ));
            RunEdge {
                node: Run {
                    meta: hit.meta,
                    now,
                },
                cursor,
                highlights: hit
                    .highlights
                    .into_iter()
                    .map(|h| Highlight {
                        field: h.field,
                        snippet: h.snippet,
                        stage: h.stage.and_then(|s| i32::try_from(s).ok()),
                    })
                    .collect(),
            }
        })
        .collect();

    Ok(RunConnection {
        edges,
        page_info: PageInfo {
            has_next_page,
            end_cursor,
        },
        total: listing.total.and_then(|t| i32::try_from(t).ok()),
        scan_truncated: listing.scan_truncated,
        missing: listing.missing,
        server_time: Timestamp(now),
    })
}

/// Every open ask across every run: the approval inbox.
///
/// The daemon holds these in memory, so this is one read rather than a walk
/// of the run store. Each entry names the run it is parked on, which is
/// what a client needs to show the row it belongs to.
pub(crate) async fn open_interactions(
    ctx: &Context<'_>,
) -> async_graphql::Result<Vec<OpenInteraction>> {
    let state = ctx.data_unchecked::<AppState>();
    let open = super::super::super::core::spawn::open_interactions(state)
        .await
        .gql()?;
    Ok(open
        .into_iter()
        .map(|(run_id, request)| OpenInteraction {
            run_id,
            request: request.into(),
        })
        .collect())
}

/// Anything with a globally unique id, from that id alone.
///
/// For a client that holds an id and no type: a webhook payload, a cache
/// key, a link somebody pasted. Ask for the fields on `Node` and narrow
/// with `... on Run { }` for the rest.
///
/// How an id routes, in order. An id tagged `mcpServer:`, `yoloProfile:` or
/// `script:` names that kind of thing; a tag this server does not know
/// answers null. An id carrying an `@` is a blueprint revision. Anything
/// else is a minted id, and the two job registries are asked before the run
/// store, which is the only one of the three that reads a file.
///
/// Null rather than an error for an id that names nothing: a deleted run, an
/// expired export and a typo are the same answer, and all three mean the
/// thing is not here. A read that could not answer the question at all, such
/// as a config file that will not parse, fails the way the listing it would
/// have come from fails.
///
/// `Model` is not a `Node`, because a model id is the provider's own and two
/// providers can serve the same one; read `models` and key on provider and
/// id together.
pub(crate) async fn node(
    ctx: &Context<'_>,
    id: async_graphql::ID,
) -> async_graphql::Result<Option<Node>> {
    node::resolve(ctx, id.as_str()).await
}
