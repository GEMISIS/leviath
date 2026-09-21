//! The GraphQL surface of `lev serve`.
//!
//! One endpoint, `POST /graphql`, where the request body says which fields it
//! wants; and `GET /ws/graphql`, where a subscription streams the same daemon
//! events `/ws` carries, filtered before they are serialized.
//!
//! It is not a layer over the REST routes. A resolver calls the same
//! `serve::core` function the matching REST handler calls, so neither surface
//! can drift from the other, and a GraphQL request costs one process hop, not
//! two.
//!
//! **Why this is worth having beside REST.** A dashboard drawing a fleet view
//! asks for runs, their costs and whatever is parked waiting on a person. Over
//! REST that is a listing plus one request per waiting run, and the client
//! holds the join. Here it is one request, and the fields nobody selected are
//! never read from disk at all.
//!
//! The schema is code-first: the Rust types in this module *are* the schema,
//! their doc comments are the descriptions clients read, and
//! `docs/schema/leviath.graphql` is generated from them and held in lockstep
//! by a test, the way `openapi.json` is held to the router.

use async_graphql::Schema;
use async_graphql::http::ALL_WEBSOCKET_PROTOCOLS;
use async_graphql_axum::{GraphQLProtocol, GraphQLRequest, GraphQLResponse, GraphQLWebSocket};
use axum::extract::{Extension, WebSocketUpgrade};
use axum::response::Response;

use super::types::AppState;

mod admin;
mod blueprint_filter;
mod checks;
mod config_input;
mod connection;
mod error;
mod events;
mod filters;
mod inputs;
mod mutation;
mod node;
mod query;
mod run_filter;
mod scalars;
mod subscription;
mod types;

/// How deeply one query may nest.
///
/// A run's children are runs, so `children { children { ... } }` is a walk of
/// the sub-agent tree that a client could otherwise ask to continue forever.
/// Twelve is well past what any real view needs and far short of what hurts.
const MAX_DEPTH: usize = 12;

/// How much work one query may ask for, counted before any of it runs.
///
/// Depth alone does not bound breadth: fifty runs each asking for fifty
/// children is shallow and large. The complexity budget is what says no to
/// that, and it says so during validation, before a single file is read.
const MAX_COMPLEXITY: usize = 10_000;

/// This server's schema.
pub(super) type LeviathSchema =
    Schema<query::Query, mutation::Mutation, subscription::Subscription_>;

/// Build the schema for one server.
///
/// Built once at startup and shared: the type registry and the limits are the
/// same for every request, and the per-request state travels in the execution
/// context instead.
pub(super) fn build_schema(state: AppState, allow_admin: bool) -> LeviathSchema {
    Schema::build(
        query::Query,
        mutation::Mutation::default(),
        subscription::Subscription_,
    )
    .data(state)
    // Whether this server was started for the acts that change the machine.
    // Decided once, here, rather than read per request: the REST side answers
    // 404 for them by not mounting the route at all, and a schema has no
    // "unmounted", so this is what stands in for it.
    .data(admin::AdminAccess(allow_admin))
    .limit_depth(MAX_DEPTH)
    .limit_complexity(MAX_COMPLEXITY)
    .finish()
}

/// `POST /graphql`: queries and mutations.
///
/// Answers 200 with an `errors` array for a failure inside a field, which is
/// what makes a partial answer possible: one unreadable run does not cost a
/// client the other forty-nine on the page. Transport-level refusals still
/// come from the layers around this one, so a missing token is a 401 and an
/// over-long request is a 408 exactly as on every REST route.
pub(super) async fn http(
    Extension(schema): Extension<LeviathSchema>,
    req: GraphQLRequest,
) -> GraphQLResponse {
    schema.execute(req.into_inner()).await.into()
}

/// `GET /ws/graphql`: subscriptions over `graphql-transport-ws`.
///
/// Mounted under `/ws/` deliberately. That prefix is what the auth layer
/// accepts a `?token=` on, because a browser cannot put a header on a
/// WebSocket, and what the request-limit layer exempts from the per-request
/// deadline, because a subscription is meant to stay open.
pub(super) async fn ws(
    Extension(schema): Extension<LeviathSchema>,
    protocol: GraphQLProtocol,
    upgrade: WebSocketUpgrade,
) -> Response {
    // Upgraded by hand rather than with the ready-made service, so this stays a
    // handler in the one route table the spec is held to.
    upgrade
        .protocols(ALL_WEBSOCKET_PROTOCOLS)
        .on_upgrade(move |socket| GraphQLWebSocket::new(socket, schema, protocol).serve())
}

/// The SDL for this schema, for the lockstep test and
/// `lev serve --print-graphql-schema`.
pub(super) fn sdl() -> String {
    Schema::build(
        query::Query,
        mutation::Mutation::default(),
        subscription::Subscription_,
    )
    .finish()
    .sdl()
}

#[cfg(test)]
#[path = "docs_examples_tests.rs"]
mod docs_examples;

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
