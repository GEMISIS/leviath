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
