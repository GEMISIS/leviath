//! Tests for [`super`]: which deadline an operation gets, and what it is
//! answered with past it.

use super::*;

use async_graphql::{EmptySubscription, Object, Request, Schema};

/// Two seconds of (virtual) time.
async fn two_seconds() -> bool {
    tokio::time::sleep(Duration::from_secs(2)).await;
    true
}

struct Query;

#[Object]
impl Query {
    async fn slow(&self) -> bool {
        two_seconds().await
    }
}

struct Mutation;

#[Object]
impl Mutation {
    async fn spawn_run(&self) -> bool {
        two_seconds().await
    }

    async fn validate_spawn(&self) -> bool {
        two_seconds().await
    }

    async fn other(&self) -> bool {
        two_seconds().await
    }
}

/// A schema whose operations each take two seconds, under these deadlines.
fn schema(ordinary: u64, spawn: u64) -> Schema<Query, Mutation, EmptySubscription> {
    Schema::build(Query, Mutation, EmptySubscription)
        .extension(Deadlines { ordinary, spawn })
        .finish()
}

/// The `code` of `response`'s first error, when it has one.
fn code(response: &Response) -> Option<String> {
    let error = response.errors.first()?;
    let code = error.extensions.as_ref()?.get("code")?;
    Some(code.to_string().trim_matches('"').to_string())
}

/// An ordinary query past the ordinary deadline is a timeout the HTTP
/// handler answers 408 with; a spawn on the same server outlives it.
#[tokio::test(start_paused = true)]
async fn a_spawn_outlives_the_ordinary_deadline_and_a_query_does_not() {
    let schema = schema(1, 5);
    let query = schema.execute("{ slow }").await;
    assert_eq!(code(&query).as_deref(), Some("REQUEST_TIMEOUT"));
    assert_eq!(
        timed_out(&query).as_deref(),
        Some("request took longer than 1 s")
    );
    let spawned = schema.execute("mutation { spawnRun }").await;
    assert!(spawned.errors.is_empty(), "{:?}", spawned.errors);
    assert_eq!(timed_out(&spawned), None);
    let checked = schema.execute("mutation { validateSpawn }").await;
    assert!(checked.errors.is_empty(), "{:?}", checked.errors);
    // Any other mutation is an ordinary operation.
    let other = schema.execute("mutation { other }").await;
    assert_eq!(code(&other).as_deref(), Some("REQUEST_TIMEOUT"));
}

/// A spawn past its own deadline says its outcome is unknown, as an error
/// rather than a 408; a dry run past it is an ordinary timeout.
#[tokio::test(start_paused = true)]
async fn a_spawn_past_its_own_deadline_says_its_outcome_is_unknown() {
    let schema = schema(30, 1);
    let spawned = schema
        .execute("mutation { spawnRun validateSpawn __typename }")
        .await;
    assert_eq!(code(&spawned).as_deref(), Some(SPAWN_OUTCOME_UNKNOWN));
    assert!(
        spawned.errors[0]
            .message
            .contains("delivery.idempotencyKey"),
        "{}",
        spawned.errors[0].message
    );
    assert_eq!(timed_out(&spawned), None);
    let checked = schema.execute("mutation { validateSpawn }").await;
    assert_eq!(
        timed_out(&checked).as_deref(),
        Some("request took longer than 1 s")
    );
}

/// A root field reached through a fragment counts, and with several
/// operations the one asked for decides.
#[tokio::test(start_paused = true)]
async fn the_root_fields_are_read_through_fragments_and_the_named_operation() {
    let schema = schema(1, 30);
    let spread = schema
        .execute("mutation { ...F } fragment F on Mutation { spawnRun }")
        .await;
    assert!(spread.errors.is_empty(), "{:?}", spread.errors);
    let inline = schema
        .execute("mutation { ... on Mutation { validateSpawn } }")
        .await;
    assert!(inline.errors.is_empty(), "{:?}", inline.errors);
    let document = "mutation Start { spawnRun } query Look { slow }";
    let start = schema
        .execute(Request::new(document).operation_name("Start"))
        .await;
    assert!(start.errors.is_empty(), "{:?}", start.errors);
    let look = schema
        .execute(Request::new(document).operation_name("Look"))
        .await;
    assert_eq!(code(&look).as_deref(), Some("REQUEST_TIMEOUT"));
}

/// A deadline of `0` is none, for either kind.
#[tokio::test(start_paused = true)]
async fn a_deadline_of_zero_waits_for_the_operation() {
    let schema = schema(0, 0);
    assert!(schema.execute("{ slow }").await.errors.is_empty());
    assert!(
        schema
            .execute("mutation { spawnRun }")
            .await
            .errors
            .is_empty()
    );
}

