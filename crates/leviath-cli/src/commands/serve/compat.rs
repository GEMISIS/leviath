//! The `/api/agents` routes older clients call, over the same core as
//! `/api/runs`.
//!
//! Most of them are the `/api/runs` handlers under the old path, since the
//! response was already the same. Two are not: `POST /api/agents` takes the
//! old task-and-flags body and answers `{agent_id, run_id}`, and
//! `GET /api/agents` lists every run as one array. Both are here.
//!
//! The old body becomes a [`SpawnRequest`](leviath_runtime::spec::request::SpawnRequest)
//! and goes through the same checks as one sent to `POST /api/runs`. Its
//! `task` is the `task` input. Each of its `regions` is the input the
//! blueprint declares for that region: the input of that name, or else the
//! one text input that seeds that region with its value alone. Any other
//! region (one no input fills, one only a template or a typed input fills,
//! or one several inputs fill) is left out, as older servers left a region
//! they did not seed, and the answer's `warnings` say so, naming the inputs
//! that do fill it. `POST /api/runs` takes inputs by name and refuses one the
//! blueprint does not declare.
//!
//! These routes are kept for older clients and will be removed.

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use leviath_runtime::spec::inputs::{InputDecl, InputSlot, InputType};
use leviath_runtime::spec::issues::{IssueCode, SpawnIssues};
use leviath_runtime::spec::names::BlueprintRef;
use leviath_runtime::spec::request::SpawnSource;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::core::error::as_api_error;
use super::core::inspect;
use super::core::spawn::{self as spawn_core, Verdict};
use super::runs::run_json;
use super::types::*;
use leviath_core::run_meta::StageRunStatus;

/// The body `POST /api/agents` takes.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(super) struct SpawnAgentReq {
    /// The installed blueprint's name.
    pub(super) blueprint: String,
    /// The task: the blueprint's `task` input.
    pub(super) task: String,
    /// A model for every stage that allows the operator's choice.
    pub(super) model: Option<String>,
    /// How deep the run's tree of child runs may grow.
    pub(super) max_depth: Option<usize>,
    /// Approve every tool call for this run.
    #[serde(default)]
    pub(super) yolo: bool,
    /// Run under a named profile from `yolo.toml`. Implies `yolo`.
    #[serde(default)]
    pub(super) yolo_profile: Option<String>,
    /// Tools to allow outright for this run.
    #[serde(default)]
    pub(super) allow: Vec<String>,
    /// Refuse the blueprint's shell-command seeds.
    #[serde(default)]
    pub(super) no_seed_commands: bool,
    /// Record every request sent to the model.
    #[serde(default)]
    pub(super) capture_model_input: bool,
    /// The directory the run's tools work in.
    pub(super) workdir: Option<String>,
    /// Text for the blueprint's inputs, keyed by the region each fills.
    #[serde(default)]
    pub(super) regions: HashMap<String, String>,
    /// The caller's labels for the run.
    #[serde(default)]
    pub(super) metadata: HashMap<String, String>,
    /// A webhook called when the run ends.
    pub(super) callback_url: Option<String>,
    /// The secret the webhook body is signed with.
    pub(super) callback_secret: Option<String>,
    /// The shape the final output is asked for in.
    pub(super) output_format: Option<String>,
    /// Extra guidance about that shape.
    pub(super) output_instructions: Option<String>,
    /// A JSON Schema the final output must satisfy.
    pub(super) output_schema: Option<serde_json::Value>,
    /// Files already inside the working directory, attached as parts.
    #[serde(default)]
    pub(super) parts: Vec<super::upload::PartRef>,
}

/// What `POST /api/agents` answers: the new run's id, twice, under the two
/// names older clients read, and what may keep the run from ever finishing
/// (left out when nothing may, so an older client sees the shape it knows).
#[derive(Debug, Serialize)]
pub(super) struct SpawnAgentResp {
    pub(super) agent_id: String,
    pub(super) run_id: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) warnings: Vec<String>,
}

