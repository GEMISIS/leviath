//! `lev serve` — REST + WebSocket API server.
//!
//! Exposes agent management, blueprint CRUD, and live event streaming over
//! HTTP. No web UI — the frontend lives in a separate repo.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path as AxumPath, Query, State, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use axum::routing::{get, post};
use axum::Router;
use clap::Args;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tower_http::cors::{Any, CorsLayer};
use tracing::{error, info, warn};

use crate::config::Config;
use crate::interaction;
use crate::runstate::{self, ContextSnapshot, RunMeta, RunStatus};

use super::run::{build_provider_registry, parse_manifest_public};

// ─── CLI ─────────────────────────────────────────────────────────────────────

#[derive(Args)]
pub struct ServeArgs {
    /// Port to listen on
    #[arg(short, long, default_value = "3000")]
    pub port: u16,

    /// Host to bind to
    #[arg(short = 'H', long, default_value = "127.0.0.1")]
    pub host: String,

    /// Allow CORS from origin (default: *)
    #[arg(long, default_value = "*")]
    pub cors: String,
}

// ─── Shared state ────────────────────────────────────────────────────────────

/// Events broadcast to WebSocket subscribers.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(dead_code)]
pub enum ServerEvent {
    AgentStatus {
        agent_id: String,
        run_id: String,
        status: String,
        stage: String,
        iteration: usize,
    },
    ContextUpdate {
        agent_id: String,
        run_id: String,
        total_tokens: usize,
        max_tokens: usize,
    },
    Log {
        agent_id: String,
        run_id: String,
        line: String,
    },
    InteractionNeeded {
        agent_id: String,
        run_id: String,
        request: serde_json::Value,
    },
    AgentSpawned {
        agent_id: String,
        run_id: String,
        parent_id: Option<String>,
        blueprint: String,
    },
    AgentCompleted {
        agent_id: String,
        run_id: String,
        status: String,
        result: Option<String>,
    },
    Tokens {
        agent_id: String,
        run_id: String,
        prompt_tokens: usize,
        completion_tokens: usize,
    },
}

#[derive(Clone)]
pub struct AppState {
    config: Arc<Config>,
    event_tx: broadcast::Sender<ServerEvent>,
}

// ─── Entrypoint ──────────────────────────────────────────────────────────────

pub async fn execute(args: ServeArgs) -> anyhow::Result<()> {
    let config = Config::load()?;
    for warning in config.validate_keys() {
        warn!("{}", warning);
    }

    let (event_tx, _) = broadcast::channel::<ServerEvent>(1024);

    let state = AppState {
        config: Arc::new(config),
        event_tx: event_tx.clone(),
    };

    // Background polling loop
    let poll_state = state.clone();
    tokio::spawn(async move {
        polling_loop(poll_state).await;
    });

    let cors = if args.cors == "*" {
        CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any)
    } else {
        CorsLayer::new()
            .allow_origin(
                args.cors
                    .parse::<axum::http::HeaderValue>()
                    .unwrap_or_else(|_| axum::http::HeaderValue::from_static("*")),
            )
            .allow_methods(Any)
            .allow_headers(Any)
    };

    let app = Router::new()
        // Blueprints
        .route(
            "/api/blueprints",
            get(list_blueprints).post(create_blueprint),
        )
        .route("/api/blueprints/validate", post(validate_blueprint))
        .route(
            "/api/blueprints/{name}",
            get(get_blueprint)
                .put(update_blueprint)
                .delete(delete_blueprint),
        )
        // Agents
        .route("/api/agents", get(list_agents).post(spawn_agent))
        .route("/api/agents/tree", get(agents_tree))
        .route("/api/agents/{id}", get(get_agent).delete(kill_agent))
        .route("/api/agents/{id}/children", get(agent_children))
        .route("/api/agents/{id}/context", get(agent_context))
        .route("/api/agents/{id}/logs", get(agent_logs))
        .route("/api/agents/{id}/result", get(agent_result))
        .route("/api/agents/{id}/tree-status", get(agent_tree_status))
        // Messages
        .route("/api/agents/{id}/message", post(send_message))
        // Interactions
        .route(
            "/api/agents/{id}/interaction",
            get(get_interaction).post(submit_interaction),
        )
        // Config
        .route("/api/config", get(get_config))
        .route("/api/models", get(get_models))
        // WebSocket
        .route("/ws", get(ws_global))
        .route("/ws/agents/{id}", get(ws_agent))
        .layer(cors)
        .with_state(state);

    let addr: SocketAddr = format!("{}:{}", args.host, args.port).parse()?;
    info!("Listening on http://{}", addr);
    println!("Leviath API server listening on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

// ─── Blueprint endpoints ─────────────────────────────────────────────────────

fn agents_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".leviath")
        .join("agents")
}

