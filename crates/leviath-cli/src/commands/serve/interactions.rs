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
        Err(e) => Err(daemon_error(e)),
    }
}

/// Read an approval scope off the wire.
///
/// `session` is the name every existing client sends for run scope, so it stays
/// the accepted spelling. Anything unrecognised narrows to `once`: a typo in a
/// request body must not widen a grant, and rejecting the request outright would
/// turn a harmless mistake into a stalled run.
fn approval_scope_from_wire(s: &str) -> ApprovalScope {
    match s {
        "session" | "run" => ApprovalScope::Run,
        "stage" => ApprovalScope::Stage,
        _ => ApprovalScope::Once,
    }
}

/// `POST /api/agents/{id}/interaction`: answer an open interaction. The request
/// id in the body selects the interaction (globally unique in the daemon).
pub(super) async fn submit_interaction(
    State(state): State<AppState>,
    AxumPath(_id): AxumPath<String>,
    Json(body): Json<SubmitInteractionReq>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    let scope = body.scope.as_deref().map(approval_scope_from_wire);
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
        Err(e) => Err(daemon_error(e)),
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
        Err(e) => Err(daemon_error(e)),
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
    use tokio::sync::broadcast;
    use tower::ServiceExt;

    /// A router over the interaction/message routes, backed by `control`.
    fn app_with(control: ControlClient) -> Router {
        let (tx, _) = broadcast::channel(16);
        let state = AppState {
            config: crate::commands::serve::testutil::fixed_config(Config::default()),
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

    /// A daemon updated under a running server, answering with something this
    /// server cannot read: not a 503 (retrying, or restarting the daemon,
    /// cannot help) but a 502 naming the process that needs restarting.
    #[tokio::test]
    async fn a_daemon_on_other_code_that_cannot_be_read_is_a_502_naming_the_fix() {
        use leviath_runtime::control_socket::{
            ControlToken, DaemonIdentity, bind_control_listener, control_id,
        };
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let dir = tempfile::tempdir().unwrap();
        let id = control_id(dir.path());
        let mut listener = bind_control_listener(&id).unwrap();
        let _token = ControlToken::create(dir.path()).unwrap();
        let mut updated = DaemonIdentity::this_process("this-build");
        updated.build = "newer-build".to_string();
        let server = tokio::spawn(async move {
            let stream = listener
                .accept()
                .await
                .expect("accept succeeds")
                .expect("our own connection is admitted");
            let (read_half, mut write_half) = tokio::io::split(stream);
            let mut lines = BufReader::new(read_half).lines();
            let _hello = lines.next_line().await.unwrap();
            let mut welcome =
                serde_json::to_string(&ControlResponse::Welcome { daemon: updated }).unwrap();
            welcome.push('\n');
            let _ = write_half.write_all(welcome.as_bytes()).await;
            let _request = lines.next_line().await.unwrap();
            let _ = write_half
                .write_all(b"{\"result\":\"from_the_future\"}\n")
                .await;
        });
        let control = ControlClient::for_home(id, dir.path()).with_build("this-build");
        let req = Request::builder()
            .method("GET")
            .uri("/api/agents/a1/interaction")
            .body(Body::empty())
            .unwrap();
        let response = app_with(control).oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("This server needs a restart"), "{text}");
        assert!(text.contains("newer-build"), "{text}");
        server.await.unwrap();
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

    /// A typo must narrow to `once`, never widen a grant, and `session` has to
    /// keep meaning run scope because that is what every client sends.
    #[test]
    fn a_wire_scope_never_widens_beyond_what_it_names() {
        assert_eq!(approval_scope_from_wire("session"), ApprovalScope::Run);
        assert_eq!(approval_scope_from_wire("run"), ApprovalScope::Run);
        assert_eq!(approval_scope_from_wire("stage"), ApprovalScope::Stage);
        assert_eq!(approval_scope_from_wire("once"), ApprovalScope::Once);
        assert_eq!(approval_scope_from_wire("sesion"), ApprovalScope::Once);
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