/// `POST /api/agents`: start a run from the old body.
///
/// A blueprint that is not installed is a 404, a request this server does not
/// allow (a workdir outside `--workdir-root`, an unattended run under
/// `--no-remote-yolo`) a 403, and any other refusal a 400 naming every
/// problem.
pub(super) async fn spawn_agent(
    State(state): State<AppState>,
    request: axum::extract::Request,
) -> Result<Json<SpawnAgentResp>, ApiError> {
    let max_upload = state.limits.request_limits.max_upload_bytes;
    let (mut body, mut parts): (SpawnAgentReq, _) =
        super::upload::json_or_multipart(&state, request, max_upload).await?;
    let source = BlueprintRef::parse(&body.blueprint)
        .map(SpawnSource::Blueprint)
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("blueprint: {e}")))?;
    let declared = super::run_spawn::declared_inputs(&state, &body.blueprint)
        .await
        .map_err(|(status, message)| match status {
            StatusCode::NOT_FOUND => (status, message),
            _ => (StatusCode::BAD_REQUEST, message),
        })?;
    // Files named inside the workdir, and the ones a text names with `@path`,
    // read where the run will work.
    let workdir = body.workdir.clone().map_or_else(
        || std::env::current_dir().unwrap_or_default(),
        std::path::PathBuf::from,
    );
    parts.extend(super::upload::json_parts(
        &body.parts,
        &workdir,
        max_upload,
    )?);
    let (task, named) = super::upload::inline_parts(&body.task, None, &workdir, max_upload)?;
    body.task = task;
    parts.extend(named);
    let mut filling = Vec::new();
    let mut left_out = Vec::new();
    for (region, text) in std::mem::take(&mut body.regions) {
        match input_for_region(&declared, &region) {
            Ok(input) => filling.push((input, region, text)),
            Err(why) => left_out.push(format!(
                "regions.{region}: {why} in blueprint '{}', so its text was left out",
                body.blueprint
            )),
        }
    }
    // Two texts for one input: the one under the input's own name wins, then
    // the first region by name.
    filling.sort_by(|(a_in, a, _), (b_in, b, _)| (a != a_in, a).cmp(&(b != b_in, b)));
    let mut regions = HashMap::new();
    let mut from: HashMap<String, String> = HashMap::new();
    for (input, region, text) in filling {
        if let Some(winner) = from.get(&input) {
            left_out.push(format!(
                "regions.{region}: fills input '{input}', which regions.{winner} fills too, so its text was left out"
            ));
            continue;
        }
        let (kept, named) =
            super::upload::inline_parts(&text, Some(&region), &workdir, max_upload)?;
        parts.extend(named);
        regions.insert(input.clone(), kept);
        from.insert(input, region);
    }
    left_out.sort();
    let request = launch_of(body, regions, parts)
        .into_request_for(source)
        .map_err(|message| err(StatusCode::BAD_REQUEST, message))?;
    match spawn_core::start(&state, request).await {
        Ok(Verdict::Accepted(started)) => Ok(Json(SpawnAgentResp {
            agent_id: started.run_id.clone(),
            run_id: started.run_id,
            warnings: left_out
                .into_iter()
                .chain(started.warnings.iter().map(ToString::to_string))
                .collect(),
        })),
        Ok(Verdict::Rejected(issues)) => Err(refusal(&issues)),
        Err(e) => Err(as_api_error(&e)),
    }
}

/// The old body as a task launch, with its regions already named as inputs.
fn launch_of(
    body: SpawnAgentReq,
    regions: HashMap<String, String>,
    parts: Vec<leviath_core::mime::InboundPart>,
) -> crate::daemon::requests::TaskLaunch {
    let output = (body.output_format.is_some()
        || body.output_instructions.is_some()
        || body.output_schema.is_some())
    .then(|| leviath_core::output::OutputSpec {
        format: body.output_format,
        instructions: body.output_instructions,
        schema: body.output_schema,
        ..Default::default()
    });
    crate::daemon::requests::TaskLaunch {
        blueprint: body.blueprint,
        task: body.task,
        regions,
        parts,
        model: body.model,
        workdir: body.workdir,
        // A named profile is a kind of unattended run.
        unattended: body.yolo || body.yolo_profile.is_some(),
        profile: body.yolo_profile,
        allow: body.allow,
        max_depth: body.max_depth,
        no_seed_commands: body.no_seed_commands,
        output,
        capture_model_input: body.capture_model_input,
        metadata: body.metadata,
        callback_url: body.callback_url,
        callback_secret: body.callback_secret,
    }
}