/// Scan for blueprints from installed agents dir and configured agent_paths.
fn discover_blueprints(config: &Config) -> Vec<BlueprintInfo> {
    let mut results = Vec::new();
    let agents = agents_dir();

    let mut dirs_to_scan: Vec<PathBuf> = vec![agents];
    dirs_to_scan.extend(config.agent_paths.iter().cloned());

    for dir in dirs_to_scan {
        if !dir.exists() {
            continue;
        }
        // Check dir itself
        let manifest = dir.join("agent.leviath");
        if manifest.exists() {
            if let Some(info) = read_blueprint_info(&manifest, &dir) {
                results.push(info);
            }
        }
        // Check subdirs
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    let m = p.join("agent.leviath");
                    if m.exists() {
                        if let Some(info) = read_blueprint_info(&m, &p) {
                            results.push(info);
                        }
                    }
                }
            }
        }
    }

    results
}

#[derive(Debug, Serialize)]
struct BlueprintInfo {
    name: String,
    version: String,
    description: String,
    path: String,
    stages: Vec<String>,
}

fn read_blueprint_info(manifest_path: &Path, dir: &Path) -> Option<BlueprintInfo> {
    let content = std::fs::read_to_string(manifest_path).ok()?;
    let bp = parse_manifest_public(&content).ok()?;
    Some(BlueprintInfo {
        name: bp.name,
        version: bp.version,
        description: bp.description,
        path: dir.to_string_lossy().to_string(),
        stages: bp.stages.iter().map(|s| s.name.clone()).collect(),
    })
}

async fn list_blueprints(State(state): State<AppState>) -> Json<Vec<BlueprintInfo>> {
    Json(discover_blueprints(&state.config))
}

async fn get_blueprint(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<BlueprintInfo>, StatusCode> {
    let blueprints = discover_blueprints(&state.config);
    blueprints
        .into_iter()
        .find(|b| b.name == name)
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

#[derive(Deserialize)]
struct CreateBlueprintReq {
    name: String,
    manifest: String,
}

async fn create_blueprint(
    Json(body): Json<CreateBlueprintReq>,
) -> Result<Json<BlueprintInfo>, (StatusCode, Json<ErrorResponse>)> {
    // Validate manifest first
    if let Err(e) = parse_manifest_public(&body.manifest) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: format!("Invalid manifest: {}", e),
            }),
        ));
    }

    let dir = agents_dir().join(&body.name);
    std::fs::create_dir_all(&dir).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Failed to create directory: {}", e),
            }),
        )
    })?;

    let manifest_path = dir.join("agent.leviath");
    std::fs::write(&manifest_path, &body.manifest).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Failed to write manifest: {}", e),
            }),
        )
    })?;

    read_blueprint_info(&manifest_path, &dir)
        .map(Json)
        .ok_or_else(|| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: "Failed to read back created blueprint".to_string(),
                }),
            )
        })
}

#[derive(Deserialize)]
struct UpdateBlueprintReq {
    manifest: String,
}

async fn update_blueprint(
    AxumPath(name): AxumPath<String>,
    Json(body): Json<UpdateBlueprintReq>,
) -> Result<Json<BlueprintInfo>, (StatusCode, Json<ErrorResponse>)> {
    if let Err(e) = parse_manifest_public(&body.manifest) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: format!("Invalid manifest: {}", e),
            }),
        ));
    }

    let dir = agents_dir().join(&name);
    let manifest_path = dir.join("agent.leviath");
    if !manifest_path.exists() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Blueprint '{}' not found", name),
            }),
        ));
    }

    std::fs::write(&manifest_path, &body.manifest).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Failed to write manifest: {}", e),
            }),
        )
    })?;

    read_blueprint_info(&manifest_path, &dir)
        .map(Json)
        .ok_or_else(|| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: "Failed to read back updated blueprint".to_string(),
                }),
            )
        })
}

