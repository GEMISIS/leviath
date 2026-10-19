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
//!
//! Both routes run under `[serve] spawn_timeout_secs` rather than the ordinary
//! request deadline, since starting a run connects its MCP servers and runs
//! its seeds before the daemon answers.

use axum::extract::multipart::Multipart;
use axum::extract::{FromRequest, Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use leviath_runtime::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
use leviath_runtime::spec::names::{IdempotencyKey, MimePattern};
use leviath_runtime::spec::request::{Attachment, Bytes, SpawnRequest};

use super::core::error::ServeError;
use super::core::spawn::{self as spawn_core, Verdict};
use super::request_limits::within;
use super::types::*;

/// `POST /api/runs`: start a run. 201 with its id, or 422 with every reason
/// it cannot start.
///
/// The request-limit layer leaves this route's deadline to it. The body has
/// the ordinary deadline to arrive in, and the daemon `spawn_timeout_secs` to
/// answer. A spawn past that answers 408 with `spawn_outcome: "unknown"` and
/// the request's idempotency key: the daemon may have started the run after
/// this server stopped waiting, and sending the request again under the same
/// key is how a caller finds out without starting it twice.
pub(super) async fn spawn_run(
    State(state): State<AppState>,
    request: axum::extract::Request,
) -> Result<Response, ServeError> {
    let limits = state.limits.request_limits;
    let request = match read_in_time(&state, request).await? {
        Ok(request) => request,
        Err(answer) => return Ok(answer),
    };
    let key = request
        .delivery
        .idempotency_key
        .as_ref()
        .map(ToString::to_string);
    match within(limits.spawn_timeout(), spawn_core::start(&state, request)).await {
        Some(Ok(Verdict::Accepted(started))) => Ok((
            StatusCode::CREATED,
            Json(SpawnedResp {
                run_id: started.run_id,
                warnings: IssuesResp::from(started.warnings).issues,
                seq: started.seq,
                server_instance: super::events::server_instance().to_string(),
                deduplicated: started.deduplicated,
            }),
        )
            .into_response()),
        Some(Ok(Verdict::Rejected(issues))) => Ok(refusal(issues)),
        Some(Err(e)) => Err(e),
        None => Ok(spawn_timed_out(limits.spawn_timeout_secs, key)),
    }
}

/// The request `request`'s body carries, read under the ordinary deadline:
/// `Ok(Err)` with the answer for a body that did not arrive in time or is
/// not a request, and `Err` for one that could not be read at all.
async fn read_in_time(
    state: &AppState,
    request: axum::extract::Request,
) -> Result<Result<SpawnRequest, Response>, ServeError> {
    let limits = state.limits.request_limits;
    match within(limits.timeout(), read_request(state, request)).await {
        None => Ok(Err(late(limits.request_timeout_secs))),
        Some(read) => Ok(read?.map_err(refusal)),
    }
}

/// The ordinary 408, for a request that ran past `secs`.
fn late(secs: u64) -> Response {
    err(
        StatusCode::REQUEST_TIMEOUT,
        format!("request took longer than {secs} s"),
    )
    .into_response()
}

/// The 408 for a spawn that ran past its deadline of `secs`.
fn spawn_timed_out(secs: u64, idempotency_key: Option<String>) -> Response {
    let error = format!(
        "the spawn took longer than {secs} s, so whether its run started is not known; send it \
         again with the same delivery.idempotency_key to find out without starting it twice"
    );
    (
        StatusCode::REQUEST_TIMEOUT,
        Json(SpawnTimeoutResp {
            error,
            spawn_outcome: "unknown",
            idempotency_key,
        }),
    )
        .into_response()
}

/// `POST /api/runs/validate`: resolve a run without starting it. 200 with a
/// summary of the run it would be, or 422 with every reason it would be
/// refused. The body has the ordinary deadline to arrive in, and the daemon
/// `spawn_timeout_secs` to answer, as for the spawn it checks: checking
/// connects the graph's MCP servers just as starting it does.
pub(super) async fn validate_run(
    State(state): State<AppState>,
    request: axum::extract::Request,
) -> Result<Response, ServeError> {
    let limits = state.limits.request_limits;
    let request = match read_in_time(&state, request).await? {
        Ok(request) => request,
        Err(answer) => return Ok(answer),
    };
    match within(
        limits.spawn_timeout(),
        spawn_core::validate(&state, request),
    )
    .await
    {
        Some(Ok(Verdict::Accepted(summary))) => Ok((StatusCode::OK, Json(summary)).into_response()),
        Some(Ok(Verdict::Rejected(issues))) => Ok(refusal(issues)),
        Some(Err(e)) => Err(e),
        None => Ok(late(limits.spawn_timeout_secs)),
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
/// An idempotency key that is not one is named at its own path, where the
/// decoder would only say it failed somewhere in the body.
fn decode(value: serde_json::Value) -> Result<SpawnRequest, SpawnIssues> {
    if let Some(refused) = value
        .pointer("/delivery/idempotency_key")
        .and_then(key_refusal)
    {
        return Err(refused.into());
    }
    serde_json::from_value(value).map_err(|e| {
        SpawnIssues::from(
            SpawnIssue::new(SpecPath::root(), IssueCode::Invalid, e.to_string())
                .hint("GET /api/schema/spawn-request for the request's shape"),
        )
    })
}

/// Why `key`, as a request body wrote it, is not an idempotency key, when it
/// is not one. A `null` is no key at all.
fn key_refusal(key: &serde_json::Value) -> Option<SpawnIssue> {
    let path = SpecPath::root().field("delivery").field("idempotency_key");
    match key {
        serde_json::Value::Null => None,
        serde_json::Value::String(text) => IdempotencyKey::new(text.as_str())
            .err()
            .map(|e| SpawnIssue::new(path, IssueCode::Invalid, e.to_string()).got(text)),
        other => Some(
            SpawnIssue::new(path, IssueCode::WrongType, "an idempotency key is text")
                .got(other.to_string()),
        ),
    }
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
