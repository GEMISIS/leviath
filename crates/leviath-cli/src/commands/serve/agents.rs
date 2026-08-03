//! Agent CRUD endpoints: spawn, list, get, kill, children, context, logs, result.

use std::path::PathBuf;

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use leviath_runtime::control_socket::{ControlRequest, ControlResponse};
use leviath_runtime::host::SpawnArgs;

use super::blueprints::discover_blueprints;
use super::types::*;
use crate::runstate::{self, ContextSnapshot, RunMeta};

/// `POST /api/agents`: spawn an agent into the shared-world daemon.
///
/// Resolves the blueprint's manifest path, mints a run id, and asks the daemon
/// (over the control socket) to create the agent; the daemon loads the blueprint,
/// resolves tools/model, and persists the run so the read endpoints observe it.
///
/// `yolo` / `allow` / `max_depth` from the request are forwarded through
/// [`SpawnArgs`] to the daemon's tool-policy resolution.
pub(super) async fn spawn_agent(
    State(state): State<AppState>,
    Json(body): Json<SpawnAgentReq>,
) -> Result<Json<SpawnAgentResp>, (StatusCode, Json<ErrorResponse>)> {
    let blueprints = discover_blueprints(&state.config);
    let bp_info = blueprints
        .iter()
        .find(|b| b.name == body.blueprint)
        .ok_or_else(|| {
            err(
                StatusCode::NOT_FOUND,
                format!("Blueprint '{}' not found", body.blueprint),
            )
        })?;
    let manifest_path = PathBuf::from(&bp_info.path).join("agent.leviath");

    let workdir = body.workdir.clone().unwrap_or_else(|| {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default()
    });
    // A caller-supplied workdir was accepted verbatim, so `{"workdir": "/"}`
    // pointed a tool-executing agent at the whole filesystem. `--workdir-root`
    // is the operator's answer to "where is this API allowed to work".
    state
        .limits
        .check_workdir(std::path::Path::new(&workdir))
        .map_err(|e| err(StatusCode::FORBIDDEN, e))?;
    // Likewise `{"yolo": true}` waived every approval prompt for an agent
    // running on the host, from a request. `--no-remote-yolo` refuses it.
    if body.yolo && state.limits.no_remote_yolo {
        return Err(err(
            StatusCode::FORBIDDEN,
            "this server refuses `yolo` on spawn requests (--no-remote-yolo)".to_string(),
        ));
    }
    // And a completion webhook is a request the daemon makes on the caller's
    // behalf, so it goes through the same SSRF policy as any model-supplied URL.
    if let Some(callback) = body.callback_url.as_deref() {
        state
            .limits
            .check_callback_url(callback)
            .map_err(|e| err(StatusCode::FORBIDDEN, e))?;
    }
    let run_id = runstate::new_run_id(&body.blueprint);
    let args = SpawnArgs {
        run_id,
        blueprint_path: manifest_path.to_string_lossy().to_string(),
        task: body.task.clone(),
        regions: body.regions.clone(),
        model: body.model.clone(),
        workdir,
        metadata: body.metadata.clone(),
        callback_url: body.callback_url.clone(),
        callback_secret: body.callback_secret.clone(),
        yolo: body.yolo,
        no_seed_commands: body.no_seed_commands,
        allow: body.allow.clone(),
        max_depth: body.max_depth,
        // Serve spawns are top-level runs.
        parent_run_id: None,
    };

    match state.control.spawn(args).await {
        Ok(ControlResponse::Spawned { run_id }) => {
            let _ = state.event_tx.send(ServerEvent::AgentSpawned {
                agent_id: run_id.clone(),
                run_id: run_id.clone(),
                parent_id: None,
                blueprint: body.blueprint.clone(),
            });
            tracing::info!(run_id = %run_id, blueprint = %body.blueprint, "spawned agent via API");
            Ok(Json(SpawnAgentResp {
                agent_id: run_id.clone(),
                run_id,
            }))
        }
        Ok(ControlResponse::Error { message }) => Err(err(
            StatusCode::BAD_REQUEST,
            format!("Failed to spawn agent: {message}"),
        )),
        Ok(other) => Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Unexpected daemon response: {other:?}"),
        )),
        Err(e) => Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("Daemon not reachable: {e}"),
        )),
    }
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

    // `.redacted()`: `RunMeta` carries the webhook signing secret, and this
    // handler serializes it whole.
    Json(runs.iter().map(RunMeta::redacted).collect())
}

pub(super) async fn get_agent(
    AxumPath(id): AxumPath<String>,
) -> Result<Json<RunMeta>, (StatusCode, Json<ErrorResponse>)> {
    runstate::read_meta(&id)
        .map(|m| Json(m.redacted()))
        .map_err(|_| {
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
        .map(|r| r.redacted())
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

pub(super) async fn agent_context_history(
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Vec<leviath_core::run_archive::RunPoint>>, (StatusCode, Json<ErrorResponse>)> {
    let history = runstate::context_history(&id);
    if history.is_empty() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("No context history for run '{}'", id),
            }),
        ));
    }
    Ok(Json(history))
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

/// How much of a file `GET /api/agents/{id}/files` returns: 1 MiB, enough for
/// any report a browser would render, small enough to hand out in one JSON body.
pub(super) const MAX_FILE_READ_BYTES: u64 = 1024 * 1024;

