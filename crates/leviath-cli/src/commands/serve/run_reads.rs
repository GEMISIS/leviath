//! One run, read: `GET /api/runs/{id}` and everything under it.
//!
//! Every read here answers from the run's file, through the service layer
//! both surfaces share, so it works with the daemon down and for a run that
//! finished last week. The one exception is a run's state as it is now, which
//! the daemon answers for a run it holds.
//!
//! The writes that steer a run (pause, resume, cancel) sit here too, beside
//! the reads of the run they act on.

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use serde::Deserialize;

use super::core::error::as_api_error;
use super::core::{history, inspect, lifecycle};
use super::runs::run_json;
use super::types::*;
use crate::runstate::{self, ContextSnapshot};

/// `GET /api/runs/{id}`: the run's record.
pub(super) async fn get_run(
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    runstate::read_meta(&id)
        .map(|m| Json(run_json(&m, leviath_core::duration::now_secs())))
        .map_err(|_| err(StatusCode::NOT_FOUND, format!("Run '{id}' not found")))
}

/// `GET /api/runs/{id}/spec`: the spec the run was resolved to.
pub(super) async fn run_spec(
    AxumPath(id): AxumPath<String>,
) -> Result<Json<leviath_runtime::spec::run_spec::RunSpec>, ApiError> {
    super::blocking::blocking(move || inspect::spec(&id))
        .await
        .map(Json)
        .map_err(|e| as_api_error(&e))
}

/// Query for `GET /api/runs/{id}/state`.
#[derive(Debug, Default, Deserialize)]
pub(super) struct StateQuery {
    /// The step to read the state at. Absent reads the run as it is now.
    pub(super) at: Option<u64>,
}

/// `GET /api/runs/{id}/state?at=<seq>`: the run's state at a step, or now.
pub(super) async fn run_state(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<StateQuery>,
) -> Result<Json<leviath_runtime::state::RunState>, ApiError> {
    inspect::state(&state, &id, query.at)
        .await
        .map(Json)
        .map_err(|e| as_api_error(&e))
}

/// Query for `GET /api/runs/{id}/deltas`.
#[derive(Debug, Default, Deserialize)]
pub(super) struct DeltasQuery {
    /// The first step to return. Absent starts at the first.
    pub(super) from: Option<u64>,
    /// The last step to return. Absent ends at the last.
    pub(super) to: Option<u64>,
}

/// `GET /api/runs/{id}/deltas?from=&to=`: the run's steps, in order.
pub(super) async fn run_deltas(
    AxumPath(id): AxumPath<String>,
    Query(query): Query<DeltasQuery>,
) -> Result<Json<Vec<leviath_runtime::state::StateDelta>>, ApiError> {
    super::blocking::blocking(move || inspect::deltas(&id, query.from, query.to))
        .await
        .map(Json)
        .map_err(|e| as_api_error(&e))
}

/// `GET /api/runs/{id}/graph`: the run's stages and edges, with how often it
/// entered each stage and took each edge.
pub(super) async fn run_graph(
    AxumPath(id): AxumPath<String>,
) -> Result<Json<inspect::RunGraphView>, ApiError> {
    super::blocking::blocking(move || inspect::graph(&id))
        .await
        .map(Json)
        .map_err(|e| as_api_error(&e))
}