async fn delete_blueprint(
    AxumPath(name): AxumPath<String>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    let dir = agents_dir().join(&name);
    if !dir.exists() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Blueprint '{}' not found", name),
            }),
        ));
    }

    std::fs::remove_dir_all(&dir).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Failed to delete blueprint: {}", e),
            }),
        )
    })?;

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct ValidateBlueprintReq {
    manifest: String,
}

#[derive(Serialize, Deserialize)]
struct ValidateResponse {
    valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    errors: Option<Vec<String>>,
}

async fn validate_blueprint(Json(body): Json<ValidateBlueprintReq>) -> Json<ValidateResponse> {
    match parse_manifest_public(&body.manifest) {
        Ok(bp) => match bp.validate() {
            Ok(()) => Json(ValidateResponse {
                valid: true,
                errors: None,
            }),
            Err(e) => Json(ValidateResponse {
                valid: false,
                errors: Some(vec![e.to_string()]),
            }),
        },
        Err(e) => Json(ValidateResponse {
            valid: false,
            errors: Some(vec![e.to_string()]),
        }),
    }
}

// ─── Agent endpoints ─────────────────────────────────────────────────────────

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

#[derive(Deserialize)]
struct SpawnAgentReq {
    blueprint: String,
    task: String,
    model: Option<String>,
    max_depth: Option<usize>,
    #[serde(default)]
    yolo: bool,
    #[serde(default)]
    allow: Vec<String>,
    workdir: Option<String>,
    #[serde(default)]
    metadata: HashMap<String, String>,
    callback_url: Option<String>,
}

#[derive(Serialize)]
struct SpawnAgentResp {
    agent_id: String,
    run_id: String,
}

