//! How long a GraphQL request may take, by what it asks for.
//!
//! Every operation arrives on `POST /graphql`, so the request-limit layer
//! cannot tell a spawn from a listing by its path, and leaves the deadline
//! for that route to this extension. Once the operation is parsed it picks
//! one: `[serve] spawn_timeout_secs` when the operation's root fields include
//! `spawnRun` or `validateSpawn`, and `request_timeout_secs` otherwise. For
//! a query or a mutation the clock starts when the request does. The
//! in-flight cap is still the layer's.
//!
//! A subscription is not a request with a deadline: it stays open for as
//! long as its client wants, which is why `GET /ws/graphql` is outside the
//! layer. What each of its events does is bounded all the same, the way every
//! read this server makes is: async-graphql runs one execution per event, and
//! each gets `request_timeout_secs` on a clock of its own that starts when
//! that event's execution does. An event whose resolution is stuck past it is
//! answered with a `REQUEST_TIMEOUT` error in its place, and the subscription
//! goes on to the next one.
//!
//! A request past its deadline is answered the way every route answers one:
//! 408 with `{"error": ...}` (see [`timed_out`]). A `spawnRun` is the
//! exception. The daemon may have started its run after this server stopped
//! waiting, so it answers with a GraphQL error coded `SPAWN_OUTCOME_UNKNOWN`,
//! which tells the client to send it again under the same
//! `delivery.idempotencyKey` rather than guess.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_graphql::extensions::{
    Extension, ExtensionContext, ExtensionFactory, NextExecute, NextParseQuery,
};
use async_graphql::parser::Positioned;
use async_graphql::parser::types::{
    Directive, ExecutableDocument, OperationType, Selection, SelectionSet,
};
use async_graphql::{ErrorExtensionValues, Response, ServerError, ServerResult, Value, Variables};

use super::super::request_limits::RequestLimits;
use super::ordering::{Fragments, operation_of};

/// The code on a `spawnRun` that ran past its deadline.
pub(crate) const SPAWN_OUTCOME_UNKNOWN: &str = "SPAWN_OUTCOME_UNKNOWN";

/// The code on any other request that ran past its deadline, which
/// [`timed_out`] turns into a 408.
const REQUEST_TIMEOUT: &str = "REQUEST_TIMEOUT";

/// Registers [`Deadline`] on the schema, with the two deadlines in seconds.
/// `0` is no deadline.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Deadlines {
    /// `request_timeout_secs`.
    ordinary: u64,
    /// `spawn_timeout_secs`.
    spawn: u64,
}

impl Deadlines {
    /// The deadlines `limits` sets.
    pub(crate) fn of(limits: &RequestLimits) -> Self {
        Self {
            ordinary: limits.request_timeout_secs,
            spawn: limits.spawn_timeout_secs,
        }
    }
}

impl ExtensionFactory for Deadlines {
    fn create(&self) -> Arc<dyn Extension> {
        Arc::new(Deadline {
            seconds: *self,
            started: tokio::time::Instant::now(),
            parsed: Mutex::new(None),
            chosen: Mutex::new(None),
        })
    }
}

/// One request's deadline. Made as the request starts, so the clock covers
/// the parse as well as the execution.
struct Deadline {
    /// The deadlines to choose from.
    seconds: Deadlines,
    /// When the request started.
    started: tokio::time::Instant,
    /// The document the parser produced and the variables it came with,
    /// kept until the operation runs.
    parsed: Mutex<Option<(ExecutableDocument, Variables)>>,
    /// How the operation was read when it first ran, kept for a
    /// subscription's later events, which run again with the document gone.
    chosen: Mutex<Option<Choice>>,
}

/// What an operation's deadline is chosen by.
#[derive(Clone, Copy)]
struct Choice {
    /// What it asks for, which picks the number of seconds.
    asks: Asks,
    /// Whether it is a subscription, whose every event runs on a clock of
    /// its own rather than on the request's.
    per_event: bool,
}

/// What an operation asks for, as far as its deadline goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Asks {
    /// A `spawnRun`, alone or beside `validateSpawn` and `__typename`.
    Spawn,
    /// A `validateSpawn`, alone or beside `__typename`.
    Check,
    /// Anything else, including a spawn beside any other field: the spawn
    /// deadline is for spawns, and a slow field beside one does not get it.
    Other,
}

#[async_trait::async_trait]
impl Extension for Deadline {
    async fn parse_query(
        &self,
        ctx: &ExtensionContext<'_>,
        query: &str,
        variables: &Variables,
        next: NextParseQuery<'_>,
    ) -> ServerResult<ExecutableDocument> {
        let document = next.run(ctx, query, variables).await?;
        *leviath_core::sync::lock(&self.parsed) = Some((document.clone(), variables.clone()));
        Ok(document)
    }

