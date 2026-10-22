//! Continuing a finished run with a new message, and reading its segments.
//!
//! `POST /api/runs/{id}/continue` takes the message `POST /api/runs/{id}/message`
//! takes, with `fork` beside it: false continues the run in place, true
//! starts a new run carrying its context and leaves it as it is.
//! `GET /api/runs/{id}/segments` lists what each segment did, the answer of
//! every one included.

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::Json;
use serde::{Deserialize, Serialize};

use super::core::error::ServeError;
use super::core::segments::SegmentRead;
use super::core::spawn::{self as spawn_core, ContinueAsk, Verdict};
use super::types::AppState;

/// `POST /api/runs/{id}/continue`'s body.
#[derive(Debug, Deserialize)]
pub(super) struct ContinueReq {
    /// The message the new segment starts with.
    pub(super) message: String,
    /// The region it lands in; the conversation when absent.
    #[serde(default)]
    pub(super) target_region: Option<String>,
    /// Files already inside the run's working directory to send with it.
    #[serde(default)]
    pub(super) parts: Vec<super::upload::PartRef>,
    /// Start a new run from the finished one, leaving it as it is.
    #[serde(default)]
    pub(super) fork: bool,
}

/// What a continuation answers with.
#[derive(Debug, Serialize)]
pub(super) struct ContinueResp {
    /// The run going again: the new run's id for a fork.
    pub(super) run_id: String,
    /// The segment that started, counted from 1.
    pub(super) segment: u32,
    /// The run it was forked from, for a fork.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) forked_from: Option<String>,
    /// The event stream's number before the run was continued: every frame
    /// of the new segment is numbered above it.
    pub(super) seq: u64,
    /// The server process that numbered `seq`.
    pub(super) server_instance: String,
}

/// `POST /api/runs/{id}/continue`: continue a finished run with a new
/// message, or fork a new run from it. 202 for a run continued in place, 201
/// for a new run; answered once the step that reopened the run is on disk.
pub(super) async fn continue_run(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    request: axum::extract::Request,
) -> Result<(StatusCode, Json<ContinueResp>), ServeError> {
    let max_upload = state.limits.request_limits.max_upload_bytes;
    let (body, mut parts): (ContinueReq, _) =
        super::upload::json_or_multipart(&state, request, max_upload).await?;
    let (text, named) =
        super::interactions::workdir_parts(&id, &body.message, &body.parts, max_upload).await?;
    parts.extend(named);
    let ask = ContinueAsk {
        text,
        region: body.target_region,
        parts,
        fork: body.fork,
    };
    let continued = match spawn_core::continue_run(&state, &id, ask).await? {
        Verdict::Accepted(continued) => continued,
        Verdict::Rejected(issues) => return Err(spawn_core::continue_refusal(&issues)),
    };
    let status = match continued.forked_from {
        Some(_) => StatusCode::CREATED,
        None => StatusCode::ACCEPTED,
    };
    Ok((
        status,
        Json(ContinueResp {
            run_id: continued.run_id,
            segment: continued.segment,
            forked_from: continued.forked_from,
            seq: continued.seq,
            server_instance: super::events::server_instance().to_string(),
        }),
    ))
}

/// One segment, as `GET /api/runs/{id}/segments` lists it.
#[derive(Debug, Serialize)]
pub(super) struct SegmentResp {
    /// Its number, counted from 1.
    pub(super) index: u32,
    /// Whether the run is in it now.
    pub(super) current: bool,
    /// The stage it started in.
    pub(super) stage: String,
    /// When it started, unix seconds.
    pub(super) started_at: i64,
    /// The step it started at.
    pub(super) started_seq: u64,
    /// The last step it recorded.
    pub(super) last_seq: u64,
    /// How it stands, or how it ended.
    pub(super) status: String,
    /// The run it was forked from, for a fork's first segment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) forked_from: Option<String>,
    /// What it cost itself, in dollars. Null is unknown, never free.
    pub(super) cost_usd: Option<f64>,
    /// Its tokens.
    pub(super) prompt_tokens: u64,
    /// Its output tokens.
    pub(super) completion_tokens: u64,
    /// Its tool calls.
    pub(super) tool_calls: u64,
    /// Its answer, when it handed one back.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) output: Option<String>,
    /// The files it handed back with its answer, by name.
    pub(super) artifacts: Vec<String>,
}

impl From<SegmentRead> for SegmentResp {
    fn from(read: SegmentRead) -> Self {
        let SegmentRead { segment, answer } = read;
        let status = match &segment.status {
            leviath_runtime::state::RunStatus::Idle => "starting",
            leviath_runtime::state::RunStatus::Active => "running",
            leviath_runtime::state::RunStatus::Waiting => "waiting_input",
            leviath_runtime::state::RunStatus::Paused => "paused",
            leviath_runtime::state::RunStatus::Complete => "complete",
            leviath_runtime::state::RunStatus::Error(_) => "error",
            leviath_runtime::state::RunStatus::Cancelled => "cancelled",
        };
        let (cost_usd, _) = leviath_runtime::insert::place::cost(&segment.spend);
        Self {
            index: segment.index,
            current: segment.current,
            stage: segment.stage.to_string(),
            started_at: segment.started_at,
            started_seq: segment.started_seq,
            last_seq: segment.last_seq,
            status: status.to_string(),
            forked_from: segment.forked_from.as_ref().map(ToString::to_string),
            cost_usd,
            prompt_tokens: segment.spend.prompt_tokens,
            completion_tokens: segment.spend.completion_tokens,
            tool_calls: segment.tool_calls,
            output: answer,
            artifacts: segment
                .final_output
                .iter()
                .flat_map(|out| &out.artifacts)
                .map(|a| a.name.clone())
                .collect(),
        }
    }
}

/// `GET /api/runs/{id}/segments`: every segment of a run, oldest first, each
/// with its own spend and answer. A run never continued has one.
pub(super) async fn run_segments(
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Vec<SegmentResp>>, ServeError> {
    // The directory and the run file are read on the blocking pool.
    let read = super::blocking::blocking(move || {
        if !crate::runstate::run_dir(&id).is_dir() {
            return Err(ServeError::run_not_found(&id));
        }
        super::core::segments::segments(&id)
    })
    .await?;
    Ok(Json(read.into_iter().map(SegmentResp::from).collect()))
}

#[cfg(test)]
#[path = "continuing_tests.rs"]
mod tests;