/// `GET /api/agents/{id}/files?path=<path>`: read a file the run wrote, so the
/// browser can render an agent's report without shell access to the host.
///
/// `path` may be relative (resolved against the run's workdir) or absolute;
/// either way the *resolved* path must still land inside the workdir, under the
/// same symlink-aware containment the file tools use
/// ([`leviath_core::resolves_within`]) - so this endpoint can read exactly what
/// the run was already allowed to write, and nothing else. Reads are capped at
/// [`MAX_FILE_READ_BYTES`]; a larger file comes back truncated and says so.
/// Purely a filesystem read: it works with the daemon down, like the other
/// read endpoints.
pub(super) async fn agent_file(
    AxumPath(id): AxumPath<String>,
    Query(query): Query<FileQuery>,
) -> Result<Json<FileContentResp>, (StatusCode, Json<ErrorResponse>)> {
    let meta = runstate::read_meta(&id)
        .map_err(|_| err(StatusCode::NOT_FOUND, format!("Agent run '{id}' not found")))?;

    let workdir = PathBuf::from(&meta.workdir);
    let requested = PathBuf::from(&query.path);
    let resolved = match requested.is_absolute() {
        true => requested,
        false => workdir.join(&requested),
    };
    if !leviath_core::resolves_within(&resolved, &workdir) {
        return Err(err(
            StatusCode::FORBIDDEN,
            format!(
                "path '{}' is outside the run's working directory",
                query.path
            ),
        ));
    }

    let size = match std::fs::metadata(&resolved) {
        Ok(m) if m.is_dir() => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                format!("'{}' is a directory, not a file", query.path),
            ));
        }
        Ok(m) => m.len(),
        Err(_) => {
            return Err(err(
                StatusCode::NOT_FOUND,
                format!("file '{}' not found", query.path),
            ));
        }
    };

    let mut bytes = Vec::new();
    if let Err(e) = std::fs::File::open(&resolved)
        .map(|f| std::io::Read::take(f, MAX_FILE_READ_BYTES))
        .and_then(|mut f| std::io::Read::read_to_end(&mut f, &mut bytes))
    {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!("could not read '{}': {e}", query.path),
        ));
    }
    let truncated = size > MAX_FILE_READ_BYTES;
    let read_len = bytes.len();
    let content = match String::from_utf8(bytes) {
        Ok(s) => s,
        // The 1 MiB cap can land mid-character in a file that is otherwise
        // valid UTF-8. That is the cap's doing, not the file's: drop the
        // split character's leading bytes rather than calling a text file
        // binary. (`valid_up_to` is at most 3 bytes short of the cap when
        // the only problem is the cut.)
        Err(e) if truncated && e.utf8_error().valid_up_to() + 4 > read_len => {
            let valid = e.utf8_error().valid_up_to();
            let mut prefix = e.into_bytes();
            prefix.truncate(valid);
            String::from_utf8_lossy(&prefix).into_owned()
        }
        Err(_) => {
            return Err(err(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                format!("'{}' is not a text file", query.path),
            ));
        }
    };

    Ok(Json(FileContentResp {
        path: resolved.to_string_lossy().into_owned(),
        size,
        content,
        truncated,
    }))
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

/// `DELETE /api/agents/{id}`: cancel a run in the shared-world daemon. The
/// daemon cancels the agent (cascading to its sub-agents in the one world) and
/// persists the terminal status.
pub(super) async fn kill_agent(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    match state
        .control
        .request(&ControlRequest::Cancel { run_id: id.clone() })
        .await
    {
        Ok(ControlResponse::Ok { ok: true }) => Ok(StatusCode::NO_CONTENT),
        Ok(ControlResponse::Ok { ok: false }) => Err(err(
            StatusCode::NOT_FOUND,
            format!("Agent run '{id}' not found"),
        )),
        Ok(other) => Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Unexpected daemon response: {other:?}"),
        )),
        Err(e) => Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("Daemon not reachable: {e}"),
        )),
    }
}

/// `POST /api/agents/{id}/pause`: park a run. The daemon refuses when the run
/// does not exist or is not pausable (waiting on input, or finished), which
/// both surface as 404 - the daemon's reply does not distinguish them.
pub(super) async fn pause_agent(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    match state
        .control
        .request(&ControlRequest::Pause { run_id: id.clone() })
        .await
    {
        Ok(ControlResponse::Ok { ok: true }) => Ok(StatusCode::NO_CONTENT),
        Ok(ControlResponse::Ok { ok: false }) => Err(err(
            StatusCode::NOT_FOUND,
            format!("Agent run '{id}' not found or not pausable"),
        )),
        Ok(other) => Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Unexpected daemon response: {other:?}"),
        )),
        Err(e) => Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("Daemon not reachable: {e}"),
        )),
    }
}

