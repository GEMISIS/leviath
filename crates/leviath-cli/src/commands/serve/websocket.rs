//! WebSocket endpoints and connection handling.

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path as AxumPath, State, WebSocketUpgrade};
use axum::response::IntoResponse;
use tokio::sync::broadcast;
use tracing::warn;

use super::types::*;

pub(super) async fn ws_global(
    State(state): State<AppState>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, state.event_tx, None))
}

pub(super) async fn ws_agent(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, state.event_tx, Some(id)))
}

async fn handle_ws(
    mut socket: WebSocket,
    event_tx: broadcast::Sender<ServerEvent>,
    filter_run_id: Option<String>,
) {
    let mut rx = event_tx.subscribe();

    loop {
        tokio::select! {
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

                        if let Ok(json) = serde_json::to_string(&ev) {
                            if socket.send(Message::Text(json.into())).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!("WebSocket subscriber lagged by {} events", n);
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

    /// Minimal hand-rolled WebSocket client used to drive `handle_ws` end to
    /// end over a real TCP loopback connection. No `tokio-tungstenite` or
    /// other WS crate is added as a dependency — this speaks just enough of
    /// RFC 6455 to perform the opening handshake and exchange text/close
    /// frames with axum's server-side WebSocket implementation.
    struct WsTestClient {
        stream: TcpStream,
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
                assert_ne!(n, 0, "connection closed before handshake completed");
                buf.push(byte[0]);
                if buf.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            let response = String::from_utf8_lossy(&buf);
            assert!(
                response.starts_with("HTTP/1.1 101"),
                "expected 101 Switching Protocols, got: {response}"
            );

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

    async fn spawn_test_server(state: AppState) -> std::net::SocketAddr {
        let app = Router::new()
            .route("/ws", get(ws_global))
            .route("/ws/agents/{id}", get(ws_agent))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        addr
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
        assert_eq!(opcode, 0x1, "expected a text frame");
        let text = String::from_utf8(payload).unwrap();
        assert!(text.contains("\"type\":\"log\""));
        assert!(text.contains("\"run_id\":\"run-1\""));

        // Close so the server-side handle_ws task terminates instead of
        // idling forever on rx.recv() for the rest of the process lifetime.
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
        assert_eq!(eof, None, "expected clean EOF after server processed close");
    }

    #[tokio::test]
    async fn ws_global_lagged_receiver_does_not_crash_connection() {
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

    #[test]
    fn server_event_run_id_extraction_agent_status() {
        let ev = ServerEvent::AgentStatus {
            agent_id: "coder".to_string(),
            run_id: "run-123".to_string(),
            status: "running".to_string(),
            stage: "plan".to_string(),
            iteration: 1,
            accepts_messages: true,
        };
        let run_id = match &ev {
            ServerEvent::AgentStatus { run_id, .. } => run_id,
            ServerEvent::ContextUpdate { run_id, .. } => run_id,
            ServerEvent::Log { run_id, .. } => run_id,
            ServerEvent::InteractionNeeded { run_id, .. } => run_id,
            ServerEvent::AgentSpawned { run_id, .. } => run_id,
            ServerEvent::AgentCompleted { run_id, .. } => run_id,
            ServerEvent::Tokens { run_id, .. } => run_id,
        };
        assert_eq!(run_id, "run-123");
    }

    #[test]
    fn server_event_run_id_extraction_context_update() {
        let ev = ServerEvent::ContextUpdate {
            agent_id: "a".to_string(),
            run_id: "run-ctx".to_string(),
            total_tokens: 100,
            max_tokens: 200000,
        };
        let run_id = match &ev {
            ServerEvent::AgentStatus { run_id, .. } => run_id,
            ServerEvent::ContextUpdate { run_id, .. } => run_id,
            ServerEvent::Log { run_id, .. } => run_id,
            ServerEvent::InteractionNeeded { run_id, .. } => run_id,
            ServerEvent::AgentSpawned { run_id, .. } => run_id,
            ServerEvent::AgentCompleted { run_id, .. } => run_id,
            ServerEvent::Tokens { run_id, .. } => run_id,
        };
        assert_eq!(run_id, "run-ctx");
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
                },
                "run-tok",
            ),
        ];

        for (ev, expected) in variants {
            let run_id = match &ev {
                ServerEvent::AgentStatus { run_id, .. } => run_id,
                ServerEvent::ContextUpdate { run_id, .. } => run_id,
                ServerEvent::Log { run_id, .. } => run_id,
                ServerEvent::InteractionNeeded { run_id, .. } => run_id,
                ServerEvent::AgentSpawned { run_id, .. } => run_id,
                ServerEvent::AgentCompleted { run_id, .. } => run_id,
                ServerEvent::Tokens { run_id, .. } => run_id,
            };
            assert_eq!(run_id, expected);
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
            accepts_messages: true,
        };
        let non_matching = ServerEvent::AgentStatus {
            agent_id: "a".to_string(),
            run_id: "run-456".to_string(),
            status: "running".to_string(),
            stage: "plan".to_string(),
            iteration: 1,
            accepts_messages: true,
        };

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
}
