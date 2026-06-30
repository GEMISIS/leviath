//! Agent CRUD endpoints: spawn, list, get, kill, children, context, logs, result.

use std::path::PathBuf;

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use tracing::info;

use super::blueprints::discover_blueprints;
use super::types::*;
use crate::commands::run::parse_manifest_public;
use crate::runstate::{self, ContextSnapshot, RunMeta, RunStatus};

pub(super) async fn spawn_agent(
    State(state): State<AppState>,
    Json(body): Json<SpawnAgentReq>,
) -> Result<Json<SpawnAgentResp>, (StatusCode, Json<ErrorResponse>)> {
    // Find the blueprint manifest
    let blueprints = discover_blueprints(&state.config);
    let bp_info = blueprints
        .iter()
        .find(|b| b.name == body.blueprint)
        .ok_or((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Blueprint '{}' not found", body.blueprint),
            }),
        ))?;

    let manifest_path = PathBuf::from(&bp_info.path).join("agent.leviath");
    let manifest_content = std::fs::read_to_string(&manifest_path).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Failed to read manifest: {}", e),
            }),
        )
    })?;
    let blueprint = parse_manifest_public(&manifest_content).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: format!("Invalid manifest: {}", e),
            }),
        )
    })?;

    let workdir = body.workdir.clone().unwrap_or_else(|| {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| ".".to_string())
    });

    let run_id = runstate::new_run_id(&blueprint.name);
    let mut meta = RunMeta::new(
        run_id.clone(),
        blueprint.name.clone(),
        bp_info.path.clone(),
        body.task.clone(),
        body.model.clone(),
        workdir.clone(),
        blueprint.stages.len(),
    );
    meta.metadata = body.metadata.clone();
    meta.callback_url = body.callback_url.clone();

    runstate::create_run(&meta).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Failed to create run: {}", e),
            }),
        )
    })?;

    // Spawn background worker process (same as `lev run`)
    let log_path = runstate::run_dir(&run_id).join("output.log");
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("Failed to open log file: {}", e),
                }),
            )
        })?;
    let log_file2 = log_file.try_clone().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Failed to clone log file: {}", e),
            }),
        )
    })?;

    let exe = std::env::current_exe().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Failed to locate executable: {}", e),
            }),
        )
    })?;

    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("__run-worker")
        .arg(manifest_path.to_string_lossy().as_ref())
        .arg("--task")
        .arg(&body.task)
        .arg("--run-id")
        .arg(&run_id);

    if let Some(ref model) = body.model {
        cmd.arg("--model").arg(model);
    }
    if body.yolo {
        cmd.arg("--yolo");
    }
    for t in &body.allow {
        cmd.arg("--allow").arg(t);
    }
    if let Some(md) = body.max_depth {
        cmd.arg("--max-depth").arg(md.to_string());
    }

    cmd.current_dir(&workdir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log_file))
        .stderr(std::process::Stdio::from(log_file2));

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }

    cmd.spawn().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Failed to spawn worker: {}", e),
            }),
        )
    })?;

    // Broadcast the spawned event
    let _ = state.event_tx.send(ServerEvent::AgentSpawned {
        agent_id: blueprint.name.clone(),
        run_id: run_id.clone(),
        parent_id: None,
        blueprint: blueprint.name.clone(),
    });

    info!(run_id = %run_id, blueprint = %blueprint.name, "Spawned agent via API");

    Ok(Json(SpawnAgentResp {
        agent_id: blueprint.name,
        run_id,
    }))
}

pub(super) async fn list_agents(Query(query): Query<ListAgentsQuery>) -> Json<Vec<RunMeta>> {
    let mut runs = runstate::list_runs();

    if let Some(ref status_filter) = query.status {
        let filters: Vec<&str> = status_filter.split(',').collect();
        runs.retain(|r| {
            let s = format!("{}", r.status).to_lowercase();
            filters.iter().any(|f| f.to_lowercase() == s)
        });
    }

    Json(runs)
}

pub(super) async fn get_agent(
    AxumPath(id): AxumPath<String>,
) -> Result<Json<RunMeta>, (StatusCode, Json<ErrorResponse>)> {
    runstate::read_meta(&id).map(Json).map_err(|_| {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Agent run '{}' not found", id),
            }),
        )
    })
}

pub(super) async fn agent_children(AxumPath(id): AxumPath<String>) -> Json<Vec<RunMeta>> {
    let runs = runstate::list_runs();
    let children: Vec<RunMeta> = runs
        .into_iter()
        .filter(|r| r.parent_run_id.as_deref() == Some(&id))
        .collect();
    Json(children)
}

pub(super) async fn agent_context(
    AxumPath(id): AxumPath<String>,
) -> Result<Json<ContextSnapshot>, (StatusCode, Json<ErrorResponse>)> {
    runstate::read_context_snapshot(&id)
        .map(Json)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: format!("No context snapshot for run '{}'", id),
                }),
            )
        })
}

pub(super) async fn agent_logs(
    AxumPath(id): AxumPath<String>,
    Query(query): Query<LogsQuery>,
) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    let run_dir = runstate::run_dir(&id);
    if !run_dir.exists() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Agent run '{}' not found", id),
            }),
        ));
    }

    let max_bytes = query.tail.unwrap_or(32_768);
    let log = runstate::tail_file(&run_dir.join("output.log"), max_bytes);
    Ok(log)
}