/// `POST /api/agents/{id}/resume`: un-pause a run.
pub(super) async fn resume_agent(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    match state
        .control
        .request(&ControlRequest::Resume { run_id: id.clone() })
        .await
    {
        Ok(ControlResponse::Ok { ok: true }) => Ok(StatusCode::NO_CONTENT),
        Ok(ControlResponse::Ok { ok: false }) => Err(err(
            StatusCode::NOT_FOUND,
            format!("Agent run '{id}' not found or not paused"),
        )),
        Ok(other) => Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Unexpected daemon response: {other:?}"),
        )),
        Err(e) => Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("Daemon not reachable: {e}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::get;
    use std::sync::Arc;
    use tokio::sync::broadcast;
    use tower::ServiceExt;

    use crate::commands::serve::testutil::fake_daemon;
    use crate::config::Config;
    use crate::runstate::{RunMeta, RunStatus, create_run};
    use leviath_runtime::control_socket::ControlClient;

    /// A control client at an address with no daemon (read endpoints don't use it).
    fn no_daemon() -> ControlClient {
        ControlClient::new(leviath_runtime::control_socket::control_id(
            std::path::Path::new("/no/such/daemon"),
        ))
    }

    fn test_state() -> AppState {
        let (tx, _) = broadcast::channel(64);
        AppState {
            config: Arc::new(Config::default()),
            event_tx: tx,
            control: no_daemon(),
            mcp: crate::commands::serve::mcp::McpAdmin::default(),
            limits: Default::default(),
        }
    }

    /// The refusals as the handler returns them: a 403 before the request ever
    /// reaches the daemon, so a token holder cannot point an agent at the
    /// filesystem root or waive its approval prompts.
    #[tokio::test]
    async fn spawn_refuses_an_out_of_root_workdir_and_remote_yolo() {
        let root = tempfile::tempdir().unwrap();
        let agents = tempfile::tempdir().unwrap();
        let bp = agents.path().join("probe");
        std::fs::create_dir(&bp).unwrap();
        std::fs::write(
            bp.join("agent.leviath"),
            "[agent]\nname = \"probe\"\nversion = \"1.0.0\"\ndescription = \"d\"\n\n\
             [stages.main]\nprompt = \"p\"\n",
        )
        .unwrap();

        let (tx, _) = broadcast::channel(64);
        let state = AppState {
            config: Arc::new(Config {
                agent_paths: vec![agents.path().to_path_buf()],
                ..Default::default()
            }),
            event_tx: tx,
            control: no_daemon(),
            mcp: crate::commands::serve::mcp::McpAdmin::default(),
            limits: Arc::new(ServeLimits {
                allow_local_network: false,
                workdir_root: Some(root.path().to_path_buf()),
                no_remote_yolo: true,
            }),
        };

        // Outside `--workdir-root`.
        let err = spawn_agent(
            State(state.clone()),
            Json(SpawnAgentReq {
                blueprint: "probe".to_string(),
                task: "t".to_string(),
                workdir: Some("/".to_string()),
                ..Default::default()
            }),
        )
        .await
        .expect_err("a workdir outside the root must be refused");
        assert_eq!(err.0, StatusCode::FORBIDDEN);
        assert!(
            err.1.0.error.contains("--workdir-root"),
            "{}",
            err.1.0.error
        );

        // Inside the root, but pointing the completion webhook at the cloud
        // metadata service - a request the daemon would make on the caller's
        // behalf, from inside the trust boundary.
        let err = spawn_agent(
            State(state.clone()),
            Json(SpawnAgentReq {
                blueprint: "probe".to_string(),
                task: "t".to_string(),
                workdir: Some(root.path().to_string_lossy().to_string()),
                callback_url: Some("http://169.254.169.254/latest/meta-data/".to_string()),
                ..Default::default()
            }),
        )
        .await
        .expect_err("a webhook aimed at link-local must be refused");
        assert_eq!(err.0, StatusCode::FORBIDDEN);
        assert!(err.1.0.error.contains("callback_url"), "{}", err.1.0.error);

        // Inside the root, but asking for yolo.
        let err = spawn_agent(
            State(state),
            Json(SpawnAgentReq {
                blueprint: "probe".to_string(),
                task: "t".to_string(),
                workdir: Some(root.path().to_string_lossy().to_string()),
                yolo: true,
                ..Default::default()
            }),
        )
        .await
        .expect_err("remote yolo must be refused");
        assert_eq!(err.0, StatusCode::FORBIDDEN);
        assert!(
            err.1.0.error.contains("no-remote-yolo"),
            "{}",
            err.1.0.error
        );
    }

    /// `--workdir-root` bounds where an API caller may point an agent. Without
    /// it, `{"workdir": "/"}` handed a tool-executing agent the whole
    /// filesystem.
    #[test]
    fn workdir_root_bounds_the_requested_workdir() {
        let root = tempfile::tempdir().unwrap();
        let inside = root.path().join("project");
        std::fs::create_dir(&inside).unwrap();
        let limits = ServeLimits {
            workdir_root: Some(root.path().to_path_buf()),
            ..Default::default()
        };

        assert!(limits.check_workdir(&inside).is_ok());
        assert!(limits.check_workdir(root.path()).is_ok());
        let err = limits.check_workdir(std::path::Path::new("/")).unwrap_err();
        assert!(err.contains("--workdir-root"), "{err}");
    }

    /// A completion webhook is a request the daemon makes on the caller's
    /// behalf, from inside the trust boundary and with retries. Unchecked,
    /// `"callback_url": "http://169.254.169.254/…"` turned the API into a
    /// repeatable request primitive against cloud metadata and the local
    /// network.
    #[test]
    fn a_callback_url_goes_through_the_same_ssrf_policy_as_any_other() {
        let limits = ServeLimits::default();
        for blocked in [
            "http://169.254.169.254/latest/meta-data/",
            "http://127.0.0.1:9200/_shutdown",
            "http://192.168.1.1/admin",
            "file:///etc/passwd",
        ] {
            let err = limits
                .check_callback_url(blocked)
                .expect_err("{blocked} must be refused");
            assert!(err.contains("callback_url"), "{err}");
        }
        assert!(limits.check_callback_url("not a url").is_err());
    }

    /// The opt-out an operator can take deliberately.
    #[test]
    fn allow_local_network_lets_a_webhook_reach_this_host() {
        let limits = ServeLimits {
            allow_local_network: true,
            ..Default::default()
        };
        // The same URL the default refuses. That this flips on the setting --
        // rather than everything being refused either way - is what shows the
        // check is reading the address and not just saying no.
        let url = "http://127.0.0.1:9000/hook";
        assert!(limits.check_callback_url(url).is_ok());
        assert!(ServeLimits::default().check_callback_url(url).is_err());
    }

    /// The containment is symlink-aware, so a link planted under the root cannot
    /// be used to walk out of it.
    #[cfg(unix)]
    #[test]
    fn workdir_root_is_not_fooled_by_a_symlink() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
        let limits = ServeLimits {
            workdir_root: Some(root.path().to_path_buf()),
            ..Default::default()
        };
        assert!(limits.check_workdir(&root.path().join("escape")).is_err());
    }

    /// With no `--workdir-root`, behavior is unchanged - the flag is opt-in for
    /// operators, not a new hard requirement on every deployment.
    #[test]
    fn no_workdir_root_permits_anything() {
        let limits = ServeLimits::default();
        assert!(limits.check_workdir(std::path::Path::new("/")).is_ok());
    }

    fn unique_run_id(prefix: &str) -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        format!("test-{}-{}-{}", prefix, std::process::id(), id)
    }

    fn make_run(id: &str) -> RunMeta {
        RunMeta::new(
            id.to_string(),
            "test-agent".to_string(),
            "/path/to/agent".to_string(),
            "do something".to_string(),
            None,
            "/tmp".to_string(),
            1,
        )
    }

    fn test_state_with_agent_paths(paths: Vec<PathBuf>, control: ControlClient) -> AppState {
        let (tx, _) = broadcast::channel(64);
        AppState {
            config: Arc::new(Config {
                agent_paths: paths,
                ..Default::default()
            }),
            event_tx: tx,
            control,
            mcp: crate::commands::serve::mcp::McpAdmin::default(),
            limits: Default::default(),
        }
    }

    fn write_test_blueprint(dir: &std::path::Path, name: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("agent.leviath"),
            format!(
                r#"
[agent]
name = "{name}"
version = "1.0.0"
description = "A spawnable test blueprint"

[stages.plan]
prompt = "Plan the work"
"#
            ),
        )
        .unwrap();
    }

    // ─── spawn_agent ──────────────────────────────────────────────────────────

    /// A router over `POST /api/agents` backed by `control`, plus a temp agents
    /// dir holding one discoverable blueprint named "spawnable".
    fn spawn_app(control: ControlClient) -> (Router, tempfile::TempDir) {
        let agents = tempfile::tempdir().unwrap();
        write_test_blueprint(&agents.path().join("spawnable"), "spawnable");
        // A sibling subdir with no manifest, so blueprint discovery exercises the
        // "subdir without agent.leviath" branch.
        std::fs::create_dir_all(agents.path().join("not-a-blueprint")).unwrap();
        let state = test_state_with_agent_paths(vec![agents.path().to_path_buf()], control);
        let app = Router::new()
            .route("/api/agents", axum::routing::post(spawn_agent))
            .with_state(state);
        (app, agents)
    }

    async fn post_spawn(app: Router, body: &str) -> StatusCode {
        let req = Request::builder()
            .method("POST")
            .uri("/api/agents")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        app.oneshot(req).await.unwrap().status()
    }

    #[tokio::test]
    async fn spawn_agent_blueprint_not_found_returns_404() {
        let (app, _agents) = spawn_app(no_daemon());
        assert_eq!(
            post_spawn(app, r#"{"blueprint":"ghost","task":"t"}"#).await,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn spawn_agent_success_returns_ok() {
        let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Spawned {
            run_id: "run-1".to_string(),
        });
        let (app, _agents) = spawn_app(control);
        assert_eq!(
            post_spawn(
                app,
                r#"{"blueprint":"spawnable","task":"do it","workdir":"/tmp"}"#
            )
            .await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn spawn_agent_without_workdir_falls_back_to_cwd() {
        let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Spawned {
            run_id: "r".to_string(),
        });
        let (app, _agents) = spawn_app(control);
        // No workdir field → the handler falls back to the current directory.
        assert_eq!(
            post_spawn(app, r#"{"blueprint":"spawnable","task":"t"}"#).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn spawn_agent_daemon_error_returns_400() {
        let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Error {
            message: "bad blueprint".to_string(),
        });
        let (app, _agents) = spawn_app(control);
        assert_eq!(
            post_spawn(app, r#"{"blueprint":"spawnable","task":"t"}"#).await,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn spawn_agent_unexpected_response_returns_500() {
        let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Ok { ok: true });
        let (app, _agents) = spawn_app(control);
        assert_eq!(
            post_spawn(app, r#"{"blueprint":"spawnable","task":"t"}"#).await,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[tokio::test]
    async fn spawn_agent_daemon_absent_returns_503() {
        let (app, _agents) = spawn_app(no_daemon());
        assert_eq!(
            post_spawn(app, r#"{"blueprint":"spawnable","task":"t"}"#).await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
    // ─── list_agents ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn list_agents_no_filter_returns_ok() {
        let app = Router::new()
            .route("/api/agents", get(list_agents))
            .with_state(test_state());
        let req = Request::builder()
            .uri("/api/agents")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
    }

    #[tokio::test]
    async fn list_agents_with_status_filter_running() {
        crate::runstate::with_isolated_runs_dir_async(
            "list_agents_with_status_filter_running",
            |_d| async move {
                let run_id = unique_run_id("list-filter");
                let mut meta = make_run(&run_id);
                meta.status = RunStatus::Running;
                create_run(&meta).unwrap();

                let app = Router::new()
                    .route("/api/agents", get(list_agents))
                    .with_state(test_state());
                let req = Request::builder()
                    .uri("/api/agents?status=running")
                    .body(Body::empty())
                    .unwrap();
                let resp = app.oneshot(req).await.unwrap();
                assert_eq!(resp.status(), axum::http::StatusCode::OK);
                let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let runs: Vec<RunMeta> = serde_json::from_slice(&body).unwrap();
                // The run we created has Running status
                let found = runs.iter().any(|r| r.run_id == run_id);
                assert_found_running_run(found);

                // Cleanup
                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    fn assert_found_running_run(found: bool) {
        assert!(found, "should find the running run");
    }

    #[test]
    #[should_panic(expected = "should find the running run")]
    fn assert_found_running_run_panics_when_not_found() {
        assert_found_running_run(false);
    }

    #[tokio::test]
    async fn list_agents_with_status_filter_excludes_others() {
        crate::runstate::with_isolated_runs_dir_async(
            "list_agents_with_status_filter_excludes_others",
            |_d| async move {
                let run_id = unique_run_id("list-filter-excl");
                let mut meta = make_run(&run_id);
                meta.status = RunStatus::Complete;
                create_run(&meta).unwrap();

                // Create a second run with Running status so the filtered list is
                // non-empty - this makes the map/any closure in the assertion actually
                // execute, covering the closure body in LLVM's instrumentation.
                let run_id2 = unique_run_id("list-filter-excl-running");
                let mut meta2 = make_run(&run_id2);
                meta2.status = RunStatus::Running;
                create_run(&meta2).unwrap();

                let app = Router::new()
                    .route("/api/agents", get(list_agents))
                    .with_state(test_state());
                let req = Request::builder()
                    .uri("/api/agents?status=running")
                    .body(Body::empty())
                    .unwrap();
                let resp = app.oneshot(req).await.unwrap();
                assert_eq!(resp.status(), axum::http::StatusCode::OK);
                let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let runs: Vec<RunMeta> = serde_json::from_slice(&body).unwrap();
                // The complete run should not appear in the 'running' filter.
                let found = runs.iter().any(|r| r.run_id == run_id);
                assert_complete_run_excluded(found);
                // The running run should appear.
                let found2 = runs.iter().any(|r| r.run_id == run_id2);
                assert_running_run_included(found2);

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id2));
            },
        )
        .await;
    }

    fn assert_complete_run_excluded(found: bool) {
        assert!(!found, "complete run should not appear in 'running' filter");
    }

    #[test]
    #[should_panic(expected = "complete run should not appear in 'running' filter")]
    fn assert_complete_run_excluded_panics_when_found() {
        assert_complete_run_excluded(true);
    }

    fn assert_running_run_included(found2: bool) {
        assert!(found2, "running run should appear in 'running' filter");
    }

    #[test]
    #[should_panic(expected = "running run should appear in 'running' filter")]
    fn assert_running_run_included_panics_when_not_found() {
        assert_running_run_included(false);
    }

    #[tokio::test]
    async fn list_agents_multi_status_filter() {
        crate::runstate::with_isolated_runs_dir_async(
            "list_agents_multi_status_filter",
            |_d| async move {
                let run_id = unique_run_id("list-multi");
                let mut meta = make_run(&run_id);
                meta.status = RunStatus::Error;
                create_run(&meta).unwrap();

                let app = Router::new()
                    .route("/api/agents", get(list_agents))
                    .with_state(test_state());
                let req = Request::builder()
                    .uri("/api/agents?status=running,error")
                    .body(Body::empty())
                    .unwrap();
                let resp = app.oneshot(req).await.unwrap();
                assert_eq!(resp.status(), axum::http::StatusCode::OK);
                let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let runs: Vec<RunMeta> = serde_json::from_slice(&body).unwrap();
                let found = runs.iter().any(|r| r.run_id == run_id);
                assert_error_run_included(found);

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    fn assert_error_run_included(found: bool) {
        assert!(found, "error run should appear in 'running,error' filter");
    }

    #[test]
    #[should_panic(expected = "error run should appear in 'running,error' filter")]
    fn assert_error_run_included_panics_when_not_found() {
        assert_error_run_included(false);
    }

    // ─── get_agent ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn get_agent_existing_run_returns_ok() {
        crate::runstate::with_isolated_runs_dir_async(
            "get_agent_existing_run_returns_ok",
            |_d| async move {
                let run_id = unique_run_id("get-agent");
                let meta = make_run(&run_id);
                create_run(&meta).unwrap();

                let app = Router::new()
                    .route("/api/agents/{id}", get(get_agent))
                    .with_state(test_state());
                let req = Request::builder()
                    .uri(format!("/api/agents/{}", run_id))
                    .body(Body::empty())
                    .unwrap();
                let resp = app.oneshot(req).await.unwrap();
                assert_eq!(resp.status(), axum::http::StatusCode::OK);
                let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let got: RunMeta = serde_json::from_slice(&body).unwrap();
                assert_eq!(got.run_id, run_id);

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    #[tokio::test]
    async fn get_agent_nonexistent_returns_404() {
        let app = Router::new()
            .route("/api/agents/{id}", get(get_agent))
            .with_state(test_state());
        let req = Request::builder()
            .uri("/api/agents/totally-nonexistent-run-id-12345")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
    }

    // ─── agent_children ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn agent_children_with_children_found() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_children_with_children_found",
            |_d| async move {
                let parent_id = unique_run_id("parent");
                let child_id = unique_run_id("child");

                let parent = make_run(&parent_id);
                create_run(&parent).unwrap();

                let mut child = make_run(&child_id);
                child.parent_run_id = Some(parent_id.clone());
                create_run(&child).unwrap();

                let app = Router::new()
                    .route("/api/agents/{id}/children", get(agent_children))
                    .with_state(test_state());
                let req = Request::builder()
                    .uri(format!("/api/agents/{}/children", parent_id))
                    .body(Body::empty())
                    .unwrap();
                let resp = app.oneshot(req).await.unwrap();
                assert_eq!(resp.status(), axum::http::StatusCode::OK);
                let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let children: Vec<RunMeta> = serde_json::from_slice(&body).unwrap();
                assert_child_run_appears(children.iter().any(|c| c.run_id == child_id));

                let _ = std::fs::remove_dir_all(runstate::run_dir(&parent_id));
                let _ = std::fs::remove_dir_all(runstate::run_dir(&child_id));
            },
        )
        .await;
    }

    fn assert_child_run_appears(found: bool) {
        assert!(found, "child run should appear");
    }

    #[test]
    #[should_panic(expected = "child run should appear")]
    fn assert_child_run_appears_panics_when_not_found() {
        assert_child_run_appears(false);
    }

    #[tokio::test]
    async fn agent_children_no_children_returns_empty() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_children_no_children_returns_empty",
            |_d| async move {
                let run_id = unique_run_id("no-children");
                let meta = make_run(&run_id);
                create_run(&meta).unwrap();

                let app = Router::new()
                    .route("/api/agents/{id}/children", get(agent_children))
                    .with_state(test_state());
                let req = Request::builder()
                    .uri(format!("/api/agents/{}/children", run_id))
                    .body(Body::empty())
                    .unwrap();
                let resp = app.oneshot(req).await.unwrap();
                assert_eq!(resp.status(), axum::http::StatusCode::OK);
                let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let children: Vec<RunMeta> = serde_json::from_slice(&body).unwrap();
                assert_no_self_in_children(&children);

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    fn assert_no_self_in_children(children: &[RunMeta]) {
        assert!(
            children.is_empty(),
            "run itself should not appear in its own children list"
        );
    }

    #[test]
    #[should_panic(expected = "run itself should not appear in its own children list")]
    fn assert_no_self_in_children_panics_when_nonempty() {
        assert_no_self_in_children(&[make_run("bogus-child")]);
    }

    // ─── agent_context ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn agent_context_with_snapshot_returns_ok() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_context_with_snapshot_returns_ok",
            |_d| async move {
                let run_id = unique_run_id("ctx-snap");
                let meta = make_run(&run_id);
                create_run(&meta).unwrap();

                // Write a context snapshot
                let snap = runstate::ContextSnapshot {
                    stage_name: "plan".to_string(),
                    total_tokens: 5000,
                    max_tokens: 200000,
                    regions: vec![],
                };
                runstate::write_context_snapshot(&run_id, &snap).unwrap();

                let app = Router::new()
                    .route("/api/agents/{id}/context", get(agent_context))
                    .with_state(test_state());
                let req = Request::builder()
                    .uri(format!("/api/agents/{}/context", run_id))
                    .body(Body::empty())
                    .unwrap();
                let resp = app.oneshot(req).await.unwrap();
                assert_eq!(resp.status(), axum::http::StatusCode::OK);
                let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let got: runstate::ContextSnapshot = serde_json::from_slice(&body).unwrap();
                assert_eq!(got.total_tokens, 5000);

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    #[tokio::test]
    async fn agent_context_no_snapshot_returns_404() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_context_no_snapshot_returns_404",
            |_d| async move {
                let run_id = unique_run_id("ctx-none");
                let meta = make_run(&run_id);
                create_run(&meta).unwrap();

                let app = Router::new()
                    .route("/api/agents/{id}/context", get(agent_context))
                    .with_state(test_state());
                let req = Request::builder()
                    .uri(format!("/api/agents/{}/context", run_id))
                    .body(Body::empty())
                    .unwrap();
                let resp = app.oneshot(req).await.unwrap();
                assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    /// Write a minimal `run.lvr` (Header + one ContextCheckpoint) for `run_id`.
    fn write_archive_fixture(run_id: &str) {
        use leviath_core::run_archive::{self, RunIdentity, RunRecord};
        let mut buf = Vec::new();
        run_archive::write_archive_start(&mut buf, run_archive::RUN_ARCHIVE_VERSION).unwrap();
        run_archive::write_record(
            &mut buf,
            &RunRecord::Header {
                identity: RunIdentity {
                    run_id: run_id.to_string(),
                    machine_id: "m".to_string(),
                    world_id: "w".to_string(),
                    created_at: 0,
                },
                meta: Box::new(make_run(run_id)),
            },
        )
        .unwrap();
        run_archive::write_record(
            &mut buf,
            &RunRecord::ContextCheckpoint {
                snapshot: runstate::ContextSnapshot {
                    stage_name: "plan".to_string(),
                    total_tokens: 7,
                    max_tokens: 100,
                    regions: vec![],
                },
                at: 1,
            },
        )
        .unwrap();
        std::fs::write(runstate::run_dir(run_id).join("run.lvr"), &buf).unwrap();
    }

    #[tokio::test]
    async fn agent_context_history_returns_ok() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_context_history_returns_ok",
            |_d| async move {
                let run_id = unique_run_id("ctx-hist");
                create_run(&make_run(&run_id)).unwrap();
                write_archive_fixture(&run_id);

                let app = Router::new()
                    .route(
                        "/api/agents/{id}/context/history",
                        get(agent_context_history),
                    )
                    .with_state(test_state());
                let req = Request::builder()
                    .uri(format!("/api/agents/{}/context/history", run_id))
                    .body(Body::empty())
                    .unwrap();
                let resp = app.oneshot(req).await.unwrap();
                assert_eq!(resp.status(), axum::http::StatusCode::OK);
                let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let got: Vec<leviath_core::run_archive::RunPoint> =
                    serde_json::from_slice(&body).unwrap();
                assert_eq!(got.len(), 1);
                assert_eq!(got[0].context.stage_name, "plan");

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    #[tokio::test]
    async fn agent_context_history_no_archive_returns_404() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_context_history_no_archive_returns_404",
            |_d| async move {
                let run_id = unique_run_id("ctx-hist-none");
                create_run(&make_run(&run_id)).unwrap();

                let app = Router::new()
                    .route(
                        "/api/agents/{id}/context/history",
                        get(agent_context_history),
                    )
                    .with_state(test_state());
                let req = Request::builder()
                    .uri(format!("/api/agents/{}/context/history", run_id))
                    .body(Body::empty())
                    .unwrap();
                let resp = app.oneshot(req).await.unwrap();
                assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    // ─── agent_logs ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn agent_logs_existing_run_returns_ok() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_logs_existing_run_returns_ok",
            |_d| async move {
                let run_id = unique_run_id("logs-ok");
                let meta = make_run(&run_id);
                create_run(&meta).unwrap();

                // Write something to output.log
                let log_path = runstate::run_dir(&run_id).join("output.log");
                std::fs::write(&log_path, "hello log\n").unwrap();

                let app = Router::new()
                    .route("/api/agents/{id}/logs", get(agent_logs))
                    .with_state(test_state());
                let req = Request::builder()
                    .uri(format!("/api/agents/{}/logs", run_id))
                    .body(Body::empty())
                    .unwrap();
                let resp = app.oneshot(req).await.unwrap();
                assert_eq!(resp.status(), axum::http::StatusCode::OK);

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    #[tokio::test]
    async fn agent_logs_with_tail_param() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_logs_with_tail_param",
            |_d| async move {
                let run_id = unique_run_id("logs-tail");
                let meta = make_run(&run_id);
                create_run(&meta).unwrap();

                let log_path = runstate::run_dir(&run_id).join("output.log");
                std::fs::write(&log_path, "line1\nline2\nline3\n").unwrap();

                let app = Router::new()
                    .route("/api/agents/{id}/logs", get(agent_logs))
                    .with_state(test_state());
                let req = Request::builder()
                    .uri(format!("/api/agents/{}/logs?tail=100", run_id))
                    .body(Body::empty())
                    .unwrap();
                let resp = app.oneshot(req).await.unwrap();
                assert_eq!(resp.status(), axum::http::StatusCode::OK);

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    #[tokio::test]
    async fn agent_logs_nonexistent_run_returns_404() {
        let app = Router::new()
            .route("/api/agents/{id}/logs", get(agent_logs))
            .with_state(test_state());
        let req = Request::builder()
            .uri("/api/agents/nonexistent-run-xyz-logs/logs")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
    }

    // ─── agent_file ───────────────────────────────────────────────────────────

    fn files_app() -> Router {
        Router::new()
            .route("/api/agents/{id}/files", get(agent_file))
            .with_state(test_state())
    }

    /// A run whose workdir is `workdir`, persisted so `read_meta` finds it.
    fn create_run_in(id: &str, workdir: &std::path::Path) -> RunMeta {
        let mut meta = make_run(id);
        meta.workdir = workdir.to_string_lossy().to_string();
        create_run(&meta).unwrap();
        meta
    }

    /// GET `/api/agents/{id}/files?path=<path>`, returning status and body.
    /// (`path` goes into the query string verbatim - every path these tests
    /// use is query-safe as-is.)
    async fn get_file(id: &str, path: &str) -> (StatusCode, Vec<u8>) {
        let uri = format!("/api/agents/{id}/files?path={path}");
        let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
        let resp = files_app().oneshot(req).await.unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, body.to_vec())
    }

    fn error_of(body: &[u8]) -> String {
        serde_json::from_slice::<serde_json::Value>(body).unwrap()["error"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn agent_file_unknown_run_returns_404() {
        let (status, body) = get_file("nonexistent-run-xyz-files", "report.md").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            error_of(&body),
            "Agent run 'nonexistent-run-xyz-files' not found"
        );
    }

    #[tokio::test]
    async fn agent_file_reads_a_relative_path_within_the_workdir() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_file_reads_a_relative_path_within_the_workdir",
            |_d| async move {
                let workdir = tempfile::tempdir().unwrap();
                std::fs::create_dir_all(workdir.path().join("notes")).unwrap();
                std::fs::write(workdir.path().join("notes/report.md"), "# Report\n").unwrap();
                let run_id = unique_run_id("file-rel");
                create_run_in(&run_id, workdir.path());

                let (status, body) = get_file(&run_id, "notes/report.md").await;
                assert_eq!(status, StatusCode::OK);
                let got: FileContentResp = serde_json::from_slice(&body).unwrap();
                assert_eq!(got.content, "# Report\n");
                assert_eq!(got.size, 9);
                assert!(!got.truncated);
                // The reported path is the resolved absolute one.
                assert!(std::path::Path::new(&got.path).is_absolute());
                assert!(got.path.ends_with("report.md"));

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    #[tokio::test]
    async fn agent_file_reads_an_absolute_path_within_the_workdir() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_file_reads_an_absolute_path_within_the_workdir",
            |_d| async move {
                let workdir = tempfile::tempdir().unwrap();
                let file = workdir.path().join("out.txt");
                std::fs::write(&file, "done").unwrap();
                let run_id = unique_run_id("file-abs");
                create_run_in(&run_id, workdir.path());

                let (status, body) = get_file(&run_id, &file.to_string_lossy()).await;
                assert_eq!(status, StatusCode::OK);
                let got: FileContentResp = serde_json::from_slice(&body).unwrap();
                assert_eq!(got.content, "done");

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    /// `..` traversal and an unrelated absolute path both resolve outside the
    /// run's workdir, and both are refused before any read happens.
    #[tokio::test]
    async fn agent_file_refuses_a_path_outside_the_workdir() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_file_refuses_a_path_outside_the_workdir",
            |_d| async move {
                let workdir = tempfile::tempdir().unwrap();
                let run_id = unique_run_id("file-outside");
                create_run_in(&run_id, workdir.path());

                for outside in ["../escape.txt", "/etc/hosts"] {
                    let (status, body) = get_file(&run_id, outside).await;
                    assert_eq!(status, StatusCode::FORBIDDEN, "{outside}");
                    assert_eq!(
                        error_of(&body),
                        format!("path '{outside}' is outside the run's working directory")
                    );
                }

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    /// The containment is symlink-aware: a link planted under the workdir
    /// cannot be used to read outside it.
    #[cfg(unix)]
    #[tokio::test]
    async fn agent_file_is_not_fooled_by_a_symlink() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_file_is_not_fooled_by_a_symlink",
            |_d| async move {
                let workdir = tempfile::tempdir().unwrap();
                let outside = tempfile::tempdir().unwrap();
                std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
                std::os::unix::fs::symlink(outside.path(), workdir.path().join("escape")).unwrap();
                let run_id = unique_run_id("file-symlink");
                create_run_in(&run_id, workdir.path());

                let (status, _body) = get_file(&run_id, "escape/secret.txt").await;
                assert_eq!(status, StatusCode::FORBIDDEN);

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    #[tokio::test]
    async fn agent_file_missing_file_returns_404() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_file_missing_file_returns_404",
            |_d| async move {
                let workdir = tempfile::tempdir().unwrap();
                let run_id = unique_run_id("file-missing");
                create_run_in(&run_id, workdir.path());

                let (status, body) = get_file(&run_id, "no-such.md").await;
                assert_eq!(status, StatusCode::NOT_FOUND);
                assert_eq!(error_of(&body), "file 'no-such.md' not found");

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    /// A run whose workdir has since been deleted answers with a plain client
    /// error (the containment or the read refuses), never a 500.
    #[tokio::test]
    async fn agent_file_deleted_workdir_is_a_client_error() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_file_deleted_workdir_is_a_client_error",
            |_d| async move {
                let workdir = tempfile::tempdir().unwrap();
                let run_id = unique_run_id("file-gone-workdir");
                create_run_in(&run_id, workdir.path());
                drop(workdir); // the tempdir is removed here

                let (status, _body) = get_file(&run_id, "report.md").await;
                assert!(status.is_client_error(), "got {status}");

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    #[tokio::test]
    async fn agent_file_directory_returns_400() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_file_directory_returns_400",
            |_d| async move {
                let workdir = tempfile::tempdir().unwrap();
                std::fs::create_dir(workdir.path().join("sub")).unwrap();
                let run_id = unique_run_id("file-dir");
                create_run_in(&run_id, workdir.path());

                let (status, body) = get_file(&run_id, "sub").await;
                assert_eq!(status, StatusCode::BAD_REQUEST);
                assert_eq!(error_of(&body), "'sub' is a directory, not a file");

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    /// A file the server cannot open (here: no read permission) is reported,
    /// not a 500.
    #[cfg(unix)]
    #[tokio::test]
    async fn agent_file_unreadable_file_is_reported() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_file_unreadable_file_is_reported",
            |_d| async move {
                use std::os::unix::fs::PermissionsExt;
                let workdir = tempfile::tempdir().unwrap();
                let file = workdir.path().join("locked.txt");
                std::fs::write(&file, "sealed").unwrap();
                std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
                let run_id = unique_run_id("file-locked");
                create_run_in(&run_id, workdir.path());

                let (status, body) = get_file(&run_id, "locked.txt").await;
                // Root ignores mode bits; everywhere else the open fails.
                if status != StatusCode::OK {
                    assert_eq!(status, StatusCode::NOT_FOUND);
                    assert!(
                        error_of(&body).starts_with("could not read 'locked.txt'"),
                        "{}",
                        error_of(&body)
                    );
                }

                let _ = std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644));
                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    #[tokio::test]
    async fn agent_file_caps_the_read_at_one_mib() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_file_caps_the_read_at_one_mib",
            |_d| async move {
                let workdir = tempfile::tempdir().unwrap();
                let full = MAX_FILE_READ_BYTES as usize + 100;
                std::fs::write(workdir.path().join("big.log"), vec![b'a'; full]).unwrap();
                let run_id = unique_run_id("file-big");
                create_run_in(&run_id, workdir.path());

                let (status, body) = get_file(&run_id, "big.log").await;
                assert_eq!(status, StatusCode::OK);
                let got: FileContentResp = serde_json::from_slice(&body).unwrap();
                assert!(got.truncated);
                assert_eq!(got.size, full as u64);
                assert_eq!(got.content.len(), MAX_FILE_READ_BYTES as usize);

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    /// The cap landing mid-character does not make a text file "not text": the
    /// split character's leading bytes are dropped instead.
    #[tokio::test]
    async fn agent_file_truncation_mid_character_stays_text() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_file_truncation_mid_character_stays_text",
            |_d| async move {
                let workdir = tempfile::tempdir().unwrap();
                // 'a' up to one byte short of the cap, then a 3-byte '€'
                // straddling it, then more text past it.
                let mut bytes = vec![b'a'; MAX_FILE_READ_BYTES as usize - 1];
                bytes.extend_from_slice("€ and more".as_bytes());
                std::fs::write(workdir.path().join("split.md"), &bytes).unwrap();
                let run_id = unique_run_id("file-split");
                create_run_in(&run_id, workdir.path());

                let (status, body) = get_file(&run_id, "split.md").await;
                assert_eq!(status, StatusCode::OK);
                let got: FileContentResp = serde_json::from_slice(&body).unwrap();
                assert!(got.truncated);
                assert_eq!(got.content.len(), MAX_FILE_READ_BYTES as usize - 1);
                assert!(got.content.ends_with('a'));

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    #[tokio::test]
    async fn agent_file_binary_returns_415() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_file_binary_returns_415",
            |_d| async move {
                let workdir = tempfile::tempdir().unwrap();
                std::fs::write(workdir.path().join("blob.bin"), [0xff, 0xfe, 0x00, 0x01]).unwrap();
                // Invalid from early on AND larger than the cap: proves a big
                // binary is still called binary, not "truncated text".
                let mut big = vec![0xffu8; 16];
                big.resize(MAX_FILE_READ_BYTES as usize + 16, 0xff);
                std::fs::write(workdir.path().join("big.bin"), &big).unwrap();
                let run_id = unique_run_id("file-binary");
                create_run_in(&run_id, workdir.path());

                for name in ["blob.bin", "big.bin"] {
                    let (status, body) = get_file(&run_id, name).await;
                    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{name}");
                    assert_eq!(error_of(&body), format!("'{name}' is not a text file"));
                }

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    // ─── agent_result ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn agent_result_existing_run_no_stages() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_result_existing_run_no_stages",
            |_d| async move {
                let run_id = unique_run_id("result-no-stages");
                let mut meta = make_run(&run_id);
                meta.status = RunStatus::Complete;
                create_run(&meta).unwrap();

                // Write some output.log content
                let log_path = runstate::run_dir(&run_id).join("output.log");
                std::fs::write(&log_path, "task complete\n").unwrap();

                let app = Router::new()
                    .route("/api/agents/{id}/result", get(agent_result))
                    .with_state(test_state());
                let req = Request::builder()
                    .uri(format!("/api/agents/{}/result", run_id))
                    .body(Body::empty())
                    .unwrap();
                let resp = app.oneshot(req).await.unwrap();
                assert_eq!(resp.status(), axum::http::StatusCode::OK);
                let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let result: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(result["run_id"].as_str().unwrap(), run_id);
                assert_eq!(result["status"].as_str().unwrap(), "Complete");

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    #[tokio::test]
    async fn agent_result_existing_run_with_stages() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_result_existing_run_with_stages",
            |_d| async move {
                let run_id = unique_run_id("result-stages");
                let meta = make_run(&run_id);
                create_run(&meta).unwrap();

                // Write a stages index and stage output
                let stages = vec![runstate::StageRecord::new("plan".to_string(), 0)];
                runstate::write_stages_index(&run_id, &stages).unwrap();
                runstate::append_stage_output(&run_id, 0, "stage output here");

                let app = Router::new()
                    .route("/api/agents/{id}/result", get(agent_result))
                    .with_state(test_state());
                let req = Request::builder()
                    .uri(format!("/api/agents/{}/result", run_id))
                    .body(Body::empty())
                    .unwrap();
                let resp = app.oneshot(req).await.unwrap();
                assert_eq!(resp.status(), axum::http::StatusCode::OK);
                let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let result: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(result["run_id"].as_str().unwrap(), run_id);
                assert!(
                    result["output"]
                        .as_str()
                        .unwrap()
                        .contains("stage output here")
                );

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    #[tokio::test]
    async fn agent_result_nonexistent_run_returns_404() {
        let app = Router::new()
            .route("/api/agents/{id}/result", get(agent_result))
            .with_state(test_state());
        let req = Request::builder()
            .uri("/api/agents/nonexistent-run-xyz-result/result")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
    }

    // ─── kill_agent ───────────────────────────────────────────────────────────

    async fn delete_agent(control: ControlClient, id: &str) -> StatusCode {
        use axum::routing::delete;
        let (tx, _) = broadcast::channel(16);
        let state = AppState {
            config: Arc::new(Config::default()),
            event_tx: tx,
            control,
            mcp: crate::commands::serve::mcp::McpAdmin::default(),
            limits: Default::default(),
        };
        let app = Router::new()
            .route("/api/agents/{id}", delete(kill_agent))
            .with_state(state);
        let req = Request::builder()
            .method("DELETE")
            .uri(format!("/api/agents/{id}"))
            .body(Body::empty())
            .unwrap();
        app.oneshot(req).await.unwrap().status()
    }

    #[tokio::test]
    async fn kill_agent_cancels_via_daemon() {
        let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Ok { ok: true });
        assert_eq!(delete_agent(control, "run-a").await, StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn kill_agent_unknown_run_is_404() {
        let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Ok { ok: false });
        assert_eq!(delete_agent(control, "ghost").await, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn kill_agent_unexpected_response_is_500() {
        let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Spawned {
            run_id: "x".to_string(),
        });
        assert_eq!(
            delete_agent(control, "a").await,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[tokio::test]
    async fn kill_agent_daemon_absent_is_503() {
        assert_eq!(
            delete_agent(no_daemon(), "a").await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    // ─── pause_agent / resume_agent ──────────────────────────────────────────

    /// POST `/api/agents/{id}/pause` (or `/resume`) against a router holding
    /// `control`, returning the response status.
    async fn post_agent_action(control: ControlClient, id: &str, action: &str) -> StatusCode {
        use axum::routing::post;
        let (tx, _) = broadcast::channel(16);
        let state = AppState {
            config: Arc::new(Config::default()),
            event_tx: tx,
            control,
            mcp: crate::commands::serve::mcp::McpAdmin::default(),
            limits: Default::default(),
        };
        let app = Router::new()
            .route("/api/agents/{id}/pause", post(pause_agent))
            .route("/api/agents/{id}/resume", post(resume_agent))
            .with_state(state);
        let req = Request::builder()
            .method("POST")
            .uri(format!("/api/agents/{id}/{action}"))
            .body(Body::empty())
            .unwrap();
        app.oneshot(req).await.unwrap().status()
    }

    #[tokio::test]
    async fn pause_agent_sends_pause_to_the_daemon() {
        let (control, _dir, _srv) = fake_daemon(|req| {
            assert_eq!(
                std::mem::discriminant(&req),
                std::mem::discriminant(&ControlRequest::Pause {
                    run_id: String::new()
                })
            );
            ControlResponse::Ok { ok: true }
        });
        assert_eq!(
            post_agent_action(control, "run-a", "pause").await,
            StatusCode::NO_CONTENT
        );
    }

    #[tokio::test]
    async fn pause_agent_refused_is_404() {
        let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Ok { ok: false });
        assert_eq!(
            post_agent_action(control, "ghost", "pause").await,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn pause_agent_unexpected_response_is_500() {
        let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Spawned {
            run_id: "x".to_string(),
        });
        assert_eq!(
            post_agent_action(control, "a", "pause").await,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[tokio::test]
    async fn pause_agent_daemon_absent_is_503() {
        assert_eq!(
            post_agent_action(no_daemon(), "a", "pause").await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn resume_agent_sends_resume_to_the_daemon() {
        let (control, _dir, _srv) = fake_daemon(|req| {
            assert_eq!(
                std::mem::discriminant(&req),
                std::mem::discriminant(&ControlRequest::Resume {
                    run_id: String::new()
                })
            );
            ControlResponse::Ok { ok: true }
        });
        assert_eq!(
            post_agent_action(control, "run-a", "resume").await,
            StatusCode::NO_CONTENT
        );
    }

    #[tokio::test]
    async fn resume_agent_refused_is_404() {
        let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Ok { ok: false });
        assert_eq!(
            post_agent_action(control, "ghost", "resume").await,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn resume_agent_unexpected_response_is_500() {
        let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Spawned {
            run_id: "x".to_string(),
        });
        assert_eq!(
            post_agent_action(control, "a", "resume").await,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[tokio::test]
    async fn resume_agent_daemon_absent_is_503() {
        assert_eq!(
            post_agent_action(no_daemon(), "a", "resume").await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

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
        assert!(req.callback_secret.is_none());
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
            "callback_url": "https://example.com/hook",
            "callback_secret": "s3cret"
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
        assert_eq!(req.callback_secret.unwrap(), "s3cret");
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