/// `GET /api/runs/{id}/children`: the runs this one started, one level down.
pub(super) async fn run_children(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Json<Vec<serde_json::Value>> {
    let now = leviath_core::duration::now_secs();
    let snapshot = state.caches.run_index.snapshot().await;
    Json(
        snapshot
            .under(Some(&id))
            .map(|r| run_json(r, now))
            .collect(),
    )
}

/// `GET /api/runs/{id}/context`: the run's context window as of its last step.
pub(super) async fn run_context(
    AxumPath(id): AxumPath<String>,
) -> Result<Json<ContextSnapshot>, ApiError> {
    super::blocking::blocking(move || inspect::context(&id))
        .await
        .map(Json)
        .map_err(|e| as_api_error(&e))
}

/// `GET /api/runs/{id}/context/history`: the run's context window over time,
/// one page at a time.
///
/// The cursor is the point's index: a run file only grows, so an index is
/// stable once written and new points only ever arrive at the end. `order=asc`
/// is the default, which is chronological.
pub(super) async fn run_context_history(
    AxumPath(id): AxumPath<String>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<Page<leviath_core::run_archive::RunPoint>>, ApiError> {
    let spec = history::HistorySpec::resolve(
        &id,
        query.limit,
        query.order.as_deref(),
        query.cursor.as_deref(),
    )
    .map_err(|e| as_api_error(&e))?;
    let page = super::blocking::blocking(move || history::page(&id, &spec))
        .await
        .map_err(|e| as_api_error(&e))?;
    Ok(Json(Page::new(
        page.points,
        page.next_cursor,
        Some(page.total),
        leviath_core::duration::now_secs(),
    )))
}

/// `GET /api/runs/{id}/logs`: what a run has written, by stage.
///
/// `?stage=` and `?stream=` select which log; both default to what a caller
/// tailing a live run wants (the current stage's readable output).
pub(super) async fn run_logs(
    AxumPath(id): AxumPath<String>,
    Query(query): Query<LogsQuery>,
) -> Result<String, ApiError> {
    if !runstate::run_dir(&id).exists() {
        return Err(err(StatusCode::NOT_FOUND, format!("Run '{id}' not found")));
    }
    let selector = query
        .selector()
        .map_err(|message| err(StatusCode::BAD_REQUEST, message))?;
    let stream = query
        .log_stream()
        .map_err(|message| err(StatusCode::BAD_REQUEST, message))?;
    // Clamped like every other limit in this API: `tail` is client-controlled
    // and `stage=all` multiplies it by the stage count, so an unclamped value
    // was an arbitrary-size allocation on request.
    let max_bytes = query.tail.unwrap_or(32_768).min(LOGS_MAX_TAIL_BYTES);
    Ok(runstate::tail_run_logs(&id, selector, stream, max_bytes))
}

/// Largest per-stage byte window `GET /api/runs/{id}/logs?tail=` honors:
/// 1 MiB, matching the file endpoint's cap.
pub(super) const LOGS_MAX_TAIL_BYTES: u64 = 1024 * 1024;

/// How much of a file `GET /api/runs/{id}/files` returns.
pub(super) use super::core::files::MAX_FILE_READ_BYTES;

/// `GET /api/runs/{id}/files?path=<path>`: read a file the run wrote, so a
/// browser can render an agent's report without shell access to the host.
///
/// `path` may be relative (resolved against the run's workdir) or absolute;
/// either way the resolved path must still land inside the workdir, under the
/// same symlink-aware containment the file tools use. Reads are capped at
/// [`MAX_FILE_READ_BYTES`]; a larger file comes back truncated and says so.
pub(super) async fn run_file(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<FileQuery>,
) -> Result<Json<FileOrListing>, ApiError> {
    use super::core::files;

    let meta = runstate::read_meta(&id)
        .map_err(|_| err(StatusCode::NOT_FOUND, format!("Run '{id}' not found")))?;
    let source = files::FileSource::parse(query.source.as_deref()).map_err(|e| as_api_error(&e))?;
    // A listing types each row by name with the same registry `/files/raw`
    // types the bytes with, so a console gets the run's answer, not its own.
    let registry = state.current_config().mime_registry_or_defaults();

    let Some(ref requested_path) = query.path else {
        let listed = files::listing(&meta, source, None, query.hidden, &registry)
            .map_err(|e| as_api_error(&e))?;
        return Ok(Json(FileOrListing::Listing(Box::new(listing_resp(listed)))));
    };

    match files::read(
        &meta,
        requested_path,
        query.offset.unwrap_or(0),
        query.hidden,
        &registry,
    )
    .map_err(|e| as_api_error(&e))?
    {
        files::FileRead::Listing(listed) => Ok(Json(FileOrListing::Listing(Box::new(
            listing_resp(*listed),
        )))),
        files::FileRead::Window(window) => Ok(Json(FileOrListing::File(FileContentResp {
            path: window.path,
            size: window.size,
            offset: window.offset,
            next_offset: window.next_offset,
            content: window.content,
            truncated: window.truncated,
        }))),
    }
}

/// A core listing in this route's own shape: `kind` is a literal a client
/// discriminates on, and both are words rather than enums.
fn listing_resp(listed: super::core::files::FileListing) -> RunFileListing {
    RunFileListing {
        kind: "listing",
        source: listed.source.wire(),
        path: listed.path,
        parent: listed.parent,
        workdir: listed.workdir,
        entries: listed
            .entries
            .into_iter()
            .map(|entry| RunFileEntry {
                name: entry.name,
                path: entry.path,
                is_dir: entry.is_dir,
                size: entry.size,
                exists: entry.exists,
                outside_workdir: entry.outside_workdir,
                mime_type: entry.mime_type,
            })
            .collect(),
        truncated: listed.truncated,
        modified_files_truncated: listed.modified_files_truncated,
        modifying_tool_calls: listed.modifying_tool_calls,
    }
}

/// `GET /api/runs/{id}/result`: the run's answer, with the tail of its last
/// stage's output beside it.
pub(super) async fn run_result(
    AxumPath(id): AxumPath<String>,
) -> Result<Json<RunResultResp>, ApiError> {
    let meta = runstate::read_meta(&id)
        .map_err(|_| err(StatusCode::NOT_FOUND, format!("Run '{id}' not found")))?;
    let output = runstate::tail_run_logs(
        &id,
        runstate::StageSelector::Current,
        runstate::LogStream::Output,
        65_536,
    );
    Ok(Json(RunResultResp {
        run_id: meta.run_id,
        status: meta.status.wire().to_string(),
        output,
        final_output: runstate::read_final_output(&id).map(Into::into),
        error: meta.error,
        prompt_tokens: meta.prompt_tokens,
        completion_tokens: meta.completion_tokens,
    }))
}

/// `GET /api/runs/{id}/stages`: the run's per-stage ledger as of its last
/// step.
///
/// A run that has not reached its first stage boundary has no records yet,
/// which is an empty list rather than a miss: the run exists.
pub(super) async fn run_stages(
    AxumPath(id): AxumPath<String>,
) -> Result<Json<RunStagesResp>, ApiError> {
    let read = id.clone();
    let stages = super::blocking::blocking(move || inspect::stages(&read))
        .await
        .map_err(|e| as_api_error(&e))?;
    Ok(Json(RunStagesResp { run_id: id, stages }))
}

/// `POST /api/runs/{id}/cancel`: cancel a run, and its sub-agents with it.
///
/// A run that has already finished answers 409: there is nothing left to stop.
pub(super) async fn cancel_run(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    lifecycle::act(&state, &id, lifecycle::Action::Cancel)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(|e| as_api_error(&e))
}

/// `POST /api/runs/{id}/pause`: park a run.
///
/// A finished run answers 409, from its own record. Every other refusal is
/// the daemon's, which does not say which of "no such run" and "not pausable
/// right now" it means, so the 404 names both.
pub(super) async fn pause_run(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    lifecycle::act(&state, &id, lifecycle::Action::Pause)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(|e| as_api_error(&e))
}

/// `POST /api/runs/{id}/resume`: un-pause a run.
pub(super) async fn resume_run(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    lifecycle::act(&state, &id, lifecycle::Action::Resume)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(|e| as_api_error(&e))
}

#[cfg(test)]
#[path = "run_reads_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "run_reads_disk_tests.rs"]
mod disk_tests;
