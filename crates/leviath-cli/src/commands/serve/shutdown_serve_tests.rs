//! A whole `lev serve` stopping: the stop future resolves, the WebSockets are
//! closed with 1001, requests in flight get the grace period, and the server
//! returns `Ok`.

use super::*;

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::commands::serve::testutil::WsTestClient;

fn args(grace_secs: u64) -> ServeArgs {
    ServeArgs {
        daemon: Default::default(),
        port: 0,
        name: None,
        host: "127.0.0.1".to_string(),
        cors: None,
        token: Some("test-token".to_string()),
        allow_admin: false,
        print_graphql_schema: false,
        workdir_roots: Vec::new(),
        no_remote_yolo: false,
        tls_cert: None,
        tls_key: None,
        no_remote_seed_commands: false,
        max_concurrent_requests: None,
        request_timeout_secs: Some(0),
        exit_on_stdin_eof: true,
        shutdown_grace_secs: grace_secs,
        json: false,
        ready_file: None,
        spawn_timeout_secs: None,
    }
}

/// A server stopped by `stop`, and the address it listens on.
async fn boot(
    grace_secs: u64,
    stop: StopFuture,
) -> (SocketAddr, tokio::task::JoinHandle<anyhow::Result<()>>) {
    let control = leviath_runtime::control_socket::ControlClient::new(
        leviath_runtime::control_socket::control_id(std::path::Path::new("/no/such/leviath")),
    );
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let handle = tokio::spawn(serve_for_test(
        args(grace_secs),
        control,
        stop,
        Some(ready_tx),
    ));
    let addr = ready_rx.await.expect("the server reports its address");
    (addr, handle)
}

/// A stop future and the sender that fires it.
fn stop_switch() -> (tokio::sync::oneshot::Sender<()>, StopFuture) {
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let stop: StopFuture = Box::pin(async move {
        let _ = rx.await;
    });
    (tx, stop)
}

/// The server task, which must end by itself and cleanly.
async fn stops_cleanly(server: tokio::task::JoinHandle<anyhow::Result<()>>) {
    tokio::time::timeout(Duration::from_secs(20), server)
        .await
        .expect("the server stops by itself")
        .expect("the server task does not panic")
        .expect("a clean stop is not an error");
}

/// The next frame is a close frame carrying `code`.
async fn closes_with(client: &mut WsTestClient, code: u16) {
    loop {
        let (opcode, payload) = client.recv_frame().await;
        if opcode == 0x8 {
            assert_eq!(u16::from_be_bytes([payload[0], payload[1]]), code);
            return;
        }
    }
}

/// A `/ws/graphql` client past its `connection_init`.
async fn graphql_client(addr: SocketAddr) -> WsTestClient {
    let mut client = WsTestClient::connect_with_protocol(
        addr,
        "/ws/graphql?token=test-token",
        Some("graphql-transport-ws"),
    )
    .await;
    client.send_text(r#"{"type":"connection_init"}"#).await;
    let (_, ack) = client.recv_frame().await;
    assert!(String::from_utf8_lossy(&ack).contains("connection_ack"));
    client
}

/// The parent closing our stdin stops the server the way a signal does: the
/// subscribers are told 1001 and the server returns `Ok`.
#[tokio::test]
async fn a_closed_stdin_pipe_stops_the_server() {
    crate::config::with_isolated_config_path_async("serve-stdin-eof", |_dir| async move {
        let (reader, writer) = std::io::pipe().expect("a pipe");
        let stop = shutdown::either(
            Box::pin(std::future::pending()),
            Some(shutdown::closed(Box::new(reader))),
        );
        let (addr, server) = boot(10, stop).await;
        let mut ws = WsTestClient::connect(addr, "/ws?token=test-token").await;
        let mut graphql = graphql_client(addr).await;

        drop(writer);

        closes_with(&mut ws, 1001).await;
        closes_with(&mut graphql, 1001).await;
        stops_cleanly(server).await;
    })
    .await;
}

/// A subscription that already ended is not sent a second goodbye, and the
/// per-run stream is closed like the global one.
#[tokio::test]
async fn a_stop_closes_every_websocket_still_open() {
    crate::config::with_isolated_config_path_async("serve-stop-ws", |_dir| async move {
        let (stop_tx, stop) = stop_switch();
        let (addr, server) = boot(10, stop).await;
        let mut run = WsTestClient::connect(addr, "/ws/agents/some-run?token=test-token").await;
        let mut gone = graphql_client(addr).await;
        gone.send_close().await;
        assert_eq!(gone.recv_eof().await.map(|_| ()), Some(()));

        stop_tx.send(()).expect("the server is listening");

        closes_with(&mut run, 1001).await;
        stops_cleanly(server).await;
    })
    .await;
}

/// Send the head of a `POST /graphql` and part of its body, leaving the
/// request in flight.
async fn half_a_request(addr: SocketAddr) -> (tokio::net::TcpStream, &'static [u8]) {
    let body: &'static [u8] = br#"{"query":"{ __typename }"}"#;
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let head = format!(
        "POST /graphql HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer test-token\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await.expect("head");
    stream
        .write_all(&body[..5])
        .await
        .expect("part of the body");
    // Let the server take the head before the stop arrives.
    tokio::time::sleep(Duration::from_millis(200)).await;
    (stream, &body[5..])
}

/// A request already in flight when the stop arrives still gets its answer.
#[tokio::test]
async fn a_request_in_flight_finishes_inside_the_grace_period() {
    crate::config::with_isolated_config_path_async("serve-stop-inflight", |_dir| async move {
        let (stop_tx, stop) = stop_switch();
        let (addr, server) = boot(10, stop).await;
        let (mut stream, rest) = half_a_request(addr).await;

        stop_tx.send(()).expect("the server is listening");
        tokio::time::sleep(Duration::from_millis(200)).await;
        stream.write_all(rest).await.expect("the rest of the body");

        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.expect("a response");
        let response = String::from_utf8_lossy(&response).into_owned();
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("__typename"), "{response}");
        stops_cleanly(server).await;
    })
    .await;
}

/// A request that never finishes holds the server for the grace period only.
#[tokio::test]
async fn a_request_that_never_finishes_is_abandoned_after_the_grace_period() {
    crate::config::with_isolated_config_path_async("serve-stop-stuck", |_dir| async move {
        let (stop_tx, stop) = stop_switch();
        let (addr, server) = boot(1, stop).await;
        let (_stream, _rest) = half_a_request(addr).await;

        let started = std::time::Instant::now();
        stop_tx.send(()).expect("the server is listening");
        stops_cleanly(server).await;
        // At least the grace period, and not much more: a server that ignored
        // it would sit on the stuck request until the 20 s test timeout.
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(900), "stopped after {took:?}");
        assert!(took < Duration::from_secs(5), "stopped after {took:?}");
    })
    .await;
}
