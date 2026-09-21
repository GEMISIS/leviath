//! The `blueprints` field: the catalogue installed on this machine, keyset
//! paged on the name it is read in.

use std::sync::Arc;

use async_graphql::Context;

use super::super::super::blocking::blocking;
use super::super::super::core::blueprints;
use super::super::super::core::error::ServeError;
use super::super::super::cursor::{self, CursorKey};
use super::super::super::types::AppState;
use super::super::blueprint_filter::BlueprintFilter;
use super::super::connection::PageInfo;
use super::super::error::IntoGraphql;
use super::super::run_filter::page_size;
use super::super::scalars::Cursor;
use super::super::types::blueprint::Blueprint;

/// What a blueprint cursor says it was minted for.
///
/// The catalogue has one order - by name, ascending - so these are constants
/// rather than arguments. They travel in the cursor all the same, because that
/// is what lets a cursor from another listing be refused rather than resume
/// this one.
const BLUEPRINT_SORT: &str = "name";
/// The one direction the blueprint catalogue is read in.
const BLUEPRINT_ORDER: &str = "asc";

/// One installed blueprint with its page cursor.
#[derive(async_graphql::SimpleObject)]
pub(crate) struct BlueprintEdge {
    /// The blueprint.
    pub(crate) node: Blueprint,
    /// Cursor for this edge.
    pub(crate) cursor: Cursor,
}

/// A paged listing of the installed blueprints.
#[derive(async_graphql::SimpleObject)]
pub(crate) struct BlueprintConnection {
    /// The blueprints on this page.
    pub(crate) edges: Vec<BlueprintEdge>,
    /// Keyset page state.
    pub(crate) page_info: PageInfo,
    /// How many blueprints are installed.
    pub(crate) total: i32,
    /// Names from an `exact` fetch that are not installed. Never fails the
    /// request.
    pub(crate) missing: Vec<String>,
}

/// The blueprints installed on this machine, by name.
///
/// This is the live definition, not what any run executed: for that, read
/// `blueprint` on the run, which answers from the run's own snapshot. The
/// digests tell you whether the two are the same bytes.
///
/// `filter.names` fetches blueprints by name. A name that is not installed
/// lands in `missing` rather than failing the request.
///
/// Keyset-paged on the name, which is the order the catalogue is read in.
/// A cursor names where you got to, so a blueprint installed or removed
/// mid-walk cannot make a page skip or repeat one.
pub(crate) async fn blueprints(
    ctx: &Context<'_>,
    filter: Option<BlueprintFilter>,
    first: i32,
    after: Option<Cursor>,
) -> async_graphql::Result<BlueprintConnection> {
    let state = ctx.data_unchecked::<AppState>();
    let limit = page_size(first).gql()?;
    let filter = filter.unwrap_or_default();
    let named = filter.exact_names();
    let matcher = filter.compiled();
    // A cursor is refused where it was minted for a different filter set,
    // rather than silently resuming a walk of something else. An empty
    // filter contributes nothing, so a cursor from an unfiltered walk
    // stays usable across a release that adds a field.
    let digest = cursor::filter_digest(&[match matcher.is_empty() {
        true => String::new(),
        false => matcher.digest_part(),
    }
    .as_str()]);
    let resume = match after {
        None => None,
        Some(ref token) => Some(
            cursor::decode(&token.0, BLUEPRINT_SORT, BLUEPRINT_ORDER, &digest)
                .map_err(|e| ServeError::BadRequest(e.message()))
                .gql()?,
        ),
    };

    let config = state.current_config();
    // The walk over every agent directory belongs on the blocking pool,
    // and the roots are resolved first so a test's agents-dir override is
    // visible from the task that resolves them.
    let roots = super::super::super::blueprints::blueprint_roots(&config);
    let installed = blocking(move || super::super::super::blueprints::discover_in(roots)).await;

    // A name nothing is installed under is reported, not thrown: one dead
    // name in a batch of fifty must not cost the other forty-nine.
    let missing: Vec<String> = named
        .into_iter()
        .flatten()
        .filter(|name| !installed.iter().any(|info| &info.name == name))
        .collect();
    // Discovery returns the catalogue name-sorted and deduplicated, so the
    // keyset walk below is over a total order with no sort of its own.
    let chosen: Vec<_> = installed
        .iter()
        .filter(|info| matcher.matches(info))
        .cloned()
        .collect();

    let total = i32::try_from(chosen.len()).unwrap_or(i32::MAX);
    let after_cursor: Vec<_> = chosen
        .into_iter()
        .filter(|info| match resume {
            None => true,
            Some(ref cursor) => {
                cursor.precedes(&CursorKey::Text(info.name.clone()), &info.name, false)
            }
        })
        .collect();
    let has_next_page = after_cursor.len() > limit;
    let mut edges = Vec::with_capacity(limit.min(after_cursor.len()));
    for info in after_cursor.into_iter().take(limit) {
        // The parse came with the listing row, so there is no second parse
        // here and no failure path: a row exists only because its manifest
        // parsed.
        let manifest = blueprints::ManifestText::installed(info.manifest.clone());
        edges.push(BlueprintEdge {
            node: Blueprint {
                parsed: Arc::clone(&info.parsed),
                digest: manifest.digest,
                source: manifest.source.into(),
            },
            cursor: Cursor(cursor::encode(
                BLUEPRINT_SORT,
                BLUEPRINT_ORDER,
                &digest,
                CursorKey::Text(info.name.clone()),
                &info.name,
            )),
        });
    }
    // Only when another page is known to exist: emitting one speculatively
    // makes a client's "loop until null" run one empty request longer.
    let end_cursor = has_next_page
        .then(|| edges.last())
        .flatten()
        .map(|edge| edge.cursor.clone());

    Ok(BlueprintConnection {
        edges,
        page_info: PageInfo {
            has_next_page,
            end_cursor,
        },
        total,
        missing,
    })
}