    async fn execute(
        &self,
        ctx: &ExtensionContext<'_>,
        operation_name: Option<&str>,
        next: NextExecute<'_>,
    ) -> Response {
        let Choice { asks, per_event } = *leviath_core::sync::lock(&self.chosen)
            .get_or_insert_with(|| {
                leviath_core::sync::lock(&self.parsed).take().map_or(
                    Choice {
                        asks: Asks::Other,
                        per_event: false,
                    },
                    |(document, variables)| choose(&document, operation_name, &variables),
                )
            });
        let secs = match asks {
            Asks::Other => self.seconds.ordinary,
            Asks::Spawn | Asks::Check => self.seconds.spawn,
        };
        if secs == 0 {
            return next.run(ctx, operation_name).await;
        }
        let started = match per_event {
            true => tokio::time::Instant::now(),
            false => self.started,
        };
        let deadline = started + Duration::from_secs(secs);
        match tokio::time::timeout_at(deadline, next.run(ctx, operation_name)).await {
            Ok(response) => response,
            Err(_) => Response::from_errors(vec![late(asks, secs)]),
        }
    }
}

/// How the operation `name` in `document` is timed under `variables`.
fn choose(document: &ExecutableDocument, name: Option<&str>, variables: &Variables) -> Choice {
    Choice {
        asks: asks(document, name, variables),
        per_event: operation_of(document, name)
            .is_some_and(|operation| operation.node.ty == OperationType::Subscription),
    }
}

/// What the operation `name` in `document` asks for, by the root fields it
/// runs with `variables`: a field `@skip` or `@include` leaves out is not
/// asked for.
fn asks(document: &ExecutableDocument, name: Option<&str>, variables: &Variables) -> Asks {
    let mut fields = Vec::new();
    // A request that reached execution names one operation the document
    // holds, so there is always one here.
    operation_of(document, name).iter().for_each(|operation| {
        root_fields(
            &operation.node.selection_set.node,
            (&document.fragments, variables),
            &mut fields,
        )
    });
    let spawns_only = fields
        .iter()
        .all(|field| matches!(*field, "spawnRun" | "validateSpawn" | "__typename"));
    match (
        spawns_only,
        fields.contains(&"spawnRun"),
        fields.contains(&"validateSpawn"),
    ) {
        (true, true, _) => Asks::Spawn,
        (true, false, true) => Asks::Check,
        _ => Asks::Other,
    }
}

/// Every field `selection` runs at its own level, through the fragments it
/// spreads there, leaving out what a directive skips under the variables.
fn root_fields<'a>(
    selection: &'a SelectionSet,
    (fragments, variables): (&'a Fragments, &Variables),
    out: &mut Vec<&'a str>,
) {
    for item in &selection.items {
        match &item.node {
            Selection::Field(field) if included(&field.node.directives, variables) => {
                out.push(field.node.name.node.as_str())
            }
            // A validated document defines every fragment it spreads.
            Selection::FragmentSpread(spread) if included(&spread.node.directives, variables) => {
                fragments
                    .get(&spread.node.fragment_name.node)
                    .iter()
                    .for_each(|fragment| {
                        root_fields(
                            &fragment.node.selection_set.node,
                            (fragments, variables),
                            out,
                        )
                    })
            }
            Selection::InlineFragment(inline) if included(&inline.node.directives, variables) => {
                root_fields(&inline.node.selection_set.node, (fragments, variables), out);
            }
            _ => {}
        }
    }
}

/// Whether a selection carrying `directives` runs under `variables`. A
/// validated document puts only `@skip` and `@include` on a selection. A
/// condition naming a variable left unset reads as `false`.
fn included(directives: &[Positioned<Directive>], variables: &Variables) -> bool {
    directives.iter().all(|directive| {
        let holds = directive.node.get_argument("if").is_some_and(|condition| {
            condition
                .node
                .clone()
                .into_const_with(|name| variables.get(&name).cloned().ok_or(()))
                == Ok(Value::Boolean(true))
        });
        match directive.node.name.node.as_str() == "skip" {
            true => !holds,
            false => holds,
        }
    })
}

/// The error for an operation that asked for `asks` and ran past its
/// deadline of `secs`.
fn late(asks: Asks, secs: u64) -> ServerError {
    let (code, message) = match asks {
        Asks::Spawn => (
            SPAWN_OUTCOME_UNKNOWN,
            format!(
                "spawnRun took longer than {secs} s, so whether its run started is not known; \
                 send it again with the same delivery.idempotencyKey to find out without \
                 starting it twice"
            ),
        ),
        Asks::Check | Asks::Other => (
            REQUEST_TIMEOUT,
            format!("request took longer than {secs} s"),
        ),
    };
    let mut extensions = ErrorExtensionValues::default();
    extensions.set("code", code);
    extensions.set("httpStatus", 408);
    let mut error = ServerError::new(message, None);
    error.extensions = Some(extensions);
    error
}

/// The message of `response`'s timeout, when it is a request other than a
/// `spawnRun` that ran past its deadline: what the HTTP handler answers 408
/// with, as every other route does.
pub(crate) fn timed_out(response: &Response) -> Option<String> {
    let code = Value::String(REQUEST_TIMEOUT.to_string());
    response
        .errors
        .iter()
        .find(|error| {
            error
                .extensions
                .as_ref()
                .and_then(|extensions| extensions.get("code"))
                == Some(&code)
        })
        .map(|error| error.message.clone())
}

#[cfg(test)]
#[path = "deadline_tests.rs"]
mod tests;