/// The input that takes an old request's text for `region`: the input of
/// that name, or else the one text input that seeds the region with its
/// value alone. Otherwise the text has nowhere to go as it stands, and the
/// error says why: no input fills the region, or the inputs that do take
/// something other than its text.
fn input_for_region(declared: &[InputDecl], region: &str) -> Result<String, String> {
    if declared.iter().any(|input| input.name.as_str() == region) {
        return Ok(region.to_string());
    }
    let filling: Vec<(&InputDecl, bool)> = declared
        .iter()
        .flat_map(|input| {
            input.binds.iter().filter_map(move |slot| match slot {
                InputSlot::Region(binding) if binding.region.as_str() == region => Some((
                    input,
                    binding.template.is_none() && matches!(input.ty, InputType::Text { .. }),
                )),
                _ => None,
            })
        })
        .collect();
    match filling.as_slice() {
        [] => Err(format!("no input fills region '{region}'")),
        [(one, true)] => Ok(one.name.to_string()),
        _ => {
            let names: Vec<String> = filling
                .iter()
                .map(|(input, _)| format!("'{}'", input.name))
                .collect();
            Err(format!(
                "region '{region}' is filled from input {} rather than from text alone",
                names.join(", ")
            ))
        }
    }
}

/// A refused spawn the way the old route answered one: one message naming
/// every problem, under a 403 when any of them is something this server does
/// not allow and a 400 otherwise.
fn refusal(issues: &SpawnIssues) -> ApiError {
    let status = match issues.iter().any(|i| i.code == IssueCode::NotAllowed) {
        true => StatusCode::FORBIDDEN,
        false => StatusCode::BAD_REQUEST,
    };
    err(status, issues.to_string())
}

/// Query for `GET /api/agents`.
#[derive(Debug, Default, Deserialize)]
pub(super) struct ListAgentsQuery {
    /// Only runs in these statuses, comma-separated.
    pub(super) status: Option<String>,
}

/// `GET /api/agents`: every run on this machine as one array, newest first
/// the way the run index holds them, optionally only those in some statuses.
pub(super) async fn list_agents(
    State(state): State<AppState>,
    Query(query): Query<ListAgentsQuery>,
) -> Json<Vec<serde_json::Value>> {
    let mut runs = state.caches.run_index.snapshot().await.into_runs();
    if let Some(filter) = query.status {
        let wanted: Vec<&str> = filter.split(',').collect();
        runs.retain(|r| wanted.iter().any(|f| status_matches(&r.status, f)));
    }
    let now = leviath_core::duration::now_secs();
    Json(runs.iter().map(|m| run_json(m, now)).collect())
}

/// `GET /api/agents/{id}/stages`: the ledger `GET /api/runs/{id}/stages`
/// serves, in the words older clients know. Those know a stage only as
/// pending, active, waiting_input, complete, error or skipped, and fail on
/// any other word, so a paused stage is sent as active and a cancelled one as
/// error.
pub(super) async fn agent_stages(
    AxumPath(id): AxumPath<String>,
) -> Result<Json<RunStagesResp>, ApiError> {
    let read = id.clone();
    let mut stages = super::blocking::blocking(move || inspect::stages(&read))
        .await
        .map_err(|e| as_api_error(&e))?;
    for stage in &mut stages {
        stage.status = as_older_clients_know(&stage.status);
    }
    Ok(Json(RunStagesResp { run_id: id, stages }))
}

/// A stage's status in the words older clients know.
fn as_older_clients_know(status: &StageRunStatus) -> StageRunStatus {
    match status {
        StageRunStatus::Paused => StageRunStatus::Active,
        StageRunStatus::Cancelled => StageRunStatus::Error,
        other => other.clone(),
    }
}

#[cfg(test)]
#[path = "compat_tests.rs"]
mod tests;
