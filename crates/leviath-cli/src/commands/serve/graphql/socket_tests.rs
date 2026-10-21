//! Tests for the `/ws/graphql` keepalive: the server pings, gives up on a
//! peer that stops answering or stops reading, and lets the protocol close
//! the socket itself.

use std::net::SocketAddr;
use std::time::Duration;

use async_graphql::http::ALL_WEBSOCKET_PROTOCOLS;
use async_graphql_axum::GraphQLProtocol;
use axum::Router;
use axum::extract::WebSocketUpgrade;
use axum::routing::get;

use super::super::super::events::{EventBus, ServerEvent, send};
use super::super::super::keepalive::WsTiming;
use super::super::super::testutil::WsTestClient;

/// A server whose `/ws/graphql` runs on `timing`, and the bus behind it.
async fn server(timing: WsTiming) -> (SocketAddr, EventBus) {
    let dir = tempfile::tempdir().expect("an agents dir");
    let state = super::super::super::testutil::state_with_agent_paths(vec![dir.keep()]);
    let bus = state.event_tx.clone();
    let shutdown = state.shutdown.clone();
    let schema = super::super::build_schema(state, false);
    let app = Router::new().route(
        "/ws/graphql",
        get(
            move |protocol: GraphQLProtocol, upgrade: WebSocketUpgrade| async move {
                upgrade
                    .protocols(ALL_WEBSOCKET_PROTOCOLS)
                    .on_upgrade(move |socket| {
                        super::serve(socket, schema, protocol, timing, shutdown)
                    })
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a port");
    let addr = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (addr, bus)
}

/// A client past its `connection_init`.
async fn client(addr: SocketAddr) -> WsTestClient {
    let mut client =
        WsTestClient::connect_with_protocol(addr, "/ws/graphql", Some("graphql-transport-ws"))
            .await;
    client.send_text(r#"{"type":"connection_init"}"#).await;
    let (_, ack) = client.recv_frame().await;
    assert!(String::from_utf8_lossy(&ack).contains("connection_ack"));
    client
}

/// Start a log-line subscription and wait for its receiver to register.
async fn subscribe(client: &mut WsTestClient, bus: &EventBus) {
    client
        .send_text(
            r#"{"id":"1","type":"subscribe","payload":{"query":"subscription { runEvents { __typename ... on LogLineWrittenEvent { line } } }"}}"#,
        )
        .await;
    receivers_reach(bus, 1).await;
}

/// Wait until the bus has `expected` receivers, or fail.
async fn receivers_reach(bus: &EventBus, expected: usize) {
    for _ in 0..200 {
        if bus.receiver_count() == expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(bus.receiver_count(), expected, "the receiver count");
}

/// A peer that answers pings stays connected across many of them.
#[tokio::test]
async fn a_peer_that_answers_pings_stays_connected() {
    let (addr, bus) = server(WsTiming {
        ping_every: Duration::from_millis(40),
        pong_timeout: Duration::from_secs(30),
        send_timeout: Duration::from_secs(5),
    })
    .await;
    let mut client = client(addr).await;
    subscribe(&mut client, &bus).await;
    let opened = String::from_utf8(client.recv_frame().await.1).expect("text");
    assert!(opened.contains("SubscriptionOpenedEvent"), "{opened}");
    for _ in 0..4 {
        let (opcode, payload) = tokio::time::timeout(Duration::from_secs(30), client.recv_frame())
            .await
            .expect("the server keeps pinging");
        assert_eq!(opcode, 0x9, "a ping");
        client.send_frame(0xA, &payload).await;
    }
    send(
        &bus,
        ServerEvent::Log {
            agent_id: "a".to_string(),
            run_id: "run-a".to_string(),
            line: "still here".to_string(),
        },
    );
    let frame = loop {
        let (opcode, payload) = client.recv_frame().await;
        if opcode == 0x1 {
            break String::from_utf8(payload).expect("text");
        }
    };
    assert!(frame.contains("still here"), "{frame}");
    client.send_close().await;
    receivers_reach(&bus, 0).await;
}

/// A peer that never answers a ping is closed when its pong is due, and its
/// subscription with it.
#[tokio::test]
async fn a_peer_that_never_answers_is_closed_when_its_pong_is_due() {
    let (addr, bus) = server(WsTiming {
        ping_every: Duration::from_millis(300),
        pong_timeout: Duration::from_millis(100),
        send_timeout: Duration::from_secs(5),
    })
    .await;
    let mut client = client(addr).await;
    subscribe(&mut client, &bus).await;
    let pinged = super::super::super::testutil::until_pinged(&mut client).await;
    assert_eq!(client.recv_eof().await, None, "the server hangs up");
    super::super::super::testutil::closed_within_the_deadline(
        pinged.elapsed(),
        Duration::from_millis(100),
    );
    receivers_reach(&bus, 0).await;
}

/// A peer that drops the connection without a close frame ends it at once.
#[tokio::test]
async fn a_peer_that_vanishes_ends_the_connection() {
    let (addr, bus) = server(WsTiming {
        ping_every: Duration::from_secs(3600),
        pong_timeout: Duration::from_secs(3600),
        send_timeout: Duration::from_secs(5),
    })
    .await;
    let mut client = client(addr).await;
    subscribe(&mut client, &bus).await;
    client.close_abruptly();
    receivers_reach(&bus, 0).await;
}

/// A peer that stopped reading is dropped once a send outlasts its deadline.
#[tokio::test]
async fn a_peer_that_stopped_reading_is_dropped() {
    let (addr, bus) = server(WsTiming {
        ping_every: Duration::from_secs(3600),
        pong_timeout: Duration::from_secs(3600),
        send_timeout: Duration::from_millis(100),
    })
    .await;
    let mut client = client(addr).await;
    subscribe(&mut client, &bus).await;
    let big = "x".repeat(256 * 1024);
    for _ in 0..64 {
        if bus.receiver_count() == 0 {
            break;
        }
        send(
            &bus,
            ServerEvent::Log {
                agent_id: "a".to_string(),
                run_id: "run-a".to_string(),
                line: big.clone(),
            },
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    receivers_reach(&bus, 0).await;
}

/// The protocol closing the socket is passed on, and ends the connection.
#[tokio::test]
async fn the_protocol_can_close_the_socket_itself() {
    let (addr, _bus) = server(WsTiming {
        ping_every: Duration::from_secs(3600),
        pong_timeout: Duration::from_secs(3600),
        send_timeout: Duration::from_secs(5),
    })
    .await;
    let mut client = client(addr).await;
    // A second `connection_init` is a protocol error the server closes on.
    client.send_text(r#"{"type":"connection_init"}"#).await;
    let (opcode, payload) = tokio::time::timeout(Duration::from_secs(10), client.recv_frame())
        .await
        .expect("the close arrives");
    assert_eq!(opcode, 0x8, "a close frame");
    assert_eq!(u16::from_be_bytes([payload[0], payload[1]]), 4429);
    assert_eq!(client.recv_eof().await, None, "and the socket ends");
}
