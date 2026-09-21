//! Tests for the live frames: what a subscription keeps, what it drops, and
//! what it says when it falls behind.

use async_graphql::{Request, Schema, Value, futures_util::StreamExt};

use super::Subscription_;
use crate::commands::serve::events::ServerEvent;
use crate::commands::serve::graphql::mutation::Mutation;
use crate::commands::serve::graphql::query::Query;

/// A status frame for one run.
fn status(run_id: &str) -> ServerEvent {
    ServerEvent::AgentStatus {
        agent_id: format!("agent-{run_id}"),
        run_id: run_id.to_string(),
        status: "running".to_string(),
        stage: "build".to_string(),
        iteration: 1,
        tool_calls: 2,
        accepts_messages: true,
        wait_reason: None,
        title: None,
    }
}

/// A log frame for one run.
fn log(run_id: &str, line: &str) -> ServerEvent {
    ServerEvent::Log {
        agent_id: format!("agent-{run_id}"),
        run_id: run_id.to_string(),
        line: line.to_string(),
    }
}

/// A spawn frame, optionally naming the run that started it.
fn spawned(run_id: &str, parent: Option<&str>) -> ServerEvent {
    ServerEvent::AgentSpawned {
        agent_id: format!("agent-{run_id}"),
        run_id: run_id.to_string(),
        parent_id: parent.map(str::to_string),
        blueprint: "coder".to_string(),
    }
}

/// Run one subscription, send the given frames, and collect what arrives.
///
/// The subscription is started before anything is sent, which is also the
/// server's own order: subscribe, then let frames flow.
async fn frames(query: &str, events: Vec<ServerEvent>, want: usize) -> Vec<serde_json::Value> {
    let state = crate::commands::serve::testutil::state_with_agent_paths(Vec::new());
    let tx = state.event_tx.clone();
    let schema = Schema::build(Query, Mutation::default(), Subscription_)
        .data(state)
        .finish();

    let mut stream = schema.execute_stream(Request::new(query));
    // The first poll is what registers the receiver on the broadcast, and it is
    // driven by the collector task below before anything is sent.
    let collector = tokio::spawn(async move {
        let mut out = Vec::new();
        while out.len() < want {
            match stream.next().await {
                Some(response) => {
                    assert!(response.errors.is_empty(), "{:?}", response.errors);
                    out.push(serde_json::to_value(&response.data).expect("data serializes"));
                }
                None => break,
            }
        }
        out
    });

    // A frame sent before the receiver exists is not delivered, which is what
    // this wait is for: the daemon's own broadcast behaves the same way.
    for _ in 0..100 {
        if tx.receiver_count() > 0 {
            break;
        }
        tokio::task::yield_now().await;
    }
    for event in events {
        let _ = tx.send(event);
    }
    tokio::time::timeout(std::time::Duration::from_secs(5), collector)
        .await
        .expect("the frames arrive")
        .expect("the collector finishes")
}

/// Every frame arrives when nothing is filtered, and each one arrives as its
/// own type in the union.
#[tokio::test]
async fn an_unscoped_subscription_sees_every_frame() {
    let out = frames(
        "subscription { events { __typename
            ... on RunStatusChanged { runId status iteration toolCalls acceptsMessages }
            ... on LogLine { runId line } } }",
        vec![status("run-a"), log("run-b", "hello")],
        2,
    )
    .await;
    assert_eq!(out[0]["events"]["__typename"], "RunStatusChanged");
    assert_eq!(out[0]["events"]["runId"], "run-a");
    assert_eq!(out[0]["events"]["status"], "running");
    assert_eq!(out[0]["events"]["toolCalls"], 2);
    assert_eq!(out[1]["events"]["__typename"], "LogLine");
    assert_eq!(out[1]["events"]["line"], "hello");
}

/// A type filter drops what it did not ask for, before the frame is converted.
#[tokio::test]
async fn a_type_filter_keeps_only_what_it_asked_for() {
    let out = frames(
        "subscription { events(types: [LOG]) { __typename ... on LogLine { line } } }",
        vec![status("run-a"), log("run-a", "only this"), status("run-b")],
        1,
    )
    .await;
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["events"]["line"], "only this");
}

/// A run-scoped subscription sees its run and nothing else.
#[tokio::test]
async fn a_run_scope_keeps_one_runs_frames() {
    let out = frames(
        r#"subscription { events(runId: "run-a") { ... on LogLine { runId line } } }"#,
        vec![
            log("run-b", "not this"),
            log("run-a", "this one"),
            log("run-c", "nor this"),
        ],
        1,
    )
    .await;
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["events"]["runId"], "run-a");
    assert_eq!(out[0]["events"]["line"], "this one");
}

