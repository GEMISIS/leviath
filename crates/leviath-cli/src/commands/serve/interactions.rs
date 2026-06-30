//! Interaction and message endpoints.

use axum::extract::Path as AxumPath;
use axum::http::StatusCode;
use axum::response::Json;

use super::types::*;
use crate::interaction;
use crate::runstate::{self, RunStatus};

pub(super) async fn get_interaction(
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let run_dir = runstate::run_dir(&id);
    if !run_dir.exists() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Agent run '{}' not found", id),
            }),
        ));
    }

    match interaction::read_request(&id) {
        Some(req) => {
            let val = serde_json::to_value(&req).unwrap_or(serde_json::Value::Null);
            Ok(Json(val))
        }
        None => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "No pending interaction".to_string(),
            }),
        )),
    }
}

pub(super) async fn submit_interaction(
    AxumPath(id): AxumPath<String>,
    Json(body): Json<SubmitInteractionReq>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    let run_dir = runstate::run_dir(&id);
    if !run_dir.exists() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Agent run '{}' not found", id),
            }),
        ));
    }

    let scope = body.scope.as_deref().map(|s| match s {
        "session" => interaction::ApprovalScope::Session,
        _ => interaction::ApprovalScope::Once,
    });

    let resp = interaction::InteractionResponse {
        request_id: body.request_id,
        value: body.value,
        choice_index: body.choice_index,
        approved: body.approved,
        scope,
    };

    interaction::write_response(&id, &resp).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Failed to write interaction response: {}", e),
            }),
        )
    })?;

    Ok(StatusCode::ACCEPTED)
}