pub(super) async fn agent_result(
    AxumPath(id): AxumPath<String>,
) -> Result<Json<AgentResultResp>, (StatusCode, Json<ErrorResponse>)> {
    let meta = runstate::read_meta(&id).map_err(|_| {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Agent run '{}' not found", id),
            }),
        )
    })?;

    // Read the last stage's output
    let stages = runstate::read_stages_index(&id);
    let output = if !stages.is_empty() {
        let last_idx = stages.len() - 1;
        runstate::tail_stage_output(&id, last_idx, 65_536)
    } else {
        runstate::tail_file(&runstate::run_dir(&id).join("output.log"), 65_536)
    };

    Ok(Json(AgentResultResp {
        run_id: meta.run_id,
        status: format!("{}", meta.status),
        output,
        error: meta.error,
        prompt_tokens: meta.prompt_tokens,
        completion_tokens: meta.completion_tokens,
    }))
}

pub(super) async fn kill_agent(
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    let meta = runstate::read_meta(&id).map_err(|_| {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Agent run '{}' not found", id),
            }),
        )
    })?;

    // Kill the process
    #[cfg(unix)]
    {
        unsafe {
            libc::kill(meta.pid as i32, libc::SIGTERM);
        }
    }

    // Update status
    let mut meta = meta;
    meta.status = RunStatus::Cancelled;
    meta.touch();
    let _ = runstate::write_meta(&meta);

    // Cascade kill to children
    let runs = runstate::list_runs();
    for child in runs {
        if child.parent_run_id.as_deref() == Some(&id) {
            #[cfg(unix)]
            {
                unsafe {
                    libc::kill(child.pid as i32, libc::SIGTERM);
                }
            }
            let mut child = child;
            child.status = RunStatus::Cancelled;
            child.touch();
            let _ = runstate::write_meta(&child);
        }
    }

    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_agent_req_deserialization_minimal() {
        let json = r#"{
            "blueprint": "coder",
            "task": "write a hello world"
        }"#;
        let req: SpawnAgentReq = serde_json::from_str(json).unwrap();
        assert_eq!(req.blueprint, "coder");
        assert_eq!(req.task, "write a hello world");
        assert!(req.model.is_none());
        assert!(req.workdir.is_none());
        assert!(!req.yolo);
        assert!(req.allow.is_empty());
        assert!(req.max_depth.is_none());
        assert!(req.metadata.is_empty());
        assert!(req.callback_url.is_none());
    }

    #[test]
    fn spawn_agent_req_deserialization_full() {
        let json = r#"{
            "blueprint": "coder",
            "task": "build app",
            "model": "claude-sonnet-4-6",
            "max_depth": 3,
            "yolo": true,
            "allow": ["read_file", "bash"],
            "workdir": "/tmp/work",
            "metadata": {"project": "test"},
            "callback_url": "https://example.com/hook"
        }"#;
        let req: SpawnAgentReq = serde_json::from_str(json).unwrap();
        assert_eq!(req.blueprint, "coder");
        assert_eq!(req.model.unwrap(), "claude-sonnet-4-6");
        assert_eq!(req.max_depth, Some(3));
        assert!(req.yolo);
        assert_eq!(req.allow.len(), 2);
        assert_eq!(req.workdir.unwrap(), "/tmp/work");
        assert_eq!(req.metadata.get("project").unwrap(), "test");
        assert_eq!(req.callback_url.unwrap(), "https://example.com/hook");
    }

    #[test]
    fn spawn_agent_resp_serialization() {
        let resp = SpawnAgentResp {
            agent_id: "coder".to_string(),
            run_id: "run-abc-123".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"agent_id\":\"coder\""));
        assert!(json.contains("\"run_id\":\"run-abc-123\""));
    }

    #[test]
    fn list_agents_query_deserialization_empty() {
        let json = "{}";
        let query: ListAgentsQuery = serde_json::from_str(json).unwrap();
        assert!(query.status.is_none());
    }

    #[test]
    fn list_agents_query_deserialization_with_status() {
        let json = r#"{"status": "running,complete"}"#;
        let query: ListAgentsQuery = serde_json::from_str(json).unwrap();
        assert_eq!(query.status.unwrap(), "running,complete");
    }

    #[test]
    fn agent_result_resp_serialization() {
        let resp = AgentResultResp {
            run_id: "run-123".to_string(),
            status: "complete".to_string(),
            output: "done!".to_string(),
            error: None,
            prompt_tokens: 5000,
            completion_tokens: 1200,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"run_id\":\"run-123\""));
        assert!(json.contains("\"status\":\"complete\""));
        assert!(json.contains("\"prompt_tokens\":5000"));
        assert!(json.contains("\"completion_tokens\":1200"));
    }

    #[test]
    fn agent_result_resp_with_error() {
        let resp = AgentResultResp {
            run_id: "run-err".to_string(),
            status: "error".to_string(),
            output: String::new(),
            error: Some("something went wrong".to_string()),
            prompt_tokens: 100,
            completion_tokens: 0,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("something went wrong"));
    }

    #[test]
    fn logs_query_deserialization() {
        let json = r#"{"tail": 8192}"#;
        let query: LogsQuery = serde_json::from_str(json).unwrap();
        assert_eq!(query.tail, Some(8192));
    }

    #[test]
    fn logs_query_deserialization_empty() {
        let json = "{}";
        let query: LogsQuery = serde_json::from_str(json).unwrap();
        assert!(query.tail.is_none());
    }

    #[test]
    fn error_response_serialization() {
        let err = ErrorResponse {
            error: "not found".to_string(),
        };
        let json = serde_json::to_string(&err).unwrap();
        assert!(json.contains("\"error\":\"not found\""));
    }
}
