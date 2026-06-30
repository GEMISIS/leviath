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
