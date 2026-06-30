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
