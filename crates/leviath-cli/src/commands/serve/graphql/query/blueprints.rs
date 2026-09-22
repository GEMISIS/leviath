//! The `blueprints` field: the catalogue installed on this machine, and the
//! one blueprint a name points at.
//!
//! The listing is the shape every listing in this schema takes: a filter that
//! mirrors the type it selects, an order built from the type's own sort keys,
//! and a keyset cursor bound to both. Nothing here decides how a filter
//! matches or how a page is walked; the mirror answers the first, and
//! [`connection`] answers the second for this listing and every other.

use std::sync::Arc;

use super::super::super::blocking::blocking;
use super::super::super::core::blueprints;
use super::super::super::types::AppState;
use super::super::connection::Connection;
use super::super::paging::order::{OrderDirection, Term};
use super::super::scalars::Cursor;
use super::super::types::blueprint::{
    Blueprint, BlueprintFilter, BlueprintOrder, BlueprintOrderField,
};
use super::listing::{Window, connection, terms};
use async_graphql::Context;

/// How many blueprints one page may carry.
///
/// A catalogue is tens of entries, not thousands, so the cap is about a client
/// that meant to page and did not: fifty is the default and two hundred is as
/// far as one request reaches.
const PAGE_CAP: usize = 200;

/// What the cap is called when a request goes over it.
const CAP_NAME: &str = "the blueprint page cap";

/// The order the catalogue is read in when nothing says otherwise.
///
/// By name, ascending: the order discovery already returns, and the order a
/// person reads a catalogue in. Spelling it as a term rather than as a special
/// case is what keeps an unfiltered cursor byte-identical to the REST one.
fn by_name() -> Vec<Term<BlueprintOrderField>> {
    vec![Term {
        field: BlueprintOrderField::Name,
        direction: OrderDirection::Asc,
    }]
}

/// Every blueprint installed on this machine, parsed.
///
/// The walk over every agent directory belongs on the blocking pool, and the
/// roots are resolved first so a test's agents-dir override is visible from
/// the task that resolves them.
async fn installed(state: &AppState) -> Vec<Blueprint> {
    let config = state.current_config();
    let roots = super::super::super::blueprints::blueprint_roots(&config);
    let found = blocking(move || super::super::super::blueprints::discover_in(roots)).await;
    found
        .into_iter()
        .map(|info| {
            // The parse came with the listing row, so there is no second parse
            // here and no failure path: a row exists only because its manifest
            // parsed.
            let manifest = blueprints::ManifestText::installed(info.manifest);
            Blueprint {
                parsed: Arc::clone(&info.parsed),
                digest: manifest.digest,
                source: manifest.source.into(),
            }
        })
        .collect()
}

/// The blueprints installed on this machine.
///
/// This is the live definition, not what any run executed: for that, read
/// `blueprint` on the run, which answers from the run's own snapshot. The
/// digests tell you whether the two are the same bytes.
///
/// Keyset-paged. A cursor names where you got to, so a blueprint installed or
/// removed mid-walk cannot make a page skip or repeat one, and a cursor minted
/// for another filter or another order is refused rather than resuming a walk
/// of something else.
pub(crate) async fn blueprints(
    ctx: &Context<'_>,
    filter: Option<BlueprintFilter>,
    order_by: Option<Vec<BlueprintOrder>>,
    first: i32,
    after: Option<Cursor>,
) -> async_graphql::Result<Connection<Blueprint>> {
    let state = ctx.data_unchecked::<AppState>();
    connection(
        installed(state).await,
        filter,
        terms(order_by, BlueprintOrder::term, by_name),
        |blueprint: &Blueprint| blueprint.parsed.name.clone(),
        Window {
            first,
            after,
            cap: PAGE_CAP,
            cap_name: CAP_NAME,
        },
    )
    .await
}

/// One installed blueprint, by the name it is installed under.
///
/// Null for a name nothing is installed under: a lookup answers "not here"
/// rather than failing, so a client reading several names gets an answer for
/// each of them.
pub(crate) async fn blueprint(ctx: &Context<'_>, name: String) -> Option<Blueprint> {
    let state = ctx.data_unchecked::<AppState>();
    installed(state)
        .await
        .into_iter()
        .find(|found| found.parsed.name == name)
}
