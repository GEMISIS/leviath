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