pub(super) async fn send_message(
    AxumPath(id): AxumPath<String>,
    Json(body): Json<SendMessageReq>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    let meta = runstate::read_meta(&id).map_err(|_| {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Agent run '{}' not found", id),
            }),
        )
    })?;

    // Write the message as a pending interaction response if the agent is waiting
    if meta.status == RunStatus::WaitingInput || meta.status == RunStatus::CompleteInteractive {
        if let Some(req) = interaction::read_request(&id) {
            let resp = interaction::InteractionResponse::text(&req.id, &body.message);
            interaction::write_response(&id, &resp).map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse {
                        error: format!("Failed to write response: {}", e),
                    }),
                )
            })?;
            return Ok(StatusCode::ACCEPTED);
        }
    }

    // If not waiting, append to the run's output log as a user message
    runstate::append_stage_output(
        &id,
        meta.stage_index,
        &format!("[User message]: {}", body.message),
    );

    Ok(StatusCode::ACCEPTED)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::{get, post};
    use axum::Router;
    use tower::ServiceExt;

    use crate::runstate::{create_run, RunMeta, RunStatus};

    fn unique_run_id(prefix: &str) -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        format!("test-int-{}-{}-{}", prefix, std::process::id(), nanos)
    }

    fn make_run(id: &str) -> RunMeta {
        RunMeta::new(
            id.to_string(),
            "test-agent".to_string(),
            "/path".to_string(),
            "task".to_string(),
            None,
            "/tmp".to_string(),
            1,
        )
    }

    // ─── get_interaction ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn get_interaction_run_not_found_returns_404() {
        let app = Router::new().route("/api/agents/{id}/interaction", get(get_interaction));
        let req = Request::builder()
            .uri("/api/agents/nonexistent-run-interact/interaction")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn get_interaction_no_pending_returns_404() {
        let run_id = unique_run_id("get-int-none");
        let meta = make_run(&run_id);
        create_run(&meta).unwrap();

        let app = Router::new().route("/api/agents/{id}/interaction", get(get_interaction));
        let req = Request::builder()
            .uri(format!("/api/agents/{}/interaction", run_id))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);

        let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
    }

    #[tokio::test]
    async fn get_interaction_with_pending_returns_ok() {
        let run_id = unique_run_id("get-int-ok");
        let meta = make_run(&run_id);
        create_run(&meta).unwrap();

        // Write a pending interaction request
        let req_val = interaction::InteractionRequest::free_text(
            "req-001",
            "What should I do?",
            "plan",
            true,
        );
        interaction::write_request(&run_id, &req_val).unwrap();

        let app = Router::new().route("/api/agents/{id}/interaction", get(get_interaction));
        let req = Request::builder()
            .uri(format!("/api/agents/{}/interaction", run_id))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let val: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(val["id"], "req-001");

        let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
    }

    // ─── submit_interaction ───────────────────────────────────────────────────

    #[tokio::test]
    async fn submit_interaction_run_not_found_returns_404() {
        let app = Router::new().route("/api/agents/{id}/interaction", post(submit_interaction));
        let body = serde_json::json!({"request_id": "req-001"});
        let req = Request::builder()
            .method("POST")
            .uri("/api/agents/nonexistent-run-submit/interaction")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn submit_interaction_once_scope_returns_accepted() {
        let run_id = unique_run_id("submit-once");
        let meta = make_run(&run_id);
        create_run(&meta).unwrap();

        let app = Router::new().route("/api/agents/{id}/interaction", post(submit_interaction));
        let body = serde_json::json!({
            "request_id": "req-001",
            "value": "yes",
            "approved": true,
            "scope": "once"
        });
        let req = Request::builder()
            .method("POST")
            .uri(format!("/api/agents/{}/interaction", run_id))
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::ACCEPTED);

        let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
    }

    #[tokio::test]
    async fn submit_interaction_session_scope_returns_accepted() {
        let run_id = unique_run_id("submit-session");
        let meta = make_run(&run_id);
        create_run(&meta).unwrap();

        let app = Router::new().route("/api/agents/{id}/interaction", post(submit_interaction));
        let body = serde_json::json!({
            "request_id": "req-002",
            "approved": true,
            "scope": "session"
        });
        let req = Request::builder()
            .method("POST")
            .uri(format!("/api/agents/{}/interaction", run_id))
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::ACCEPTED);

        let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
    }

    #[tokio::test]
    async fn submit_interaction_no_scope_returns_accepted() {
        let run_id = unique_run_id("submit-noscope");
        let meta = make_run(&run_id);
        create_run(&meta).unwrap();

        let app = Router::new().route("/api/agents/{id}/interaction", post(submit_interaction));
        let body = serde_json::json!({
            "request_id": "req-003",
            "value": "do it"
        });
        let req = Request::builder()
            .method("POST")
            .uri(format!("/api/agents/{}/interaction", run_id))
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::ACCEPTED);

        let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
    }

    // ─── send_message ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn send_message_run_not_found_returns_404() {
        let app = Router::new().route("/api/agents/{id}/message", post(send_message));
        let body = serde_json::json!({"message": "hello"});
        let req = Request::builder()
            .method("POST")
            .uri("/api/agents/nonexistent-run-msg/message")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn send_message_running_agent_appends_to_log() {
        let run_id = unique_run_id("msg-running");
        let mut meta = make_run(&run_id);
        meta.status = RunStatus::Running;
        create_run(&meta).unwrap();

        let app = Router::new().route("/api/agents/{id}/message", post(send_message));
        let body = serde_json::json!({"message": "keep going"});
        let req = Request::builder()
            .method("POST")
            .uri(format!("/api/agents/{}/message", run_id))
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::ACCEPTED);

        let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
    }

    #[tokio::test]
    async fn send_message_waiting_input_with_pending_writes_response() {
        let run_id = unique_run_id("msg-waiting");
        let mut meta = make_run(&run_id);
        meta.status = RunStatus::WaitingInput;
        create_run(&meta).unwrap();

        // Create a pending interaction request
        let req_val = interaction::InteractionRequest::free_text(
            "req-wait-001",
            "What should I do?",
            "plan",
            true,
        );
        interaction::write_request(&run_id, &req_val).unwrap();

        let app = Router::new().route("/api/agents/{id}/message", post(send_message));
        let body = serde_json::json!({"message": "do the thing"});
        let req = Request::builder()
            .method("POST")
            .uri(format!("/api/agents/{}/message", run_id))
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::ACCEPTED);

        // Verify a response was written
        let response = interaction::take_response(&run_id);
        assert!(response.is_some(), "response should have been written");
        assert_eq!(response.unwrap().value.as_deref(), Some("do the thing"));

        let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
    }

    #[tokio::test]
    async fn send_message_complete_interactive_with_pending_writes_response() {
        let run_id = unique_run_id("msg-complete-interactive");
        let mut meta = make_run(&run_id);
        meta.status = RunStatus::CompleteInteractive;
        create_run(&meta).unwrap();

        // Create a pending interaction request
        let req_val = interaction::InteractionRequest::free_text(
            "req-ci-001",
            "Optional follow-up?",
            "result",
            false,
        );
        interaction::write_request(&run_id, &req_val).unwrap();

        let app = Router::new().route("/api/agents/{id}/message", post(send_message));
        let body = serde_json::json!({"message": "no thanks"});
        let req = Request::builder()
            .method("POST")
            .uri(format!("/api/agents/{}/message", run_id))
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::ACCEPTED);

        let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
    }

    #[tokio::test]
    async fn send_message_waiting_input_no_pending_appends_to_log() {
        let run_id = unique_run_id("msg-waiting-nopend");
        let mut meta = make_run(&run_id);
        meta.status = RunStatus::WaitingInput;
        // No pending request written
        create_run(&meta).unwrap();

        let app = Router::new().route("/api/agents/{id}/message", post(send_message));
        let body = serde_json::json!({"message": "hello there"});
        let req = Request::builder()
            .method("POST")
            .uri(format!("/api/agents/{}/message", run_id))
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::ACCEPTED);

        let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
    }

    #[test]
    fn submit_interaction_req_deserialization_minimal() {
        let json = r#"{"request_id": "req-001"}"#;
        let req: SubmitInteractionReq = serde_json::from_str(json).unwrap();
        assert_eq!(req.request_id, "req-001");
        assert!(req.value.is_none());
        assert!(req.choice_index.is_none());
        assert!(req.approved.is_none());
        assert!(req.scope.is_none());
    }

    #[test]
    fn submit_interaction_req_deserialization_full() {
        let json = r#"{
            "request_id": "req-002",
            "value": "yes please",
            "choice_index": 1,
            "approved": true,
            "scope": "session"
        }"#;
        let req: SubmitInteractionReq = serde_json::from_str(json).unwrap();
        assert_eq!(req.request_id, "req-002");
        assert_eq!(req.value.unwrap(), "yes please");
        assert_eq!(req.choice_index, Some(1));
        assert_eq!(req.approved, Some(true));
        assert_eq!(req.scope.unwrap(), "session");
    }

    #[test]
    fn send_message_req_deserialization() {
        let json = r#"{"message": "hello agent"}"#;
        let req: SendMessageReq = serde_json::from_str(json).unwrap();
        assert_eq!(req.message, "hello agent");
        assert!(req.target_region.is_none());
    }

    #[test]
    fn send_message_req_with_target_region() {
        let json = r#"{"message": "hi", "target_region": "conversation"}"#;
        let req: SendMessageReq = serde_json::from_str(json).unwrap();
        assert_eq!(req.message, "hi");
        assert_eq!(req.target_region.unwrap(), "conversation");
    }

    #[test]
    fn scope_mapping_session() {
        let scope_str = "session";
        let scope = match scope_str {
            "session" => interaction::ApprovalScope::Session,
            _ => interaction::ApprovalScope::Once,
        };
        assert_eq!(scope, interaction::ApprovalScope::Session);
    }

    #[test]
    fn scope_mapping_defaults_to_once() {
        let scope_str = "anything_else";
        let scope = match scope_str {
            "session" => interaction::ApprovalScope::Session,
            _ => interaction::ApprovalScope::Once,
        };
        assert_eq!(scope, interaction::ApprovalScope::Once);
    }
}