/// Several runs can be named at once, which is what a dashboard watching a few
/// rows wants.
#[tokio::test]
async fn a_set_of_runs_can_be_named() {
    let out = frames(
        r#"subscription { events(runIds: ["run-a", "run-c"]) { ... on LogLine { runId } } }"#,
        vec![log("run-a", "1"), log("run-b", "2"), log("run-c", "3")],
        2,
    )
    .await;
    assert_eq!(out[0]["events"]["runId"], "run-a");
    assert_eq!(out[1]["events"]["runId"], "run-c");
}

/// A sub-agent spawned *after* the subscription started is included when the
/// client asked for descendants.
///
/// This is the difference the option buys: without it a fan-out means
/// re-querying the tree and re-subscribing as it grows, and frames in between
/// are lost.
#[tokio::test]
async fn descendants_spawned_later_are_included() {
    let out = frames(
        r#"subscription { events(runId: "root", includeDescendants: true) {
             __typename
             ... on RunSpawned { runId parentId }
             ... on LogLine { runId line } } }"#,
        vec![
            spawned("worker-1", Some("root")),
            log("worker-1", "from the worker"),
            log("stranger", "from nobody's child"),
        ],
        2,
    )
    .await;
    assert_eq!(out[0]["events"]["__typename"], "RunSpawned");
    assert_eq!(out[0]["events"]["runId"], "worker-1");
    assert_eq!(out[0]["events"]["parentId"], "root");
    // The worker's own frames now arrive, and an unrelated run's still do not.
    assert_eq!(out[1]["events"]["runId"], "worker-1");
    assert_eq!(out[1]["events"]["line"], "from the worker");
}

/// Without the option, a sub-agent's frames are not in scope: the scope is the
/// runs the client named.
#[tokio::test]
async fn a_sub_agent_is_out_of_scope_by_default() {
    let out = frames(
        r#"subscription { events(runId: "root") { ... on LogLine { runId } } }"#,
        vec![
            spawned("worker-1", Some("root")),
            log("worker-1", "ignored"),
            log("root", "kept"),
        ],
        1,
    )
    .await;
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["events"]["runId"], "root");
}

/// A spawn whose parent is not in scope does not widen it.
#[tokio::test]
async fn a_spawn_from_another_tree_does_not_widen_the_scope() {
    let out = frames(
        r#"subscription { events(runId: "root", includeDescendants: true) {
             ... on LogLine { runId } } }"#,
        vec![
            spawned("worker-1", Some("elsewhere")),
            log("worker-1", "not ours"),
            log("root", "ours"),
        ],
        1,
    )
    .await;
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["events"]["runId"], "root");
}

/// A top-level spawn carries no parent, and is kept or dropped on its own run
/// id like any other frame.
#[tokio::test]
async fn a_top_level_spawn_is_scoped_by_its_own_id() {
    let out = frames(
        r#"subscription { events(runId: "root", includeDescendants: true) {
             ... on RunSpawned { runId parentId } } }"#,
        vec![spawned("stranger", None), spawned("root", None)],
        1,
    )
    .await;
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["events"]["runId"], "root");
    assert!(out[0]["events"]["parentId"].is_null());
}

/// The daemon link reaches a scoped subscription, because it explains why that
/// run's frames stopped. The machine's other frames do not.
#[tokio::test]
async fn the_daemon_link_reaches_a_scoped_subscription() {
    let out = frames(
        r#"subscription { events(runId: "run-a") {
             __typename
             ... on DaemonLinkChanged { connected restarted } } }"#,
        vec![
            ServerEvent::ConfigHealth {
                healthy: false,
                path: "/config.toml".to_string(),
                error: None,
                config_mtime: None,
            },
            ServerEvent::DaemonLink {
                connected: false,
                daemon: None,
                restarted: false,
                restart_advised: None,
            },
        ],
        1,
    )
    .await;
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["events"]["__typename"], "DaemonLinkChanged");
    assert_eq!(out[0]["events"]["connected"], false);
}

/// An unscoped subscription gets the machine's own frames too, exactly as `/ws`
/// does.
#[tokio::test]
async fn the_machine_frames_reach_an_unscoped_subscription() {
    let out = frames(
        "subscription { events { __typename
             ... on ConfigHealthChanged { healthy path error } } }",
        vec![ServerEvent::ConfigHealth {
            healthy: false,
            path: "/config.toml".to_string(),
            error: None,
            config_mtime: Some(1_788_000_000),
        }],
        1,
    )
    .await;
    assert_eq!(out[0]["events"]["__typename"], "ConfigHealthChanged");
    assert_eq!(out[0]["events"]["path"], "/config.toml");
}

