//! Starting a run over HTTP: `POST /api/runs`, its dry run
//! `POST /api/runs/validate`, the schema both take, and the inputs a
//! blueprint declares.
//!
//! Both take a [`SpawnRequest`] as their body, as JSON, or as
//! `multipart/form-data` with the JSON in a `request` field and each file in a
//! field of its own. A file field becomes one of the request's `attachments`,
//! named after the field: a `file` input names it by that name. Its
//! `Content-Type` is its mime type, unless it is `application/octet-stream`,
//! which leaves the type to be read from the bytes.
//!
//! A request the daemon or this server refuses answers 422 with every problem
//! at once, each with its path, what was expected, what arrived and how to fix
//! it. A body that is not a request at all (the wrong JSON shape, an unknown
//! key) is a refusal too, with the one problem the decoder could name.

use axum::extract::multipart::Multipart;
use axum::extract::{FromRequest, Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use leviath_runtime::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
use leviath_runtime::spec::names::MimePattern;
use leviath_runtime::spec::request::{Attachment, Bytes, SpawnRequest};

use super::core::error::ServeError;
use super::core::spawn::{self as spawn_core, Verdict};
use super::types::*;

/// `POST /api/runs`: start a run. 201 with its id, or 422 with every reason
/// it cannot start.
pub(super) async fn spawn_run(
    State(state): State<AppState>,
    request: axum::extract::Request,
) -> Result<Response, ServeError> {
    let request = match read_request(&state, request).await? {
        Ok(request) => request,
        Err(issues) => return Ok(refusal(issues)),
    };
    match spawn_core::start(&state, request).await {
        Ok(Verdict::Accepted(started)) => Ok((
            StatusCode::CREATED,
            Json(SpawnedResp {
                run_id: started.run_id,
                warnings: IssuesResp::from(started.warnings).issues,
                seq: started.seq,
                server_instance: super::events::server_instance().to_string(),
            }),
        )
            .into_response()),
        Ok(Verdict::Rejected(issues)) => Ok(refusal(issues)),
        Err(e) => Err(e),
    }
}

/// `POST /api/runs/validate`: resolve a run without starting it. 200 with a
/// summary of the run it would be, or 422 with every reason it would be
/// refused.
pub(super) async fn validate_run(
    State(state): State<AppState>,
    request: axum::extract::Request,
) -> Result<Response, ServeError> {
    let request = match read_request(&state, request).await? {
        Ok(request) => request,
        Err(issues) => return Ok(refusal(issues)),
    };
    match spawn_core::validate(&state, request).await {
        Ok(Verdict::Accepted(summary)) => Ok((StatusCode::OK, Json(summary)).into_response()),
        Ok(Verdict::Rejected(issues)) => Ok(refusal(issues)),
        Err(e) => Err(e),
    }
}

/// A refused spawn: 422 with every issue.
fn refusal(issues: SpawnIssues) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(IssuesResp::from(issues)),
    )
        .into_response()
}

/// `GET /api/schema/spawn-request`: the JSON Schema of the request
/// `POST /api/runs` takes, for outside tools and agents to write one against.
pub(super) async fn spawn_request_schema() -> Json<serde_json::Value> {
    Json(leviath_runtime::runfile::spawn_request_schema())
}

/// `GET /api/blueprints/{name}/inputs`: the inputs a blueprint declares, each
/// with its type, whether it is required, its default and where it goes.
pub(super) async fn blueprint_inputs(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<Vec<leviath_runtime::spec::inputs::InputDecl>>, ServeError> {
    declared_inputs(&state, &name).await.map(Json)
}

/// The inputs the blueprint `name` declares, read from the installed
/// blueprints and the configured `agent_paths`. 404 for a blueprint that is
/// in none of them, 422 for one whose graph does not hold together.
pub(super) async fn declared_inputs(
    state: &AppState,
    name: &str,
) -> Result<Vec<leviath_runtime::spec::inputs::InputDecl>, ServeError> {
    let roots = super::blueprints::blueprint_roots(&*state.current_config().await);
    let name = name.to_string();
    // The blueprints are found and the one named is checked on the blocking
    // pool.
    super::blocking::blocking(move || {
        let found = super::blueprints::discover_in(roots)
            .into_iter()
            .find(|blueprint| blueprint.name == name)
            .ok_or_else(|| ServeError::NotFound(format!("Blueprint '{name}' not found")))?;
        // Checked as a spawn would check it: inputs read off a graph that does
        // not hold together would offer a form for a run that cannot start.
        let loaded = leviath_blueprint::validate(std::path::Path::new(&found.path))
            .map_err(|e| ServeError::Unprocessable(e.to_string()))?;
        Ok(loaded.graph.inputs)
    })
    .await
}

/// The request a body carries: `Ok(Ok)` for a request, `Ok(Err)` for a body
/// that is not one (refused like any other request), and `Err` for a body
/// that could not be read at all.
async fn read_request(
    state: &AppState,
    request: axum::extract::Request,
) -> Result<Result<SpawnRequest, SpawnIssues>, ServeError> {
    let max_bytes = state.limits.request_limits.max_upload_bytes;
    let is_multipart = request
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.to_ascii_lowercase().starts_with("multipart/form-data"));
    if !is_multipart {
        let body = axum::body::Bytes::from_request(request, state)
            .await
            .map_err(|e| super::upload::unreadable(e.status(), e.body_text()))?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|e| ServeError::BadRequest(format!("the body is not JSON: {e}")))?;
        return Ok(decode(value));
    }
    let multipart = Multipart::from_request(request, state)
        .await
        .map_err(|e| super::upload::unreadable(e.status(), e.body_text()))?;
    let (value, attachments) = read_multipart(multipart, max_bytes).await?;
    Ok(decode(value).map(|mut request| {
        request.attachments.extend(attachments);
        request
    }))
}

/// A JSON value as a request, or the one issue that says why it is not one.
fn decode(value: serde_json::Value) -> Result<SpawnRequest, SpawnIssues> {
    serde_json::from_value(value).map_err(|e| {
        SpawnIssues::from(
            SpawnIssue::new(SpecPath::root(), IssueCode::Invalid, e.to_string())
                .hint("GET /api/schema/spawn-request for the request's shape"),
        )
    })
}

/// A multipart body: the `request` field's JSON, and every other field as an
/// attachment named after it.
async fn read_multipart(
    multipart: Multipart,
    max_bytes: u64,
) -> Result<(serde_json::Value, Vec<Attachment>), ServeError> {
    let (request, files) = super::upload::read_envelope(multipart, max_bytes).await?;
    let attachments = files
        .into_iter()
        .map(|file| {
            Ok(Attachment {
                mime_type: file
                    .content_type
                    .as_deref()
                    .map(|t| {
                        MimePattern::new(t).map_err(|e| super::upload::bad_type(&file.name, t, e))
                    })
                    .transpose()?,
                name: file.name,
                region: None,
                deliver: None,
                caption: None,
                data: Bytes(file.data),
            })
        })
        .collect::<Result<_, ServeError>>()?;
    Ok((request, attachments))
}

#[cfg(test)]
#[path = "run_spawn_tests.rs"]
mod tests;
