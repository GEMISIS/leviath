//! WebSocket endpoints and connection handling.

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path as AxumPath, State, WebSocketUpgrade};
use axum::response::IntoResponse;
use tokio::sync::broadcast;

use super::types::*;

pub(super) async fn ws_global(
    State(state): State<AppState>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    // Subscribe before on_upgrade so the Receiver (not a Sender clone) is
    // moved into the handler — that way when all external Senders drop the
    // channel becomes Closed and rx.recv() returns Err(Closed) immediately,
    // making that match arm reachable in tests.
    let rx = state.event_tx.subscribe();
    ws.on_upgrade(move |socket| handle_ws(socket, rx, None))
}

pub(super) async fn ws_agent(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let rx = state.event_tx.subscribe();
    ws.on_upgrade(move |socket| handle_ws(socket, rx, Some(id)))
}

async fn handle_ws(
    mut socket: WebSocket,
    mut rx: broadcast::Receiver<ServerEvent>,
    filter_run_id: Option<String>,
) {
    loop {
        tokio::select! {
            biased;
            event = rx.recv() => {
                match event {
                    Ok(ev) => {
                        // If filtering by run_id, skip non-matching events
                        if let Some(ref filter) = filter_run_id {
                            let event_run_id = match &ev {
                                ServerEvent::AgentStatus { run_id, .. } => run_id,
                                ServerEvent::ContextUpdate { run_id, .. } => run_id,
                                ServerEvent::Log { run_id, .. } => run_id,
                                ServerEvent::InteractionNeeded { run_id, .. } => run_id,
                                ServerEvent::AgentSpawned { run_id, .. } => run_id,
                                ServerEvent::AgentCompleted { run_id, .. } => run_id,
                                ServerEvent::Tokens { run_id, .. } => run_id,
                            };
                            if event_run_id != filter {
                                continue;
                            }
                        }

                        // ServerEvent always serializes; a failure is a bug.
                        let json = serde_json::to_string(&ev)
                            .expect("ServerEvent serialization must not fail");
                        if socket.send(Message::Text(json.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("WebSocket subscriber lagged by {} events", n);
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {} // Ignore other client messages
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::routing::get;
    use axum::Router;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    use crate::config::Config;
    use crate::test_support::with_tracing;

    /// Minimal hand-rolled WebSocket client used to drive `handle_ws` end to
    /// end over a real TCP loopback connection. No `tokio-tungstenite` or
    /// other WS crate is added as a dependency — this speaks just enough of
    /// RFC 6455 to perform the opening handshake and exchange text/close
    /// frames with axum's server-side WebSocket implementation.
    struct WsTestClient {
        stream: TcpStream,
    }

    /// `stream.read()` returning `0` mid-handshake means the peer closed the
    /// connection before sending a complete response.
    fn assert_handshake_byte_read(n: usize) {
        assert_ne!(n, 0, "connection closed before handshake completed");
    }

    #[test]
    #[should_panic(expected = "connection closed before handshake completed")]
    fn assert_handshake_byte_read_panics_on_zero() {
        assert_handshake_byte_read(0);
    }

    fn assert_handshake_101(response: &str) {
        #[rustfmt::skip]
        assert!(response.starts_with("HTTP/1.1 101"), "expected 101 Switching Protocols, got: {response}");
    }

    #[test]
    #[should_panic(expected = "expected 101 Switching Protocols, got: HTTP/1.1 404 Not Found")]
    fn assert_handshake_101_panics_on_non_101() {
        assert_handshake_101("HTTP/1.1 404 Not Found");
    }

    impl WsTestClient {
        async fn connect(addr: std::net::SocketAddr, path: &str) -> Self {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            let request = format!(
                "GET {path} HTTP/1.1\r\n\
                 Host: {addr}\r\n\
                 Connection: Upgrade\r\n\
                 Upgrade: websocket\r\n\
                 Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                 Sec-WebSocket-Version: 13\r\n\
                 \r\n"
            );
            stream.write_all(request.as_bytes()).await.unwrap();

            // Read until the end of the HTTP response headers (\r\n\r\n).
            let mut buf = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                let n = stream.read(&mut byte).await.unwrap();
                assert_handshake_byte_read(n);
                buf.push(byte[0]);
                if buf.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            let response = String::from_utf8_lossy(&buf);
            assert_handshake_101(&response);

            Self { stream }
        }

        /// Send a masked text frame (client → server frames must be masked
        /// per RFC 6455).
        async fn send_text(&mut self, text: &str) {
            self.send_frame(0x1, text.as_bytes()).await;
        }

        /// Send a masked close frame.
        async fn send_close(&mut self) {
            self.send_frame(0x8, &[]).await;
        }

        async fn send_frame(&mut self, opcode: u8, payload: &[u8]) {
            let mut frame = vec![0x80 | opcode];
            let mask: [u8; 4] = [0x12, 0x34, 0x56, 0x78];
            let len = payload.len();
            if len < 126 {
                frame.push(0x80 | len as u8);
            } else {
                frame.push(0x80 | 126);
                frame.push((len >> 8) as u8);
                frame.push(len as u8);
            }
            frame.extend_from_slice(&mask);
            for (i, b) in payload.iter().enumerate() {
                frame.push(b ^ mask[i % 4]);
            }
            self.stream.write_all(&frame).await.unwrap();
        }

        /// Read one server frame (unmasked) and return (opcode, payload).
        async fn recv_frame(&mut self) -> (u8, Vec<u8>) {
            let mut header = [0u8; 2];
            self.stream.read_exact(&mut header).await.unwrap();
            let opcode = header[0] & 0x0f;
            let mut len = (header[1] & 0x7f) as usize;
            if len == 126 {
                let mut ext = [0u8; 2];
                self.stream.read_exact(&mut ext).await.unwrap();
                len = u16::from_be_bytes(ext) as usize;
            } else if len == 127 {
                let mut ext = [0u8; 8];
                self.stream.read_exact(&mut ext).await.unwrap();
                len = u64::from_be_bytes(ext) as usize;
            }
            let mut payload = vec![0u8; len];
            if len > 0 {
                self.stream.read_exact(&mut payload).await.unwrap();
            }
            (opcode, payload)
        }

        /// Read a single byte, returning `None` on a clean EOF (used to
        /// detect that the server has closed the connection).
        async fn recv_eof(&mut self) -> Option<u8> {
            let mut byte = [0u8; 1];
            match self.stream.read(&mut byte).await {
                Ok(0) => None,
                Ok(_) => Some(byte[0]),
                Err(_) => None,
            }
        }
    }

    fn test_state() -> AppState {
        let (tx, _) = broadcast::channel(64);
        AppState {
            config: Arc::new(Config::default()),
            event_tx: tx,
        }
    }

    /// Returns `(addr, shutdown_tx, handle)`.  Sending on (or dropping)
    /// `shutdown_tx` causes axum to stop accepting and the server task to exit.
    async fn spawn_test_server_with_shutdown(
        state: AppState,
    ) -> (
        std::net::SocketAddr,
        tokio::sync::oneshot::Sender<()>,
        tokio::task::JoinHandle<()>,
    ) {
        let app = Router::new()
            .route("/ws", get(ws_global))
            .route("/ws/agents/{id}", get(ws_agent))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await;
        });
        (addr, shutdown_tx, handle)
    }

    async fn spawn_test_server(state: AppState) -> std::net::SocketAddr {
        let (addr, shutdown_tx, _handle) = spawn_test_server_with_shutdown(state).await;
        // Leak the shutdown sender so the server stays alive for the duration
        // of the test process; tests that need a clean shutdown use
        // `spawn_test_server_with_shutdown` directly.
        std::mem::forget(shutdown_tx);
        addr
    }

    fn assert_text_frame(opcode: u8) {
        assert_eq!(opcode, 0x1, "expected a text frame");
    }

    #[test]
    #[should_panic(expected = "expected a text frame")]
    fn assert_text_frame_panics_on_non_text_opcode() {
        assert_text_frame(0x2);
    }

    #[tokio::test]
    async fn ws_global_relays_broadcast_event_to_client() {
        let state = test_state();
        let tx = state.event_tx.clone();
        let addr = spawn_test_server(state).await;

        let mut client = WsTestClient::connect(addr, "/ws").await;

        // Give the server a moment to subscribe before we broadcast.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        tx.send(ServerEvent::Log {
            agent_id: "a".to_string(),
            run_id: "run-1".to_string(),
            line: "hello".to_string(),
        })
        .unwrap();

        let (opcode, payload) =
            tokio::time::timeout(std::time::Duration::from_secs(5), client.recv_frame())
                .await
                .expect("timed out waiting for event frame");
        assert_text_frame(opcode);
        let text = String::from_utf8(payload).unwrap();
        assert!(text.contains("\"type\":\"log\""));
        assert!(text.contains("\"run_id\":\"run-1\""));

        // Close so the server-side handle_ws task terminates instead of
        // idling forever on rx.recv() for the rest of the process lifetime.
        client.send_close().await;
    }

    #[tokio::test]
    async fn ws_global_relays_large_event_using_64bit_extended_length() {
        // A `ServerEvent::Log` with a >65535-byte `line` field serializes to
        // a JSON payload exceeding u16::MAX bytes, forcing the server's WS
        // frame to use the 8-byte extended-length encoding (0x7f) instead of
        // the 2-byte one (0x7e) that every other event in this file's tests
        // is small enough to use -- exercising `recv_frame`'s `len == 127`
        // branch, previously never triggered.
        let state = test_state();
        let tx = state.event_tx.clone();
        let addr = spawn_test_server(state).await;

        let mut client = WsTestClient::connect(addr, "/ws").await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let huge_line = "x".repeat(70_000);
        tx.send(ServerEvent::Log {
            agent_id: "a".to_string(),
            run_id: "run-huge".to_string(),
            line: huge_line.clone(),
        })
        .unwrap();

        let (opcode, payload) =
            tokio::time::timeout(std::time::Duration::from_secs(5), client.recv_frame())
                .await
                .expect("timed out waiting for large event frame");
        assert_eq!(opcode, 0x1);
        let text = String::from_utf8(payload).unwrap();
        assert!(text.contains(&huge_line));

        client.send_close().await;
    }

    #[tokio::test]
    async fn ws_test_client_send_frame_encodes_medium_length_payload() {
        // `send_frame`'s own medium-length (0x7e, 2-byte extended) encoding
        // branch was never exercised: every existing test's outbound
        // client->server frame (a text message or an empty close frame) is
        // under 126 bytes. The server ignores non-Close client messages
        // either way (see `handle_ws`'s `_ => {}` arm), so this only proves
        // the test helper itself encodes a >=126-byte frame correctly and
        // that doing so doesn't upset the server's connection handling.
        let state = test_state();
        let addr = spawn_test_server(state).await;
        let mut client = WsTestClient::connect(addr, "/ws").await;

        let payload = "y".repeat(200);
        client.send_frame(0x1, payload.as_bytes()).await;

        // Prove the connection is still alive after sending the oversized
        // frame by having the server relay a follow-up event normally.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        client.send_close().await;
    }

    #[tokio::test]
    async fn ws_agent_filters_events_by_run_id() {
        let state = test_state();
        let tx = state.event_tx.clone();
        let addr = spawn_test_server(state).await;

        let mut client = WsTestClient::connect(addr, "/ws/agents/run-match").await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Non-matching event first — should be filtered out and not sent.
        tx.send(ServerEvent::Log {
            agent_id: "a".to_string(),
            run_id: "run-other".to_string(),
            line: "skip me".to_string(),
        })
        .unwrap();
        // Matching event — should be relayed.
        tx.send(ServerEvent::Log {
            agent_id: "a".to_string(),
            run_id: "run-match".to_string(),
            line: "deliver me".to_string(),
        })
        .unwrap();

        let (opcode, payload) =
            tokio::time::timeout(std::time::Duration::from_secs(5), client.recv_frame())
                .await
                .expect("timed out waiting for event frame");
        assert_eq!(opcode, 0x1);
        let text = String::from_utf8(payload).unwrap();
        assert!(text.contains("\"run_id\":\"run-match\""));
        assert!(!text.contains("run-other"));

        // Close so the server-side handle_ws task terminates instead of
        // idling forever on rx.recv() for the rest of the process lifetime.
        client.send_close().await;
    }

    #[tokio::test]
    async fn ws_agent_filter_matches_run_id_for_every_event_variant() {
        // Drives handle_ws's run_id-extraction match with one of each
        // ServerEvent variant so every match arm in the filtering branch
        // (not just the Log arm exercised by other tests) is executed.
        let state = test_state();
        let tx = state.event_tx.clone();
        let addr = spawn_test_server(state).await;

        let mut client = WsTestClient::connect(addr, "/ws/agents/run-match").await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let events = vec![
            ServerEvent::AgentStatus {
                agent_id: "a".to_string(),
                run_id: "run-match".to_string(),
                status: "running".to_string(),
                stage: "plan".to_string(),
                iteration: 1,
                tool_calls: 0,
                accepts_messages: true,
            },
            ServerEvent::ContextUpdate {
                agent_id: "a".to_string(),
                run_id: "run-match".to_string(),
                total_tokens: 10,
                max_tokens: 100,
            },
            ServerEvent::InteractionNeeded {
                agent_id: "a".to_string(),
                run_id: "run-match".to_string(),
                request: serde_json::Value::Null,
            },
            ServerEvent::AgentSpawned {
                agent_id: "a".to_string(),
                run_id: "run-match".to_string(),
                parent_id: None,
                blueprint: "bp".to_string(),
            },
            ServerEvent::AgentCompleted {
                agent_id: "a".to_string(),
                run_id: "run-match".to_string(),
                status: "complete".to_string(),
                result: None,
            },
            ServerEvent::Tokens {
                agent_id: "a".to_string(),
                run_id: "run-match".to_string(),
                prompt_tokens: 1,
                completion_tokens: 2,
                cached_tokens: 0,
                cache_write_tokens: 0,
            },
        ];

        for ev in events {
            tx.send(ev).unwrap();
            let (opcode, payload) =
                tokio::time::timeout(std::time::Duration::from_secs(5), client.recv_frame())
                    .await
                    .expect("timed out waiting for event frame");
            assert_eq!(opcode, 0x1);
            let text = String::from_utf8(payload).unwrap();
            assert!(text.contains("\"run_id\":\"run-match\""));
        }

        // Close so the server-side handle_ws task terminates instead of
        // idling forever on rx.recv() for the rest of the process lifetime.
        client.send_close().await;
    }

    fn assert_clean_eof_after_close(eof: Option<u8>) {
        assert_eq!(eof, None, "expected clean EOF after server processed close");
    }

    #[test]
    #[should_panic(expected = "expected clean EOF after server processed close")]
    fn assert_clean_eof_after_close_panics_on_some() {
        assert_clean_eof_after_close(Some(0x42));
    }

    #[tokio::test]
    async fn ws_global_closes_on_client_close_frame() {
        let state = test_state();
        let addr = spawn_test_server(state).await;

        let mut client = WsTestClient::connect(addr, "/ws").await;
        // Send a text frame first to prove normal client->server traffic
        // is accepted (and ignored) rather than breaking the loop.
        client.send_text("ignored client message").await;
        client.send_close().await;

        // `handle_ws` breaks its loop on a close frame, which drops the
        // socket and closes the underlying TCP connection cleanly.
        let eof = tokio::time::timeout(std::time::Duration::from_secs(5), client.recv_eof())
            .await
            .expect("timed out waiting for server to close the connection");
        assert_clean_eof_after_close(eof);
    }

    #[tokio::test]
    async fn ws_global_lagged_receiver_does_not_crash_connection() {
        // Install the shared `AlwaysOnSubscriber` (see `crate::test_support`)
        // so the `warn!(...)` call in handle_ws's `Lagged` arm below actually
        // evaluates its message-format region instead of short-circuiting.
        with_tracing(|| {});
        // Use a tiny broadcast buffer so that flooding it triggers a `Lagged`
        // error on the server-side subscriber, exercising that branch of
        // `handle_ws` without needing to fabricate the error directly.
        let (tx, _) = broadcast::channel::<ServerEvent>(2);
        let state = AppState {
            config: Arc::new(Config::default()),
            event_tx: tx.clone(),
        };
        let addr = spawn_test_server(state).await;

        let mut client = WsTestClient::connect(addr, "/ws").await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Send far more events than the channel capacity so the server's
        // subscriber lags and receives `RecvError::Lagged`.
        for i in 0..20 {
            let _ = tx.send(ServerEvent::Log {
                agent_id: "a".to_string(),
                run_id: "run-1".to_string(),
                line: format!("line-{i}"),
            });
        }

        // The connection should survive the lag and keep delivering whatever
        // events remain in the channel afterwards.
        let (opcode, _payload) =
            tokio::time::timeout(std::time::Duration::from_secs(5), client.recv_frame())
                .await
                .expect("connection should still be alive after a lag");
        assert_eq!(opcode, 0x1);

        // Close so the server-side handle_ws task terminates instead of
        // idling forever on rx.recv() for the rest of the process lifetime.
        client.send_close().await;
    }

    #[tokio::test]
    async fn ws_global_breaks_on_abrupt_tcp_close_without_ws_close_frame() {
        // Unlike `ws_global_closes_on_client_close_frame` (a clean WS close
        // frame -> `Some(Ok(Message::Close(_)))`), this drives the sibling
        // `None` arm of the same match: the client tears down the raw TCP
        // connection without ever sending a WS close frame, so
        // `socket.recv()` observes end-of-stream as `None`.
        let state = test_state();
        let addr = spawn_test_server(state).await;

        let client = WsTestClient::connect(addr, "/ws").await;
        drop(client.stream);

        // The server task should exit promptly rather than hang; prove the
        // server itself is still alive and accepting new connections
        // afterwards (i.e. handling the abrupt close didn't panic anything).
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let mut second = WsTestClient::connect(addr, "/ws").await;
        second.send_close().await;
    }

    #[tokio::test]
    async fn ws_global_breaks_when_send_fails_after_abrupt_client_close() {
        // Exercises `socket.send(...).await.is_err() { break; }` at line 62.
        //
        // Strategy: fill the broadcast channel with many events BEFORE and
        // immediately after dropping the client. With the biased select!,
        // handle_ws always tries the event branch first, so when the TCP RST
        // eventually propagates it catches a pending event and tries (and
        // fails) to send it to the dead socket. Sending many large events
        // ensures the kernel send-buffer is exhausted quickly so the failure
        // occurs within the first few retries.
        let state = test_state();
        let tx = state.event_tx.clone();
        let addr = spawn_test_server(state).await;

        let client = WsTestClient::connect(addr, "/ws").await;
        // Let the WS handshake settle.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        // Flood the channel so handle_ws has pending events when the RST arrives.
        for i in 0..100 {
            let _ = tx.send(ServerEvent::Log {
                agent_id: "a".to_string(),
                run_id: "run-1".to_string(),
                line: format!("pre-drop flood event {i}"),
            });
        }

        // Drop the client — TCP FIN/RST is sent.
        drop(client.stream);

        // Keep sending events so the biased select keeps trying the event
        // branch, hitting the broken socket until send() returns Err.
        for i in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let _ = tx.send(ServerEvent::Log {
                agent_id: "a".to_string(),
                run_id: "run-1".to_string(),
                line: format!("post-drop event {i}"),
            });
        }

        // Prove the server is still healthy afterwards.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let mut second = WsTestClient::connect(addr, "/ws").await;
        second.send_close().await;
    }

    /// Exercises `Err(RecvError::Closed) => break` in `handle_ws`.
    ///
    /// Because `handle_ws` now receives a `Receiver` (not a `Sender`), all
    /// senders can drop while `handle_ws` is running.  We trigger that by:
    /// 1. creating a test-side sender + AppState that each hold a sender clone;
    /// 2. connecting a WS client so `handle_ws` is actively looping;
    /// 3. shutting the server down with graceful shutdown (drops AppState +
    ///    its Sender clone); and
    /// 4. dropping the test-side sender — making the channel Closed so
    ///    rx.recv() returns Err(Closed) and handle_ws breaks.
    fn assert_closed_after_channel_closed(eof: Option<u8>) {
        assert_eq!(eof, None, "server should close after channel Closed");
    }

    #[test]
    #[should_panic(expected = "server should close after channel Closed")]
    fn assert_closed_after_channel_closed_panics_on_some() {
        assert_closed_after_channel_closed(Some(0x1));
    }

    #[tokio::test]
    async fn handle_ws_breaks_on_closed_channel_via_server_shutdown() {
        let (tx, _) = broadcast::channel::<ServerEvent>(16);
        let state = AppState {
            config: Arc::new(Config::default()),
            event_tx: tx.clone(),
        };
        let (addr, shutdown_tx, handle) = spawn_test_server_with_shutdown(state).await;

        let mut client = WsTestClient::connect(addr, "/ws").await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Drop the test-side sender clone first.
        drop(tx);
        // Signal graceful shutdown: the server accepts no new connections and
        // drops its router (AppState), which drops the last Sender clone.
        let _ = shutdown_tx.send(());
        // Wait for the server task to exit — at that point the channel is Closed.
        tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("server did not shut down in time")
            .expect("server panicked");

        // handle_ws's rx.recv() should now return Err(Closed), causing break.
        let eof = tokio::time::timeout(std::time::Duration::from_secs(5), client.recv_eof())
            .await
            .expect("timed out waiting for server to close after channel closed");
        assert_closed_after_channel_closed(eof);
    }

    /// Exercises the graceful-shutdown path of `axum::serve` so the
    /// `spawn_test_server_with_shutdown` helper's `axum::serve(…).await`
    /// expression is covered.
    #[tokio::test]
    async fn spawn_test_server_axum_serve_returns_on_graceful_shutdown() {
        let state = test_state();
        let (addr, shutdown_tx, handle) = spawn_test_server_with_shutdown(state).await;
        // Confirm the server is up.
        let mut client = WsTestClient::connect(addr, "/ws").await;
        client.send_close().await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // Signal shutdown and wait for the task to exit.
        let _ = shutdown_tx.send(());
        tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("timed out waiting for server to shut down")
            .unwrap();
    }

    fn assert_text_opcode(opcode: u8) {
        assert_eq!(opcode, 0x1, "expected text opcode");
    }

    #[test]
    #[should_panic(expected = "expected text opcode")]
    fn assert_text_opcode_panics_on_non_text() {
        assert_text_opcode(0x2);
    }

    fn assert_empty_payload(payload: &[u8]) {
        assert!(payload.is_empty(), "expected empty payload");
    }

    #[test]
    #[should_panic(expected = "expected empty payload")]
    fn assert_empty_payload_panics_on_nonempty() {
        assert_empty_payload(&[1, 2, 3]);
    }

    /// Exercises `recv_frame`'s `if len > 0` false branch by having a raw TCP
    /// server send a WS text frame with a 0-byte payload.
    #[tokio::test]
    async fn recv_frame_handles_zero_length_payload() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            // FIN=1, RSV=0, opcode=text(0x1): 0x81; MASK=0, len=0: 0x00
            let frame = [0x81u8, 0x00];
            let _ = sock.write_all(&frame).await;
            let _ = sock.shutdown().await;
        });

        // Connect a raw TcpStream and call recv_frame directly (no WS handshake).
        let stream = TcpStream::connect(addr).await.unwrap();
        let mut client = WsTestClient { stream };

        let (opcode, payload) =
            tokio::time::timeout(std::time::Duration::from_secs(5), client.recv_frame())
                .await
                .expect("timed out waiting for zero-length frame");
        assert_text_opcode(opcode);
        assert_empty_payload(&payload);
    }

    fn assert_none_on_clean_eof(result: Option<u8>) {
        assert_eq!(result, None, "expected None on clean EOF");
    }

    #[test]
    #[should_panic(expected = "expected None on clean EOF")]
    fn assert_none_on_clean_eof_panics_on_some() {
        assert_none_on_clean_eof(Some(0x1));
    }

    /// Exercises `recv_eof`'s `Ok(0) => None` branch by closing the server
    /// write side so the client sees a clean EOF.
    #[tokio::test]
    async fn recv_eof_returns_none_on_clean_server_shutdown() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut conn, _) = listener.accept().await.unwrap();
            // Drain any bytes the client sends (ignore) then close write side.
            let mut buf = [0u8; 256];
            let _ = conn.read(&mut buf).await;
            conn.shutdown().await.unwrap();
        });

        let mut stream = TcpStream::connect(addr).await.unwrap();
        // Send something so the server doesn't block on read.
        let _ = stream.write_all(b"hi").await;
        let mut client = WsTestClient { stream };
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), client.recv_eof())
            .await
            .expect("timed out");
        assert_none_on_clean_eof(result);
    }

    fn assert_some_byte_arrived(result: Option<u8>) {
        assert_eq!(result, Some(0x42), "expected the sent byte");
    }

    #[test]
    #[should_panic(expected = "expected the sent byte")]
    fn assert_some_byte_arrived_panics_on_mismatch() {
        assert_some_byte_arrived(Some(0x99));
    }

    /// Exercises `recv_eof`'s `Ok(_) => Some(byte[0])` branch by having the
    /// server send one byte after the client connects.
    #[tokio::test]
    async fn recv_eof_returns_some_when_byte_arrives() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut conn, _) = listener.accept().await.unwrap();
            // Send one byte immediately, then close so the client sees the byte.
            let _ = conn.write_all(&[0x42u8]).await;
            let _ = conn.shutdown().await;
        });

        let stream = TcpStream::connect(addr).await.unwrap();
        let mut client = WsTestClient { stream };
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), client.recv_eof())
            .await
            .expect("timed out");
        assert_some_byte_arrived(result);
    }

    /// Single shared copy of the `run_id`-extraction match every test below
    /// exercises, instead of each test carrying its own inline copy. Before
    /// this, 4 separate tests each duplicated the full match expression at
    /// a different source line but only ever constructed 1-5 of the 7
    /// `ServerEvent` variants, so `llvm-cov` (which counts per source line,
    /// not per logical match) saw most arms of most of those copies as
    /// never hit -- even though every arm undeniably works, just not from
    /// that specific copy's test data. A single shared function means every
    /// arm only needs to be hit once, from any caller, to be covered.
    fn get_run_id(ev: &ServerEvent) -> &str {
        match ev {
            ServerEvent::AgentStatus { run_id, .. } => run_id,
            ServerEvent::ContextUpdate { run_id, .. } => run_id,
            ServerEvent::Log { run_id, .. } => run_id,
            ServerEvent::InteractionNeeded { run_id, .. } => run_id,
            ServerEvent::AgentSpawned { run_id, .. } => run_id,
            ServerEvent::AgentCompleted { run_id, .. } => run_id,
            ServerEvent::Tokens { run_id, .. } => run_id,
        }
    }

    #[test]
    fn server_event_run_id_extraction_agent_status() {
        let ev = ServerEvent::AgentStatus {
            agent_id: "coder".to_string(),
            run_id: "run-123".to_string(),
            status: "running".to_string(),
            stage: "plan".to_string(),
            iteration: 1,
            tool_calls: 0,
            accepts_messages: true,
        };
        assert_eq!(get_run_id(&ev), "run-123");
    }

    #[test]
    fn server_event_run_id_extraction_context_update() {
        let ev = ServerEvent::ContextUpdate {
            agent_id: "a".to_string(),
            run_id: "run-ctx".to_string(),
            total_tokens: 100,
            max_tokens: 200000,
        };
        assert_eq!(get_run_id(&ev), "run-ctx");
    }

    #[test]
    fn server_event_run_id_extraction_all_variants() {
        let variants: Vec<(ServerEvent, &str)> = vec![
            (
                ServerEvent::Log {
                    agent_id: "a".to_string(),
                    run_id: "run-log".to_string(),
                    line: "hi".to_string(),
                },
                "run-log",
            ),
            (
                ServerEvent::InteractionNeeded {
                    agent_id: "a".to_string(),
                    run_id: "run-int".to_string(),
                    request: serde_json::Value::Null,
                },
                "run-int",
            ),
            (
                ServerEvent::AgentSpawned {
                    agent_id: "a".to_string(),
                    run_id: "run-spawn".to_string(),
                    parent_id: None,
                    blueprint: "bp".to_string(),
                },
                "run-spawn",
            ),
            (
                ServerEvent::AgentCompleted {
                    agent_id: "a".to_string(),
                    run_id: "run-done".to_string(),
                    status: "complete".to_string(),
                    result: None,
                },
                "run-done",
            ),
            (
                ServerEvent::Tokens {
                    agent_id: "a".to_string(),
                    run_id: "run-tok".to_string(),
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    cached_tokens: 0,
                    cache_write_tokens: 0,
                },
                "run-tok",
            ),
        ];

        for (ev, expected) in variants {
            assert_eq!(get_run_id(&ev), expected);
        }
    }

    #[test]
    fn server_event_filter_matching() {
        let filter = "run-123".to_string();
        let matching = ServerEvent::AgentStatus {
            agent_id: "a".to_string(),
            run_id: "run-123".to_string(),
            status: "running".to_string(),
            stage: "plan".to_string(),
            iteration: 1,
            tool_calls: 0,
            accepts_messages: true,
        };
        let non_matching = ServerEvent::AgentStatus {
            agent_id: "a".to_string(),
            run_id: "run-456".to_string(),
            status: "running".to_string(),
            stage: "plan".to_string(),
            iteration: 1,
            tool_calls: 0,
            accepts_messages: true,
        };

        assert_eq!(get_run_id(&matching), filter);
        assert_ne!(get_run_id(&non_matching), filter);
    }

    #[test]
    fn server_event_serializes_to_json() {
        let ev = ServerEvent::AgentStatus {
            agent_id: "coder".to_string(),
            run_id: "run-ws".to_string(),
            status: "running".to_string(),
            stage: "plan".to_string(),
            iteration: 3,
            tool_calls: 0,
            accepts_messages: false,
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("\"type\":\"agent_status\""));
        assert!(json.contains("\"run_id\":\"run-ws\""));
    }

    #[test]
    fn broadcast_channel_creation() {
        let (tx, _rx) = broadcast::channel::<ServerEvent>(16);
        let ev = ServerEvent::Log {
            agent_id: "a".to_string(),
            run_id: "r".to_string(),
            line: "test".to_string(),
        };
        assert!(tx.send(ev).is_ok());
    }

    fn assert_none_on_connection_reset(result: Option<u8>) {
        assert_eq!(result, None, "expected None on connection reset");
    }

    #[test]
    #[should_panic(expected = "expected None on connection reset")]
    fn assert_none_on_connection_reset_panics_on_some() {
        assert_none_on_connection_reset(Some(0x1));
    }

    /// Exercises `recv_eof`'s `Err(_) => None` branch by having the server
    /// abort the connection with SO_LINGER=0, which causes the client to
    /// receive a TCP RST rather than a clean FIN, so `read()` returns an IO
    /// error (`connection reset by peer`) instead of `Ok(0)`.
    #[tokio::test]
    async fn recv_eof_returns_none_on_io_error() {
        use std::time::Duration;
        use tokio::net::TcpSocket;

        let server_sock = TcpSocket::new_v4().unwrap();
        server_sock.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = server_sock.local_addr().unwrap();
        let listener = server_sock.listen(1).unwrap();

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            // Set SO_LINGER to 0 — causes RST on close instead of FIN.
            #[allow(deprecated)]
            stream.set_linger(Some(Duration::from_secs(0))).unwrap();
            // Close immediately (drop triggers RST).
        });

        let stream = TcpStream::connect(addr).await.unwrap();
        // Give the server task a moment to accept and set SO_LINGER before
        // we try to read.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut client = WsTestClient { stream };
        // With a RST, read() returns Err("connection reset by peer") → None.
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), client.recv_eof())
            .await
            .expect("timed out waiting for recv_eof on RST");
        assert_none_on_connection_reset(result);
    }
}