/// A subscription that falls behind is told how many frames it missed.
///
/// Silently skipped, before this: a client could not tell a quiet run from a
/// missed one, and so could not know to re-read what it renders.
#[tokio::test]
async fn a_subscription_that_falls_behind_is_told() {
    let state = crate::commands::serve::testutil::state_with_agent_paths(Vec::new());
    let tx = state.event_tx.clone();
    let schema = Schema::build(Query, Mutation::default(), Subscription_)
        .data(state)
        .finish();
    let mut stream = schema.execute_stream(Request::new(
        "subscription { events { __typename ... on EventsDropped { count } } }",
    ));

    // Registered first, then overrun: the channel this server uses holds 256,
    // so filling it past that is what a slow client does to itself.
    let first = tokio::spawn(async move {
        let mut seen = Vec::new();
        while seen.len() < 2 {
            match stream.next().await {
                Some(response) => seen.push(response),
                None => break,
            }
        }
        seen
    });
    for _ in 0..100 {
        if tx.receiver_count() > 0 {
            break;
        }
        tokio::task::yield_now().await;
    }
    for i in 0..600 {
        let _ = tx.send(log("run-a", &format!("line {i}")));
    }

    let seen = tokio::time::timeout(std::time::Duration::from_secs(5), first)
        .await
        .expect("frames arrive")
        .expect("the collector finishes");
    let dropped = seen
        .iter()
        .filter_map(|response| match &response.data {
            Value::Object(map) => map.get("events").cloned(),
            _ => None,
        })
        .find_map(|events| match events {
            Value::Object(map) => (map.get("__typename")
                == Some(&Value::String("EventsDropped".into())))
            .then(|| map.get("count").cloned())
            .flatten(),
            _ => None,
        })
        .expect("a drop was reported");
    let missed = match dropped {
        Value::Number(n) => n.as_i64().unwrap_or_default(),
        other => panic!("count is a number: {other:?}"),
    };
    assert!(missed > 0, "it says how many were missed: {missed}");
}

/// The subscription endpoint over a real socket: handshake, subscribe, frame.
///
/// The resolvers are exercised above without HTTP. What this covers is the
/// endpoint: the `graphql-transport-ws` handshake, and a frame arriving over
/// the wire after a broadcast.
#[tokio::test]
async fn a_subscription_runs_over_a_real_websocket() {
    use axum::Router;
    use axum::routing::get;

    let state = crate::commands::serve::testutil::state_with_agent_paths(Vec::new());
    let tx = state.event_tx.clone();
    let schema = crate::commands::serve::graphql::build_schema(state, false);
    let app = Router::new()
        .route("/ws/graphql", get(crate::commands::serve::graphql::ws))
        .layer(axum::extract::Extension(schema));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a port");
    let addr = listener.local_addr().expect("the bound address");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let mut client = crate::commands::serve::testutil::WsTestClient::connect_with_protocol(
        addr,
        "/ws/graphql",
        Some("graphql-transport-ws"),
    )
    .await;

    client.send_text("{\"type\":\"connection_init\"}").await;
    let (opcode, payload) = client.recv_frame().await;
    assert_eq!(opcode, 0x1, "a text frame");
    let ack: serde_json::Value =
        serde_json::from_slice(&payload).expect("the acknowledgement is JSON");
    assert_eq!(ack["type"], "connection_ack");

    client
        .send_text(
            &serde_json::json!({
                "id": "1",
                "type": "subscribe",
                "payload": { "query": "subscription { events { ... on LogLine { runId line } } }" },
            })
            .to_string(),
        )
        .await;

    // Subscribed only once the server has a receiver on the broadcast: a frame
    // sent before that is not delivered, here or in production.
    for _ in 0..200 {
        if tx.receiver_count() > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let _ = tx.send(log("run-a", "over the wire"));

    let (opcode, payload) =
        tokio::time::timeout(std::time::Duration::from_secs(5), client.recv_frame())
            .await
            .expect("the frame arrives");
    assert_eq!(opcode, 0x1);
    let frame: serde_json::Value = serde_json::from_slice(&payload).expect("the frame is JSON");
    assert_eq!(frame["type"], "next");
    assert_eq!(frame["payload"]["data"]["events"]["runId"], "run-a");
    assert_eq!(frame["payload"]["data"]["events"]["line"], "over the wire");

    client.send_close().await;
    server.abort();
}