async fn spawn_agent(
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

#[derive(Deserialize)]
struct ListAgentsQuery {
    status: Option<String>,
}

async fn list_agents(Query(query): Query<ListAgentsQuery>) -> Json<Vec<RunMeta>> {
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

async fn get_agent(
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

async fn agent_children(AxumPath(id): AxumPath<String>) -> Json<Vec<RunMeta>> {
    let runs = runstate::list_runs();
    let children: Vec<RunMeta> = runs
        .into_iter()
        .filter(|r| r.parent_run_id.as_deref() == Some(&id))
        .collect();
    Json(children)
}

async fn agent_context(
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

#[derive(Deserialize)]
struct LogsQuery {
    tail: Option<u64>,
}

async fn agent_logs(
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

async fn agent_result(
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

#[derive(Serialize)]
struct AgentResultResp {
    run_id: String,
    status: String,
    output: String,
    error: Option<String>,
    prompt_tokens: usize,
    completion_tokens: usize,
}

async fn kill_agent(
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

// ─── Tree endpoints ──────────────────────────────────────────────────────────

#[derive(Serialize)]
struct AgentTreeNode {
    run_id: String,
    agent_name: String,
    status: String,
    stage: String,
    iteration: usize,
    prompt_tokens: usize,
    completion_tokens: usize,
    children: Vec<AgentTreeNode>,
}

fn build_tree(runs: &[RunMeta], parent_id: Option<&str>) -> Vec<AgentTreeNode> {
    runs.iter()
        .filter(|r| r.parent_run_id.as_deref() == parent_id)
        .map(|r| {
            let children = build_tree(runs, Some(&r.run_id));
            AgentTreeNode {
                run_id: r.run_id.clone(),
                agent_name: r.agent_name.clone(),
                status: format!("{}", r.status),
                stage: r.current_stage.clone(),
                iteration: r.iteration,
                prompt_tokens: r.prompt_tokens,
                completion_tokens: r.completion_tokens,
                children,
            }
        })
        .collect()
}

async fn agents_tree() -> Json<Vec<AgentTreeNode>> {
    let runs = runstate::list_runs();
    // Root nodes are those without a parent
    let tree = build_tree(&runs, None);
    Json(tree)
}

#[derive(Serialize)]
struct TreeStatusNode {
    run_id: String,
    agent_name: String,
    status: String,
    stage: String,
    prompt_tokens: usize,
    completion_tokens: usize,
    subtree_prompt_tokens: usize,
    subtree_completion_tokens: usize,
    children: Vec<TreeStatusNode>,
}

fn build_tree_status(runs: &[RunMeta], parent_id: Option<&str>) -> Vec<TreeStatusNode> {
    runs.iter()
        .filter(|r| r.parent_run_id.as_deref() == parent_id)
        .map(|r| {
            let children = build_tree_status(runs, Some(&r.run_id));
            let subtree_prompt: usize = r.prompt_tokens
                + children
                    .iter()
                    .map(|c| c.subtree_prompt_tokens)
                    .sum::<usize>();
            let subtree_completion: usize = r.completion_tokens
                + children
                    .iter()
                    .map(|c| c.subtree_completion_tokens)
                    .sum::<usize>();
            TreeStatusNode {
                run_id: r.run_id.clone(),
                agent_name: r.agent_name.clone(),
                status: format!("{}", r.status),
                stage: r.current_stage.clone(),
                prompt_tokens: r.prompt_tokens,
                completion_tokens: r.completion_tokens,
                subtree_prompt_tokens: subtree_prompt,
                subtree_completion_tokens: subtree_completion,
                children,
            }
        })
        .collect()
}

async fn agent_tree_status(
    AxumPath(id): AxumPath<String>,
) -> Result<Json<TreeStatusNode>, (StatusCode, Json<ErrorResponse>)> {
    let runs = runstate::list_runs();
    let root = runs.iter().find(|r| r.run_id == id).ok_or((
        StatusCode::NOT_FOUND,
        Json(ErrorResponse {
            error: format!("Agent run '{}' not found", id),
        }),
    ))?;

    let children = build_tree_status(&runs, Some(&id));
    let subtree_prompt: usize = root.prompt_tokens
        + children
            .iter()
            .map(|c| c.subtree_prompt_tokens)
            .sum::<usize>();
    let subtree_completion: usize = root.completion_tokens
        + children
            .iter()
            .map(|c| c.subtree_completion_tokens)
            .sum::<usize>();

    Ok(Json(TreeStatusNode {
        run_id: root.run_id.clone(),
        agent_name: root.agent_name.clone(),
        status: format!("{}", root.status),
        stage: root.current_stage.clone(),
        prompt_tokens: root.prompt_tokens,
        completion_tokens: root.completion_tokens,
        subtree_prompt_tokens: subtree_prompt,
        subtree_completion_tokens: subtree_completion,
        children,
    }))
}

// ─── Message endpoint ────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct SendMessageReq {
    message: String,
    #[allow(dead_code)]
    target_region: Option<String>,
}

async fn send_message(
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

// ─── Interaction endpoints ───────────────────────────────────────────────────

async fn get_interaction(
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

#[derive(Deserialize)]
struct SubmitInteractionReq {
    request_id: String,
    value: Option<String>,
    choice_index: Option<usize>,
    approved: Option<bool>,
    scope: Option<String>,
}

async fn submit_interaction(
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

// ─── Config endpoints ────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
struct RedactedConfig {
    default_provider: String,
    has_anthropic_key: bool,
    has_openai_key: bool,
    has_openrouter_key: bool,
    ollama_base_url: Option<String>,
    agent_paths: Vec<PathBuf>,
    registries: Vec<String>,
    mcp_server_count: usize,
}

async fn get_config(State(state): State<AppState>) -> Json<RedactedConfig> {
    let c = &*state.config;
    Json(RedactedConfig {
        default_provider: c.default_provider.clone(),
        has_anthropic_key: c.providers.anthropic_api_key.is_some(),
        has_openai_key: c.providers.openai_api_key.is_some(),
        has_openrouter_key: c.openrouter_api_key.is_some(),
        ollama_base_url: c.ollama_base_url.clone(),
        agent_paths: c.agent_paths.clone(),
        registries: c.registries.clone(),
        mcp_server_count: c.mcp_servers.len(),
    })
}

#[derive(Serialize)]
struct ModelEntry {
    id: String,
    provider: String,
    display_name: Option<String>,
    max_context_tokens: usize,
    max_output_tokens: usize,
    supports_tools: bool,
}

async fn get_models(State(state): State<AppState>) -> Json<Vec<ModelEntry>> {
    let registry = build_provider_registry(&state.config);
    let mut models = Vec::new();

    for provider_name in registry.provider_names() {
        if let Some(provider) = registry.get(provider_name) {
            if let Ok(list) = provider.list_models().await {
                for m in list {
                    models.push(ModelEntry {
                        id: m.id,
                        provider: m.provider,
                        display_name: m.display_name,
                        max_context_tokens: m.capabilities.max_context_tokens,
                        max_output_tokens: m.capabilities.max_output_tokens,
                        supports_tools: m.capabilities.supports_tools,
                    });
                }
            }
        }
    }

    Json(models)
}

// ─── WebSocket endpoints ─────────────────────────────────────────────────────

async fn ws_global(State(state): State<AppState>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, state.event_tx, None))
}

async fn ws_agent(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, state.event_tx, Some(id)))
}

async fn handle_ws(
    mut socket: WebSocket,
    event_tx: broadcast::Sender<ServerEvent>,
    filter_run_id: Option<String>,
) {
    let mut rx = event_tx.subscribe();

    loop {
        tokio::select! {
            event = rx.recv() => {
                match event {
                    Ok(ev) => {
                        // If filtering by run_id, skip non-matching events
                        if let Some(ref filter) = filter_run_id {
                            let event_run_id = match &ev {
                                ServerEvent::AgentStatus { run_id, .. } => run_id,
                                ServerEvent::ContextUpdate { run_id, .. } => run_id,
                                ServerEvent::Log { run_id, .. } => run_id,
                                ServerEvent::InteractionNeeded { run_id, .. } => run_id,
                                ServerEvent::AgentSpawned { run_id, .. } => run_id,
                                ServerEvent::AgentCompleted { run_id, .. } => run_id,
                                ServerEvent::Tokens { run_id, .. } => run_id,
                            };
                            if event_run_id != filter {
                                continue;
                            }
                        }

                        if let Ok(json) = serde_json::to_string(&ev) {
                            if socket.send(Message::Text(json.into())).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!("WebSocket subscriber lagged by {} events", n);
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {} // Ignore other client messages
                }
            }
        }
    }
}

// ─── Background polling loop ─────────────────────────────────────────────────

/// Cached state for change detection.
struct PollState {
    /// run_id → (status_string, iteration, prompt_tokens, completion_tokens)
    last_status: HashMap<String, (String, usize, usize, usize)>,
    /// run_id → total_tokens from last context snapshot
    last_context_tokens: HashMap<String, usize>,
    /// run_id → whether we saw a pending interaction
    last_pending: HashMap<String, bool>,
    /// run_id → set of run_ids we have already fired callbacks for
    callback_fired: HashMap<String, bool>,
}

async fn polling_loop(state: AppState) {
    let mut poll = PollState {
        last_status: HashMap::new(),
        last_context_tokens: HashMap::new(),
        last_pending: HashMap::new(),
        callback_fired: HashMap::new(),
    };

    let client = reqwest::Client::new();

    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;

        let runs = runstate::list_runs();

        for meta in &runs {
            let status_str = format!("{}", meta.status);
            let key = (
                status_str.clone(),
                meta.iteration,
                meta.prompt_tokens,
                meta.completion_tokens,
            );

            // Detect meta.json changes
            if poll.last_status.get(&meta.run_id) != Some(&key) {
                let _ = state.event_tx.send(ServerEvent::AgentStatus {
                    agent_id: meta.agent_name.clone(),
                    run_id: meta.run_id.clone(),
                    status: status_str.clone(),
                    stage: meta.current_stage.clone(),
                    iteration: meta.iteration,
                });

                // Token update
                let _ = state.event_tx.send(ServerEvent::Tokens {
                    agent_id: meta.agent_name.clone(),
                    run_id: meta.run_id.clone(),
                    prompt_tokens: meta.prompt_tokens,
                    completion_tokens: meta.completion_tokens,
                });

                // Detect completion
                let was_terminal = poll
                    .last_status
                    .get(&meta.run_id)
                    .map(|(s, _, _, _)| s == "Complete" || s == "Error" || s == "Cancelled")
                    .unwrap_or(false);

                if !was_terminal
                    && (meta.status == RunStatus::Complete || meta.status == RunStatus::Error)
                {
                    let _ = state.event_tx.send(ServerEvent::AgentCompleted {
                        agent_id: meta.agent_name.clone(),
                        run_id: meta.run_id.clone(),
                        status: status_str.clone(),
                        result: meta.error.clone(),
                    });

                    // Fire webhook callback if configured
                    if let Some(ref url) = meta.callback_url {
                        if !poll
                            .callback_fired
                            .get(&meta.run_id)
                            .copied()
                            .unwrap_or(false)
                        {
                            poll.callback_fired.insert(meta.run_id.clone(), true);
                            let payload = serde_json::json!({
                                "event": "agent_completed",
                                "run_id": meta.run_id,
                                "agent_id": meta.agent_name,
                                "status": status_str,
                                "result": meta.error,
                                "metadata": meta.metadata,
                                "tokens": {
                                    "prompt": meta.prompt_tokens,
                                    "completion": meta.completion_tokens,
                                }
                            });
                            let client = client.clone();
                            let url = url.clone();
                            tokio::spawn(async move {
                                if let Err(e) = client.post(&url).json(&payload).send().await {
                                    error!(url = %url, error = %e, "Webhook callback failed");
                                }
                            });
                        }
                    }
                }

                poll.last_status.insert(meta.run_id.clone(), key);
            }

            // Detect context.json changes
            if let Some(ctx) = runstate::read_context_snapshot(&meta.run_id) {
                let prev = poll.last_context_tokens.get(&meta.run_id).copied();
                if prev != Some(ctx.total_tokens) {
                    let _ = state.event_tx.send(ServerEvent::ContextUpdate {
                        agent_id: meta.agent_name.clone(),
                        run_id: meta.run_id.clone(),
                        total_tokens: ctx.total_tokens,
                        max_tokens: ctx.max_tokens,
                    });
                    poll.last_context_tokens
                        .insert(meta.run_id.clone(), ctx.total_tokens);
                }
            }

            // Detect pending.json
            let has_pending = interaction::read_request(&meta.run_id).is_some();
            let had_pending = poll
                .last_pending
                .get(&meta.run_id)
                .copied()
                .unwrap_or(false);
            if has_pending && !had_pending {
                if let Some(req) = interaction::read_request(&meta.run_id) {
                    let val = serde_json::to_value(&req).unwrap_or(serde_json::Value::Null);
                    let _ = state.event_tx.send(ServerEvent::InteractionNeeded {
                        agent_id: meta.agent_name.clone(),
                        run_id: meta.run_id.clone(),
                        request: val,
                    });
                }
            }
            poll.last_pending.insert(meta.run_id.clone(), has_pending);
        }

        // Clean up old entries for runs that no longer exist
        let run_ids: std::collections::HashSet<String> =
            runs.iter().map(|r| r.run_id.clone()).collect();
        poll.last_status.retain(|k, _| run_ids.contains(k));
        poll.last_context_tokens.retain(|k, _| run_ids.contains(k));
        poll.last_pending.retain(|k, _| run_ids.contains(k));
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn test_state() -> AppState {
        let (tx, _) = broadcast::channel(64);
        AppState {
            config: Arc::new(Config::default()),
            event_tx: tx,
        }
    }

    fn test_app() -> Router {
        let state = test_state();
        Router::new()
            .route("/api/blueprints", get(list_blueprints))
            .route("/api/blueprints/validate", post(validate_blueprint))
            .route(
                "/api/blueprints/{name}",
                get(get_blueprint).delete(delete_blueprint),
            )
            .route("/api/agents", get(list_agents))
            .route("/api/agents/tree", get(agents_tree))
            .route("/api/agents/{id}", get(get_agent))
            .route("/api/agents/{id}/children", get(agent_children))
            .route("/api/agents/{id}/context", get(agent_context))
            .route("/api/agents/{id}/logs", get(agent_logs))
            .route("/api/agents/{id}/result", get(agent_result))
            .route("/api/agents/{id}/tree-status", get(agent_tree_status))
            .route(
                "/api/agents/{id}/interaction",
                get(get_interaction).post(submit_interaction),
            )
            .route("/api/config", get(get_config))
            .with_state(state)
    }

    #[tokio::test]
    async fn test_list_blueprints() {
        let app = test_app();
        let req = Request::builder()
            .uri("/api/blueprints")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_get_blueprint_not_found() {
        let app = test_app();
        let req = Request::builder()
            .uri("/api/blueprints/nonexistent-agent-xyz")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_validate_blueprint_valid() {
        let app = test_app();
        let manifest = r#"
[agent]
name = "test-agent"
version = "0.1.0"
description = "A test"

[stages.main]
mode = "autonomous"
[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-4-6"
"#;
        let body = serde_json::json!({ "manifest": manifest });
        let req = Request::builder()
            .method("POST")
            .uri("/api/blueprints/validate")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let val: ValidateResponse = serde_json::from_slice(&body).unwrap();
        assert!(val.valid);
    }

    #[tokio::test]
    async fn test_validate_blueprint_invalid() {
        let app = test_app();
        let body = serde_json::json!({ "manifest": "not valid toml {{{{" });
        let req = Request::builder()
            .method("POST")
            .uri("/api/blueprints/validate")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let val: ValidateResponse = serde_json::from_slice(&body).unwrap();
        assert!(!val.valid);
        assert!(val.errors.is_some());
    }

    #[tokio::test]
    async fn test_list_agents() {
        let app = test_app();
        let req = Request::builder()
            .uri("/api/agents")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_agents_tree() {
        let app = test_app();
        let req = Request::builder()
            .uri("/api/agents/tree")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_get_agent_not_found() {
        let app = test_app();
        let req = Request::builder()
            .uri("/api/agents/nonexistent-run-id-xyz")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_agent_children_empty() {
        let app = test_app();
        let req = Request::builder()
            .uri("/api/agents/nonexistent/children")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // children returns 200 with empty array even if parent doesn't exist
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_agent_context_not_found() {
        let app = test_app();
        let req = Request::builder()
            .uri("/api/agents/nonexistent/context")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_agent_logs_not_found() {
        let app = test_app();
        let req = Request::builder()
            .uri("/api/agents/nonexistent/logs")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_agent_result_not_found() {
        let app = test_app();
        let req = Request::builder()
            .uri("/api/agents/nonexistent/result")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_agent_tree_status_not_found() {
        let app = test_app();
        let req = Request::builder()
            .uri("/api/agents/nonexistent/tree-status")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_interaction_not_found() {
        let app = test_app();
        let req = Request::builder()
            .uri("/api/agents/nonexistent/interaction")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_get_config() {
        let app = test_app();
        let req = Request::builder()
            .uri("/api/config")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let val: RedactedConfig = serde_json::from_slice(&body).unwrap();
        assert_eq!(val.default_provider, "anthropic");
        // Default config has no keys
        assert!(!val.has_anthropic_key);
        assert!(!val.has_openai_key);
    }

    #[tokio::test]
    async fn test_tree_building() {
        // Unit test for the tree builder
        let runs = vec![
            RunMeta::new(
                "parent-1".to_string(),
                "agent-a".to_string(),
                "/path".to_string(),
                "task".to_string(),
                None,
                "/work".to_string(),
                1,
            ),
            {
                let mut child = RunMeta::new(
                    "child-1".to_string(),
                    "agent-b".to_string(),
                    "/path".to_string(),
                    "sub-task".to_string(),
                    None,
                    "/work".to_string(),
                    1,
                );
                child.parent_run_id = Some("parent-1".to_string());
                child.prompt_tokens = 100;
                child.completion_tokens = 50;
                child
            },
        ];

        let tree = build_tree_status(&runs, None);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].run_id, "parent-1");
        assert_eq!(tree[0].children.len(), 1);
        assert_eq!(tree[0].subtree_prompt_tokens, 100); // parent (0) + child (100)
        assert_eq!(tree[0].subtree_completion_tokens, 50);
    }

    #[tokio::test]
    async fn test_delete_blueprint_not_found() {
        let app = test_app();
        let req = Request::builder()
            .method("DELETE")
            .uri("/api/blueprints/nonexistent-agent-xyz")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_server_event_serialization() {
        let event = ServerEvent::AgentStatus {
            agent_id: "coder".to_string(),
            run_id: "run-123".to_string(),
            status: "running".to_string(),
            stage: "implement".to_string(),
            iteration: 5,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"agent_status\""));
        assert!(json.contains("\"agent_id\":\"coder\""));

        let event2 = ServerEvent::Tokens {
            agent_id: "coder".to_string(),
            run_id: "run-123".to_string(),
            prompt_tokens: 5000,
            completion_tokens: 1200,
        };
        let json2 = serde_json::to_string(&event2).unwrap();
        assert!(json2.contains("\"type\":\"tokens\""));
        assert!(json2.contains("\"prompt_tokens\":5000"));
    }
}