/// The HTTP handler answers a timeout 408 in the REST error shape, and any
/// other response as GraphQL does, 200 with its errors.
#[tokio::test(start_paused = true)]
async fn the_handler_answers_a_timeout_408_and_a_spawns_as_graphql() {
    let schema = schema(1, 1);
    let late = super::super::answer(schema.execute("{ slow }").await);
    assert_eq!(late.status(), axum::http::StatusCode::REQUEST_TIMEOUT);
    let body = axum::body::to_bytes(late.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["error"], "request took longer than 1 s");
    let spawn = super::super::answer(schema.execute("mutation { spawnRun }").await);
    assert_eq!(spawn.status(), axum::http::StatusCode::OK);
    let body = axum::body::to_bytes(spawn.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        body["errors"][0]["extensions"]["code"],
        SPAWN_OUTCOME_UNKNOWN
    );
    assert_eq!(body["errors"][0]["extensions"]["httpStatus"], 408);
}

/// Only a timeout is one: an answer with no errors, or with errors of
/// another kind, is not.
#[tokio::test]
async fn only_a_timeout_reads_as_one() {
    assert_eq!(timed_out(&Response::new(Value::Null)), None);
    let refused = schema(30, 330).execute("{ nonesuch }").await;
    assert!(!refused.errors.is_empty());
    assert_eq!(timed_out(&refused), None);
    let bare = Response::from_errors(vec![ServerError::new("no code", None)]);
    assert_eq!(timed_out(&bare), None);
    let limits = RequestLimits {
        max_concurrent_requests: 1,
        request_timeout_secs: 7,
        spawn_timeout_secs: 9,
        max_upload_bytes: 1,
    };
    let of = Deadlines::of(&limits);
    assert_eq!((of.ordinary, of.spawn), (7, 9));
}

/// `POST /graphql` through the real route: a body still arriving after the
/// ordinary deadline is a 408, one that is not a GraphQL request is refused,
/// and a sound one is answered.
#[tokio::test(start_paused = true)]
async fn the_route_reads_its_body_under_the_ordinary_deadline() {
    use axum::body::Body;
    use axum::http::{Request as Http, StatusCode};
    use tower::ServiceExt;

    let mut state = crate::commands::serve::testutil::state_with_agent_paths(Vec::new());
    state.limits = Arc::new(crate::commands::serve::types::ServeLimits {
        request_limits: RequestLimits {
            request_timeout_secs: 1,
            ..Default::default()
        },
        ..Default::default()
    });
    let app = || {
        crate::commands::serve::api_router()
            .layer(axum::extract::Extension(super::super::build_schema(
                state.clone(),
                false,
            )))
            .with_state(state.clone())
    };
    let post = |body: Body| {
        Http::builder()
            .method("POST")
            .uri("/graphql")
            .header("content-type", "application/json")
            .body(body)
            .unwrap()
    };
    let stuck = futures_util::stream::pending::<Result<axum::body::Bytes, std::io::Error>>();
    let late = app().oneshot(post(Body::from_stream(stuck))).await.unwrap();
    assert_eq!(late.status(), StatusCode::REQUEST_TIMEOUT);
    let bad = app().oneshot(post(Body::from("not json"))).await.unwrap();
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    let sound = app()
        .oneshot(post(Body::from(r#"{"query": "{ __typename }"}"#)))
        .await
        .unwrap();
    assert_eq!(sound.status(), StatusCode::OK);
}

/// The spawn deadline is for spawns alone: a slow field beside one runs
/// under the ordinary deadline, unless `@skip` or `@include` leaves it out,
/// and a spawn a directive leaves out buys nothing.
#[tokio::test(start_paused = true)]
async fn a_spawn_beside_another_field_gets_the_ordinary_deadline() {
    let schema = schema(1, 30);
    let beside = schema.execute("mutation { validateSpawn other }").await;
    assert_eq!(code(&beside).as_deref(), Some("REQUEST_TIMEOUT"));
    let skipped_spawn = schema
        .execute("mutation { spawnRun @skip(if: true) __typename }")
        .await;
    assert!(skipped_spawn.errors.is_empty(), "nothing slow runs");
    let skipped_other = schema
        .execute("mutation { spawnRun other @skip(if: true) }")
        .await;
    assert!(
        skipped_other.errors.is_empty(),
        "{:?}",
        skipped_other.errors
    );
    let gated = "mutation ($on: Boolean!) { spawnRun other @include(if: $on) }";
    let off = schema
        .execute(
            Request::new(gated).variables(Variables::from_json(serde_json::json!({ "on": false }))),
        )
        .await;
    assert!(off.errors.is_empty(), "{:?}", off.errors);
    let on = schema
        .execute(
            Request::new(gated).variables(Variables::from_json(serde_json::json!({ "on": true }))),
        )
        .await;
    assert_eq!(code(&on).as_deref(), Some("REQUEST_TIMEOUT"));
    let spread = schema
        .execute("mutation { spawnRun ...F @skip(if: true) } fragment F on Mutation { other }")
        .await;
    assert!(spread.errors.is_empty(), "{:?}", spread.errors);
    let inline = schema
        .execute("mutation { spawnRun ... on Mutation @include(if: false) { other } }")
        .await;
    assert!(inline.errors.is_empty(), "{:?}", inline.errors);
}
