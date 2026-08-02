//! Interaction and message endpoints.

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::Json;
use leviath_core::interaction::{ApprovalScope, InteractionResponse};
use leviath_runtime::control_socket::{ControlRequest, ControlResponse};

use super::types::*;

/// `GET /api/agents/{id}/interaction`: the open interaction the daemon has for
/// this agent, if any (from the in-memory interaction hub).
pub(super) async fn get_interaction(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    match state
        .control
        .request(&ControlRequest::ListInteractions)
        .await
    {
        Ok(ControlResponse::Interactions { interactions }) => {
            match interactions
                .into_iter()
                .find(|(agent_id, _)| agent_id == &id)
            {
                Some((_, req)) => Ok(Json(
                    serde_json::to_value(&req).unwrap_or(serde_json::Value::Null),
                )),
                None => Err(err(
                    StatusCode::NOT_FOUND,
                    "No pending interaction".to_string(),
                )),
            }
        }
        Ok(other) => Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Unexpected daemon response: {other:?}"),
        )),
        Err(e) => Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("Daemon not reachable: {e}"),
        )),
    }
}

/// `POST /api/agents/{id}/interaction`: answer an open interaction. The request
/// id in the body selects the interaction (globally unique in the daemon).
pub(super) async fn submit_interaction(
    State(state): State<AppState>,
    AxumPath(_id): AxumPath<String>,
    Json(body): Json<SubmitInteractionReq>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    let scope = body.scope.as_deref().map(|s| match s {
        "session" => ApprovalScope::Session,
        _ => ApprovalScope::Once,
    });
    let response = InteractionResponse {
        request_id: body.request_id,
        value: body.value,
        choice_index: body.choice_index,
        approved: body.approved,
        scope,
    };
    match state
        .control
        .request(&ControlRequest::AnswerInteraction { response })
        .await
    {
        Ok(ControlResponse::Ok { ok: true }) => Ok(StatusCode::ACCEPTED),
        Ok(ControlResponse::Ok { ok: false }) => Err(err(
            StatusCode::NOT_FOUND,
            "No such open interaction".to_string(),
        )),
        Ok(other) => Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Unexpected daemon response: {other:?}"),
        )),
        Err(e) => Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("Daemon not reachable: {e}"),
        )),
    }
}

