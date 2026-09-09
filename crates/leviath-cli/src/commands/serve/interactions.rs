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
) -> Result<Json<serde_json::Value>, ApiError> {
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
        Ok(other) => Err(unexpected_response(other)),
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
) -> Result<StatusCode, ApiError> {
    if body.approved == Some(true) && body.feedback.is_some() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "feedback goes with a deny: send it with \"approved\": false, or drop it to approve"
                .to_string(),
        ));
    }
    let scope = body.scope.as_deref().map(approval_scope_from_wire);
    let response = InteractionResponse {
        request_id: body.request_id,
        value: body.value,
        choice_index: body.choice_index,
        approved: body.approved,
        scope,
        feedback: body.feedback,
    };
    let reply = state
        .control
        .request(&ControlRequest::AnswerInteraction { response })
        .await;
    daemon_ok(
        reply,
        StatusCode::ACCEPTED,
        "No such open interaction".to_string(),
    )
}

/// `POST /api/agents/{id}/message`: deliver a message to a running agent.
pub(super) async fn send_message(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    request: axum::extract::Request,
) -> Result<StatusCode, ApiError> {
    let max_upload = state.limits.request_limits.max_upload_bytes;
    let (mut body, mut parts): (SendMessageReq, _) =
        super::upload::json_or_multipart(&state, request, max_upload).await?;
    // Files named inside the run's workdir, by `parts` or by `@path` in the
    // text. Only a run this API can see has a workdir to resolve against;
    // a message to one it cannot still goes through, with its uploads.
    if let Ok(meta) = crate::runstate::read_meta(&id) {
        let workdir = std::path::Path::new(&meta.workdir);
        parts.extend(super::upload::json_parts(&body.parts, workdir, max_upload)?);
        let (kept, named) = super::upload::inline_parts(&body.message, None, workdir, max_upload)?;
        body.message = kept;
        parts.extend(named);
    } else if !body.parts.is_empty() {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!("Agent run '{id}' has no working directory this server can read parts from"),
        ));
    }
    let reply = state
        .control
        .request(&ControlRequest::Message {
            agent_id: id.clone(),
            content: body.message,
            target_region: body.target_region,
            parts,
        })
        .await;
    daemon_ok(
        reply,
        StatusCode::ACCEPTED,
        format!("Agent run '{id}' is not accepting messages"),
    )
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
            update_check: Default::default(),
            update_jobs: Default::default(),
            config: crate::commands::serve::testutil::fixed_config(Config::default()),
            event_tx: tx,
            control,
            mcp: crate::commands::serve::mcp::McpAdmin::default(),
            providers: crate::commands::serve::providers::ProviderAdmin::default(),
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

    /// Post `body` to a fresh one-request daemon: the status, and the control
    /// request the daemon saw, as JSON text (empty when the server refused the
    /// body before asking).
    async fn answered_with(body: &'static str) -> (StatusCode, String) {
        use std::sync::{Arc, Mutex};
        let seen: Arc<Mutex<String>> = Arc::default();
        let sink = seen.clone();
        let (control, _dir, _srv) = fake_daemon(move |req| {
            *sink.lock().unwrap() = serde_json::to_string(&req).unwrap();
            ControlResponse::Ok { ok: true }
        });
        let status = status_of(
            app_with(control),
            "POST",
            "/api/agents/a/interaction",
            Body::from(body),
        )
        .await;
        let seen = std::mem::take(&mut *seen.lock().unwrap());
        (status, seen)
    }

    /// The three body shapes on a deny: bare, with feedback, and feedback on a
    /// grant. The first two reach the daemon as the response they name; the
    /// third is refused before the daemon sees it.
    #[tokio::test]
    async fn submit_interaction_feedback_shapes() {
        let (status, seen) = answered_with(r#"{"request_id":"q1","approved":false}"#).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert!(seen.contains(r#""approved":false"#), "{seen}");
        assert!(!seen.contains("feedback"), "absent stays absent: {seen}");

        let (status, seen) = answered_with(
            r#"{"request_id":"q1","approved":false,"feedback":"use the API instead"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert!(seen.contains(r#""approved":false"#), "{seen}");
        assert!(
            seen.contains(r#""feedback":"use the API instead""#),
            "{seen}"
        );

        let (status, seen) =
            answered_with(r#"{"request_id":"q1","approved":true,"feedback":"why"}"#).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(seen.is_empty(), "the 400 never reached the daemon: {seen}");
    }

    /// The 400 says what to change.
    #[tokio::test]
    async fn feedback_on_a_grant_names_the_fix() {
        // No daemon: the refusal happens before one would be asked.
        let req = Request::builder()
            .method("POST")
            .uri("/api/agents/a/interaction")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"request_id":"q1","approved":true,"feedback":"why"}"#,
            ))
            .unwrap();
        let resp = app_with(no_daemon()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("feedback goes with a deny"), "{text}");
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

    /// A message with files: what the daemon receives on the wire.
    async fn message_seen(
        run_id: &str,
        content_type: &str,
        body: Vec<u8>,
    ) -> (StatusCode, Option<serde_json::Value>) {
        let captured = std::sync::Arc::new(std::sync::Mutex::new(None));
        let sink = std::sync::Arc::clone(&captured);
        let (control, _dir, _srv) = fake_daemon(move |req| {
            *sink.lock().unwrap() = Some(serde_json::to_value(&req).unwrap());
            ControlResponse::Ok { ok: true }
        });
        let req = Request::builder()
            .method("POST")
            .uri(format!("/api/agents/{run_id}/message"))
            .header("content-type", content_type)
            .body(Body::from(body))
            .unwrap();
        let status = app_with(control).oneshot(req).await.unwrap().status();
        let seen = captured.lock().unwrap().take();
        (status, seen)
    }

    #[tokio::test]
    async fn a_message_carries_files_named_in_the_workdir_or_uploaded() {
        crate::runstate::with_isolated_runs_dir_async("message_carries_files", |_d| async move {
            let workdir = tempfile::tempdir().unwrap();
            std::fs::write(workdir.path().join("mark.png"), b"\x89PNG\r\n\x1a\nmark").unwrap();
            let run_id = "msg-run";
            let meta = leviath_core::run_meta::RunMeta::new(
                run_id.to_string(),
                "a".to_string(),
                "/p".to_string(),
                "t".to_string(),
                None,
                workdir.path().to_string_lossy().to_string(),
                1,
            );
            crate::runstate::create_run(&meta).unwrap();

            let body = b"{\"message\":\"see @mark.png\",\"parts\":[{\"path\":\"mark.png\",\"region\":\"art\"}]}";
            let (status, seen) = message_seen(run_id, "application/json", body.to_vec()).await;
            assert_eq!(status, StatusCode::ACCEPTED);
            let seen = seen.expect("delivered");
            assert_eq!(seen["content"], "see @mark.png");
            let parts = seen["parts"].as_array().unwrap();
            assert_eq!(parts.len(), 2);
            assert_eq!(parts[0]["region"], "art");
            assert!(parts[1].get("region").is_none());

            let boundary = "levboundary";
            let body = format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"request\"\r\n\r\n\
                 {{\"message\":\"look\"}}\r\n--{boundary}\r\nContent-Disposition: form-data; \
                 name=\"part\"; filename=\"up.png\"\r\nContent-Type: image/png\r\n\r\nbytes\r\n\
                 --{boundary}--\r\n"
            );
            let (status, seen) = message_seen(
                run_id,
                &format!("multipart/form-data; boundary={boundary}"),
                body.into_bytes(),
            )
            .await;
            assert_eq!(status, StatusCode::ACCEPTED);
            let parts = seen.unwrap()["parts"].as_array().unwrap().clone();
            assert_eq!(parts[0]["name"], "up.png");

            // A workdir part that cannot be read, a mention of one, and a
            // body that is no form at all each fail before the daemon hears.
            std::fs::write(workdir.path().join("empty.png"), b"").unwrap();
            for (content_type, body) in [
                (
                    "application/json",
                    b"{\"message\":\"hi\",\"parts\":[{\"path\":\"missing.png\"}]}".to_vec(),
                ),
                ("application/json", b"{\"message\":\"see @empty.png\"}".to_vec()),
                ("multipart/form-data; boundary=b", b"garbage".to_vec()),
            ] {
                let (status, seen) = message_seen(run_id, content_type, body).await;
                assert_eq!(status, StatusCode::BAD_REQUEST);
                assert!(seen.is_none());
            }

            // A run this server cannot see: uploads still go, workdir parts do
            // not.
            let (status, seen) =
                message_seen("ghost", "application/json", b"{\"message\":\"hi\"}".to_vec()).await;
            assert_eq!(status, StatusCode::ACCEPTED);
            assert!(seen.is_some());
            let (status, _) = message_seen(
                "ghost",
                "application/json",
                b"{\"message\":\"hi\",\"parts\":[{\"path\":\"x\"}]}".to_vec(),
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND);
        })
        .await;
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
