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

use async_graphql::http::ALL_WEBSOCKET_PROTOCOLS;
use async_graphql::{SDLExportOptions, Schema};
use async_graphql_axum::rejection::GraphQLRejection;
use async_graphql_axum::{GraphQLProtocol, GraphQLRequest, GraphQLResponse};
use axum::extract::{Extension, FromRequest, State, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};

use super::admin_gate::{AdminGate, Presented};
use super::types::AppState;

mod admin;
mod checks;
mod config_input;
mod connection;
mod deadline;
mod error;
mod events;
mod filter;
mod inputs;
mod mutation;
mod node;
mod ordering;
pub(super) mod paging;
mod query;
mod scalars;
mod script_ref;
mod socket;
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
pub(super) fn build_schema(state: AppState, admin: AdminGate) -> LeviathSchema {
    let state_limits = state.limits.request_limits;
    Schema::build(
        query::Query,
        mutation::Mutation::default(),
        subscription::Subscription_,
    )
    .data(state)
    // The two frame interfaces. Nothing returns either of them - a
    // subscription yields a union, so a client can tell a domain frame from a
    // transport one - so they are registered by name or they would not reach
    // the schema at all.
    .register_output_type::<events::Event>()
    .register_output_type::<events::RunEvent>()
    // How this server offers the acts that change the machine. Decided once,
    // here: the REST side answers 404 for them by not mounting the route at
    // all, and a schema has no "unmounted", so this is what stands in for it.
    // Whether one request may use them is the gate's answer to what that
    // request presented, which `http` and `ws` put beside it.
    .data(admin)
    // What gives a parse or validation refusal the same `extensions.code` a
    // resolver's failure carries. Registered here and not in `sdl()`, because
    // an extension is a hook around execution and contributes nothing to the
    // type registry the SDL is printed from.
    .extension(error::CodeEveryRefusal)
    // The response answers fields in the order the query selected them, as
    // the spec says, whichever field resolved first.
    .extension(ordering::AnswerInSelectionOrder)
    // The deadline, which a path cannot pick for this route: a spawn's is
    // minutes, any other operation's is the ordinary one.
    .extension(deadline::Deadlines::of(&state_limits))
    .limit_depth(MAX_DEPTH)
    .limit_complexity(MAX_COMPLEXITY)
    .finish()
}

/// `POST /graphql`: queries and mutations.
///
/// Answers 200 with an `errors` array for a failure inside a field, which is
/// what makes a partial answer possible: one unreadable run does not cost a
/// client the other forty-nine on the page. Transport-level refusals still
/// come from the layers around this one, so a missing token is a 401. An
/// operation past its deadline is a 408 exactly as on every REST route,
/// except a `spawnRun`, whose run may have started anyway: that is an error
/// coded `SPAWN_OUTCOME_UNKNOWN` (see `deadline`).
///
/// The request-limit layer leaves this route's deadline to the handler, so
/// the body is read here under the ordinary deadline: a request still
/// arriving after it has no business holding a slot.
pub(super) async fn http(
    State(state): State<AppState>,
    Extension(schema): Extension<LeviathSchema>,
    req: axum::extract::Request,
) -> Response {
    let limits = state.limits.request_limits;
    // What the request presented, kept for the admin guard: by the time a
    // mutation runs, the HTTP request is long gone.
    let presented = Presented::of(req.headers(), req.uri());
    let read = GraphQLRequest::<GraphQLRejection>::from_request(req, &state);
    match super::request_limits::within(limits.timeout(), read).await {
        Some(Ok(request)) => answer(schema.execute(request.into_inner().data(presented)).await),
        Some(Err(refused)) => refused.into_response(),
        None => super::types::err(
            StatusCode::REQUEST_TIMEOUT,
            format!("request took longer than {} s", limits.request_timeout_secs),
        )
        .into_response(),
    }
}

/// The HTTP answer to `response`: a 408 for an operation past its deadline,
/// as every REST route answers one, and the GraphQL response otherwise.
fn answer(response: async_graphql::Response) -> Response {
    match deadline::timed_out(&response) {
        Some(message) => super::types::err(StatusCode::REQUEST_TIMEOUT, message).into_response(),
        None => GraphQLResponse::from(response).into_response(),
    }
}

/// `GET /ws/graphql`: subscriptions over `graphql-transport-ws`.
///
/// Mounted under `/ws/` deliberately. That prefix is what the auth layer
/// accepts a `?token=` on, because a browser cannot put a header on a
/// WebSocket, and what the request-limit layer exempts from the per-request
/// deadline, because a subscription is meant to stay open.
///
/// The connection runs under the same keepalive as `/ws`: a ping every 20 s,
/// a close for a peer that has not answered one in 60 s, a 10 s deadline on
/// every frame sent, and a 256 KiB write buffer. When the server begins to
/// stop, the socket is closed with 1001 (going away).
pub(super) async fn ws(
    State(state): State<AppState>,
    Extension(schema): Extension<LeviathSchema>,
    protocol: GraphQLProtocol,
    headers: HeaderMap,
    uri: Uri,
    upgrade: WebSocketUpgrade,
) -> Response {
    // What the upgrade presented, which is what every operation on the socket
    // presents to the admin guard.
    let presented = Presented::of(&headers, &uri);
    // Upgraded by hand rather than with the ready-made service, so this stays a
    // handler in the one route table the spec is held to.
    let shutdown = state.shutdown;
    upgrade
        .protocols(ALL_WEBSOCKET_PROTOCOLS)
        .max_write_buffer_size(super::keepalive::WS_MAX_WRITE_BUFFER)
        .on_upgrade(move |socket| {
            shutdown.clone().track(socket::serve(
                socket,
                schema,
                protocol,
                presented,
                super::keepalive::WS_TIMING,
                shutdown,
            ))
        })
}

/// The SDL for this schema, for the lockstep test and
/// `lev serve --print-graphql-schema`.
pub(super) fn sdl() -> String {
    Schema::build(
        query::Query,
        mutation::Mutation::default(),
        subscription::Subscription_,
    )
    // The two frame interfaces. Nothing returns either of them - a
    // subscription yields a union, so a client can tell a domain frame from a
    // transport one - so they are registered by name or they would not reach
    // the schema at all.
    .register_output_type::<events::Event>()
    .register_output_type::<events::RunEvent>()
    .finish()
    // A description that fits on one line is printed on one line. Most of
    // them do, and the block form costs two fence lines each, which is most
    // of the file's length for no more information.
    .sdl_with_options(SDLExportOptions::new().prefer_single_line_descriptions())
}

#[cfg(test)]
#[path = "docs_examples_tests.rs"]
mod docs_examples;

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