/// `POST /api/agents/{id}/message`: deliver a message to a running agent.
pub(super) async fn send_message(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<SendMessageReq>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    match state
        .control
        .request(&ControlRequest::Message {
            agent_id: id.clone(),
            content: body.message,
            target_region: body.target_region,
        })
        .await
    {
        Ok(ControlResponse::Ok { ok: true }) => Ok(StatusCode::ACCEPTED),
        Ok(ControlResponse::Ok { ok: false }) => Err(err(
            StatusCode::NOT_FOUND,
            format!("Agent run '{id}' is not accepting messages"),
        )),
        Ok(other) => Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Unexpected daemon response: {other:?}"),
        )),
        Err(e) => Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("Daemon not reachable: {e}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::serve::AppState;
    use crate::commands::serve::testutil::fake_daemon;
    use crate::config::Config;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::{get, post};
    use leviath_core::interaction::InteractionRequest;
    use leviath_runtime::control_socket::ControlClient;
    use std::sync::Arc;
    use tokio::sync::broadcast;
    use tower::ServiceExt;

    /// A router over the interaction/message routes, backed by `control`.
    fn app_with(control: ControlClient) -> Router {
        let (tx, _) = broadcast::channel(16);
        let state = AppState {
            config: Arc::new(Config::default()),
            event_tx: tx,
            control,
            mcp: crate::commands::serve::mcp::McpAdmin::default(),
            limits: Default::default(),
        };
        Router::new()
            .route(
                "/api/agents/{id}/interaction",
                get(get_interaction).post(submit_interaction),
            )
            .route("/api/agents/{id}/message", post(send_message))
            .with_state(state)
    }

    async fn status_of(app: Router, method: &str, uri: &str, body: Body) -> StatusCode {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(body)
            .unwrap();
        app.oneshot(req).await.unwrap().status()
    }

    /// A control client at an address with no daemon.
    fn no_daemon() -> ControlClient {
        ControlClient::new(leviath_runtime::control_socket::control_id(
            std::path::Path::new("/no/such/daemon"),
        ))
    }

    // ─── get_interaction ─────────────────────────────────────────────────────
    #[tokio::test]
    async fn get_interaction_returns_agents_pending_request() {
        let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Interactions {
            interactions: vec![(
                "a1".to_string(),
                InteractionRequest::free_text("q1", "prompt?", "stage", true),
            )],
        });
        assert_eq!(
            status_of(
                app_with(control),
                "GET",
                "/api/agents/a1/interaction",
                Body::empty()
            )
            .await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn get_interaction_no_match_is_404() {
        let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Interactions {
            interactions: vec![],
        });
        assert_eq!(
            status_of(
                app_with(control),
                "GET",
                "/api/agents/none/interaction",
                Body::empty()
            )
            .await,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn get_interaction_unexpected_is_500() {
        let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Ok { ok: true });
        assert_eq!(
            status_of(
                app_with(control),
                "GET",
                "/api/agents/a/interaction",
                Body::empty()
            )
            .await,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[tokio::test]
    async fn get_interaction_daemon_absent_is_503() {
        assert_eq!(
            status_of(
                app_with(no_daemon()),
                "GET",
                "/api/agents/a/interaction",
                Body::empty()
            )
            .await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    // ─── submit_interaction ──────────────────────────────────────────────────
    #[tokio::test]
    async fn submit_interaction_accepted_once_scope() {
        let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Ok { ok: true });
        assert_eq!(
            status_of(
                app_with(control),
                "POST",
                "/api/agents/a/interaction",
                Body::from(r#"{"request_id":"q1","scope":"once","value":"hi"}"#),
            )
            .await,
            StatusCode::ACCEPTED
        );
    }

    #[tokio::test]
    async fn submit_interaction_session_scope_not_found() {
        let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Ok { ok: false });
        assert_eq!(
            status_of(
                app_with(control),
                "POST",
                "/api/agents/a/interaction",
                Body::from(r#"{"request_id":"q1","scope":"session","approved":true}"#),
            )
            .await,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn submit_interaction_unexpected_is_500() {
        let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Spawned {
            run_id: "x".to_string(),
        });
        assert_eq!(
            status_of(
                app_with(control),
                "POST",
                "/api/agents/a/interaction",
                Body::from(r#"{"request_id":"q1"}"#),
            )
            .await,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[tokio::test]
    async fn submit_interaction_absent_is_503() {
        assert_eq!(
            status_of(
                app_with(no_daemon()),
                "POST",
                "/api/agents/a/interaction",
                Body::from(r#"{"request_id":"q1"}"#),
            )
            .await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    // ─── send_message ────────────────────────────────────────────────────────
    #[tokio::test]
    async fn send_message_delivered() {
        let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Ok { ok: true });
        assert_eq!(
            status_of(
                app_with(control),
                "POST",
                "/api/agents/a/message",
                Body::from(r#"{"message":"hi"}"#),
            )
            .await,
            StatusCode::ACCEPTED
        );
    }

    #[tokio::test]
    async fn send_message_not_accepting_is_404() {
        let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Ok { ok: false });
        assert_eq!(
            status_of(
                app_with(control),
                "POST",
                "/api/agents/a/message",
                Body::from(r#"{"message":"hi","target_region":"conversation"}"#),
            )
            .await,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn send_message_unexpected_is_500() {
        let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::List {
            runs: vec![],
            finished: vec![],
            health: Default::default(),
        });
        assert_eq!(
            status_of(
                app_with(control),
                "POST",
                "/api/agents/a/message",
                Body::from(r#"{"message":"hi"}"#),
            )
            .await,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[tokio::test]
    async fn send_message_absent_is_503() {
        assert_eq!(
            status_of(
                app_with(no_daemon()),
                "POST",
                "/api/agents/a/message",
                Body::from(r#"{"message":"hi"}"#),
            )
            .await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
}
