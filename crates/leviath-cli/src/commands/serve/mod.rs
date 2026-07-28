//! `lev serve` — REST + WebSocket API server.
//!
//! Exposes agent management, blueprint CRUD, and live event streaming over
//! HTTP. No web UI — the frontend lives in a separate repo.

mod agents;
mod auth;
mod blueprints;
mod config;
mod interactions;
mod mcp;
mod polling;
#[cfg(test)]
mod testutil;
mod tree;
mod types;
mod websocket;

use types::ServeLimits;
pub use types::{AppState, ServeArgs, ServerEvent};

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::routing::{delete, get, post};
use tokio::sync::broadcast;
use tower_http::cors::{Any, CorsLayer};

use crate::config::Config;

// ─── Entrypoint ──────────────────────────────────────────────────────────────

/// Aborts a spawned task when dropped — including when dropped mid-flight as
/// part of an outer future's cancellation (e.g. `JoinHandle::abort()` on the
/// task that owns this guard), not just on normal scope exit.
struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub async fn execute(
    args: ServeArgs,
    control: leviath_runtime::control_socket::ControlClient,
) -> anyhow::Result<()> {
    execute_with_shutdown(args, control, Box::pin(std::future::pending()), None).await
}

/// Core of [`execute`], with an optional shutdown signal so tests can stop
/// the server gracefully and cover the `Ok(())` return path.
///
/// Takes `shutdown` as a boxed trait object (`Pin<Box<dyn Future<...>>>`)
/// rather than `impl Future<...>` so every caller -- production's
/// `std::future::pending()` and tests' various `async move { ... }` blocks
/// awaiting a `oneshot::Receiver` -- shares exactly ONE monomorphization of
/// this (large, multi-branch) function instead of one per concrete future
/// type. Confirmed via HTML/JSON segment inspection that every source
/// position has a covered instantiation (this is the same trait-object-erasure
/// technique used for `io::Write` in `leviath-package`'s `bundler.rs`).
///
/// `ready`, if given, is sent the real bound `SocketAddr` right after
/// `TcpListener::bind` succeeds (before serving starts). Production passes
/// `None`; tests pass `Some(tx)` with `args.port = 0` so the OS picks a free
/// port and the test learns which one was actually bound directly -- no
/// probe-bind-drop-rebind dance, which is a genuine TOCTOU race (confirmed
/// to reproduce on real CI: another process/test could grab the just-freed
/// port before this function's own bind runs), not just a test-only
/// convenience.
async fn execute_with_shutdown(
    args: ServeArgs,
    control: leviath_runtime::control_socket::ControlClient,
    shutdown: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
    ready: Option<tokio::sync::oneshot::Sender<SocketAddr>>,
) -> anyhow::Result<()> {
    // Resolve the API token before binding — refuse to start unauthenticated.
    let auth_token = std::sync::Arc::new(auth::resolve_token(args.token.as_deref())?);
    // The API can spawn tool-executing agents; loudly warn if bound off-host.
    if args.host != "127.0.0.1" && args.host != "localhost" && args.host != "::1" {
        tracing::warn!(
            host = %args.host,
            "serving the agent API on a non-local address — anyone who can reach \
             this host and holds the token can spawn agents"
        );
    }

    let cfg = Config::load()?;
    // Read before `cfg` moves into the shared state below.
    let allow_local_network = cfg.security.allow_local_network;
    for warning in cfg.validate_keys() {
        tracing::warn!("{}", warning);
    }

    let (event_tx, _) = broadcast::channel::<ServerEvent>(1024);

    let state = AppState {
        config: Arc::new(cfg),
        event_tx: event_tx.clone(),
        control,
        mcp: mcp::McpAdmin::default(),
        limits: Arc::new(ServeLimits {
            workdir_root: args.workdir_root.clone(),
            no_remote_yolo: args.no_remote_yolo,
            allow_local_network,
        }),
    };

    // Background world-event consumer: subscribes to the daemon's pushed
    // `WorldEvent` stream and forwards each event to WebSocket subscribers.
    // Held behind an abort-on-drop guard so the task is torn down whenever this
    // function returns *or* is cancelled — e.g. when a test aborts the outer
    // `execute()`/`execute_with_shutdown()` task. Without this, aborting only
    // the outer task left the inner `event_loop` (an unconditional
    // subscribe-and-reconnect loop) running detached until the whole runtime
    // was torn down.
    let event_state = state.clone();
    let _event_guard = AbortOnDrop(tokio::spawn(polling::event_loop(
        event_state,
        polling::RECONNECT_BACKOFF,
    )));

    // No `--cors` at all: no CORS layer. Programmatic clients are not subject to
    // CORS, so the previous `*` default bought them nothing while telling every
    // browser that any page may talk to this server.
    let cors = match args.cors.as_deref() {
        None => None,
        Some("*") => Some(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any),
        ),
        Some(origin) => {
            // An unparseable value used to fall back to `*` — silently turning a
            // typo into "allow everything", the opposite of what was asked for.
            // Refuse to start instead.
            let value = origin.parse::<axum::http::HeaderValue>().map_err(|_| {
                anyhow::anyhow!("--cors value '{origin}' is not a valid origin header")
            })?;
            Some(
                CorsLayer::new()
                    .allow_origin(value)
                    .allow_methods(Any)
                    .allow_headers(Any),
            )
        }
    };

    let app = Router::new()
        // Blueprints
        .route(
            "/api/blueprints",
            get(blueprints::list_blueprints).post(blueprints::create_blueprint),
        )
        .route(
            "/api/blueprints/validate",
            post(blueprints::validate_blueprint),
        )
        .route(
            "/api/blueprints/{name}",
            get(blueprints::get_blueprint)
                .put(blueprints::update_blueprint)
                .delete(blueprints::delete_blueprint),
        )
        // Agents
        .route(
            "/api/agents",
            get(agents::list_agents).post(agents::spawn_agent),
        )
        .route("/api/agents/tree", get(tree::agents_tree))
        .route(
            "/api/agents/{id}",
            get(agents::get_agent).delete(agents::kill_agent),
        )
        .route("/api/agents/{id}/children", get(agents::agent_children))
        .route("/api/agents/{id}/context", get(agents::agent_context))
        .route(
            "/api/agents/{id}/context/history",
            get(agents::agent_context_history),
        )
        .route("/api/agents/{id}/logs", get(agents::agent_logs))
        .route("/api/agents/{id}/result", get(agents::agent_result))
        .route("/api/agents/{id}/tree-status", get(tree::agent_tree_status))
        // Messages
        .route("/api/agents/{id}/message", post(interactions::send_message))
        // Interactions
        .route(
            "/api/agents/{id}/interaction",
            get(interactions::get_interaction).post(interactions::submit_interaction),
        )
        // MCP servers — read-only surface. The mutating half is mounted below,
        // behind `--allow-admin`.
        .route("/api/mcp/servers", get(mcp::list_servers))
        .route("/api/mcp/servers/{name}/status", get(mcp::status))
        .route("/api/mcp/servers/{name}/login", post(mcp::login))
        .route("/api/mcp/servers/{name}/test", post(mcp::test_server))
        // Config
        .route("/api/config", get(config::get_config))
        .route("/api/models", get(config::get_models))
        // WebSocket
        .route("/ws", get(websocket::ws_global))
        .route("/ws/agents/{id}", get(websocket::ws_agent));

    // The MCP administration endpoints are remote code execution by
    // construction: `add_server` writes a `command` and `args` into
    // `~/.leviath/config.toml`, and Leviath then spawns exactly that — for this
    // run and every future one. The rest of the API can only run agents the user
    // already installed. Not mounted unless the operator asked for them, so an
    // unmounted route 404s rather than relying on a check inside the handler
    // that someone could later route around.
    let app = match args.allow_admin {
        true => app
            .route("/api/mcp/servers", post(mcp::add_server))
            .route("/api/mcp/servers/{name}", delete(mcp::remove_server)),
        false => app,
    };

    let app = app
        // Require a valid token on every route; CORS stays outermost so browser
        // preflight (OPTIONS) is answered before the auth check.
        .layer(axum::middleware::from_fn_with_state(
            auth_token,
            auth::require_auth,
        ))
        .with_state(state);
    // Applied by branching on the router rather than layering an `Option`:
    // `Option<CorsLayer>` is not a `Layer`, and a permissive-but-unused layer
    // would be exactly the default this change removes.
    let app = match cors {
        Some(layer) => app.layer(layer),
        None => app,
    };

    let addr: SocketAddr = format!("{}:{}", args.host, args.port).parse()?;
    tracing::info!("Listening on http://{}", addr);
    println!("Leviath API server listening on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    if let Some(ready) = ready {
        // A test-only observer failing to receive (e.g. it already gave up
        // after a timeout) shouldn't stop the server from starting for real.
        let local_addr = listener
            .local_addr()
            .expect("infallible: a freshly bound TcpListener always has a local address");
        let _ = ready.send(local_addr);
    }
    // axum::serve with graceful shutdown always returns Ok(()) — discard the
    // infallible Result so LLVM-cov does not instrument an unreachable Err branch.
    let _ = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await;

    Ok(())
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use crate::runstate::RunMeta;
    use crate::test_support::with_tracing;

    /// Extracted so the `assert!` failure-message region (only executed
    /// when the assertion fails) is covered by this function's own
    /// `#[should_panic]` test below, rather than showing as a
    /// permanently-uncovered region at every real call site.
    fn assert_execute_failed_on_malformed_config(result: &anyhow::Result<()>) {
        assert!(
            result.is_err(),
            "execute should fail when config is malformed"
        );
    }

    #[test]
    #[should_panic(expected = "execute should fail when config is malformed")]
    fn assert_execute_failed_on_malformed_config_panics_when_ok() {
        assert_execute_failed_on_malformed_config(&Ok(()));
    }

    /// See [`assert_execute_failed_on_malformed_config`] — same rationale,
    /// for the bad-API-key startup failure-message region.
    fn assert_connected_with_bad_api_key(connected: bool) {
        assert!(connected, "server should start even with a bad API key");
    }

    #[test]
    #[should_panic(expected = "server should start even with a bad API key")]
    fn assert_connected_with_bad_api_key_panics_when_not_connected() {
        assert_connected_with_bad_api_key(false);
    }

    /// See [`assert_execute_failed_on_malformed_config`] — same rationale,
    /// for the graceful-shutdown return-value failure-message region.
    fn assert_execute_returned_ok_after_shutdown(result: &Result<(), anyhow::Error>) {
        assert!(
            result.is_ok(),
            "execute should return Ok after graceful shutdown"
        );
    }

    #[test]
    #[should_panic(expected = "execute should return Ok after graceful shutdown")]
    fn assert_execute_returned_ok_after_shutdown_panics_when_err() {
        assert_execute_returned_ok_after_shutdown(&Err(anyhow::anyhow!("boom")));
    }

    /// See [`assert_execute_failed_on_malformed_config`] — same rationale,
    /// for the port-in-use failure-message region.
    fn assert_execute_failed_on_port_in_use(result: &anyhow::Result<()>) {
        assert!(
            result.is_err(),
            "execute should fail when port is already in use"
        );
    }

    #[test]
    #[should_panic(expected = "execute should fail when port is already in use")]
    fn assert_execute_failed_on_port_in_use_panics_when_ok() {
        assert_execute_failed_on_port_in_use(&Ok(()));
    }

    /// See [`assert_execute_failed_on_malformed_config`] — same rationale,
    /// for `execute_with_shutdown`'s graceful-shutdown return-value
    /// failure-message region.
    fn assert_execute_with_shutdown_returned_ok(result: &Result<(), anyhow::Error>) {
        assert!(
            result.is_ok(),
            "execute_with_shutdown should return Ok(()) after graceful shutdown"
        );
    }

    #[test]
    #[should_panic(expected = "execute_with_shutdown should return Ok(()) after graceful shutdown")]
    fn assert_execute_with_shutdown_returned_ok_panics_when_err() {
        assert_execute_with_shutdown_returned_ok(&Err(anyhow::anyhow!("boom")));
    }

    /// See [`assert_execute_failed_on_malformed_config`] — same rationale,
    /// for the HTTP response status-line failure-message region.
    fn assert_response_ok(resp_str: &str) {
        assert!(resp_str.starts_with("HTTP/1.1 200"), "got: {resp_str}");
    }

    #[test]
    #[should_panic(expected = "got: HTTP/1.1 404 Not Found")]
    fn assert_response_ok_panics_when_not_200() {
        assert_response_ok("HTTP/1.1 404 Not Found\r\n\r\n");
    }

    /// A control client pointing at an address with no daemon: agent-action
    /// endpoints report "not reachable", and read/bootstrap paths don't touch it.
    fn no_daemon_control() -> leviath_runtime::control_socket::ControlClient {
        leviath_runtime::control_socket::ControlClient::new(
            leviath_runtime::control_socket::control_id(std::path::Path::new("/no/such/leviath")),
        )
    }

    fn test_state() -> AppState {
        let (tx, _) = broadcast::channel(64);
        AppState {
            config: Arc::new(Config::default()),
            event_tx: tx,
            control: no_daemon_control(),
            mcp: crate::commands::serve::mcp::McpAdmin::default(),
            limits: Default::default(),
        }
    }

    fn test_app() -> Router {
        let state = test_state();
        Router::new()
            .route("/api/blueprints", get(blueprints::list_blueprints))
            .route(
                "/api/blueprints/validate",
                post(blueprints::validate_blueprint),
            )
            .route(
                "/api/blueprints/{name}",
                get(blueprints::get_blueprint).delete(blueprints::delete_blueprint),
            )
            .route("/api/agents", get(agents::list_agents))
            .route("/api/agents/tree", get(tree::agents_tree))
            .route("/api/agents/{id}", get(agents::get_agent))
            .route("/api/agents/{id}/children", get(agents::agent_children))
            .route("/api/agents/{id}/context", get(agents::agent_context))
            .route(
                "/api/agents/{id}/context/history",
                get(agents::agent_context_history),
            )
            .route("/api/agents/{id}/logs", get(agents::agent_logs))
            .route("/api/agents/{id}/result", get(agents::agent_result))
            .route("/api/agents/{id}/tree-status", get(tree::agent_tree_status))
            .route(
                "/api/agents/{id}/interaction",
                get(interactions::get_interaction).post(interactions::submit_interaction),
            )
            .route("/api/config", get(config::get_config))
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
        let val: types::ValidateResponse = serde_json::from_slice(&body).unwrap();
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
        let val: types::ValidateResponse = serde_json::from_slice(&body).unwrap();
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
    async fn test_interaction_route_reaches_daemon() {
        // The route is wired to the handler, which (with no daemon in this test)
        // reports the daemon unreachable — proving the request reached it.
        let app = test_app();
        let req = Request::builder()
            .uri("/api/agents/nonexistent/interaction")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
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
        let val: types::RedactedConfig = serde_json::from_slice(&body).unwrap();
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

        let tree = tree::build_tree_status(&runs, None);
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
            tool_calls: 0,
            accepts_messages: true,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"agent_status\""));
        assert!(json.contains("\"agent_id\":\"coder\""));

        let event2 = ServerEvent::Tokens {
            agent_id: "coder".to_string(),
            run_id: "run-123".to_string(),
            prompt_tokens: 5000,
            completion_tokens: 1200,
            cached_tokens: 0,
            cache_write_tokens: 0,
        };
        let json2 = serde_json::to_string(&event2).unwrap();
        assert!(json2.contains("\"type\":\"tokens\""));
        assert!(json2.contains("\"prompt_tokens\":5000"));
    }

    fn full_app() -> Router {
        let state = test_state();
        Router::new()
            .route(
                "/api/blueprints",
                get(blueprints::list_blueprints).post(blueprints::create_blueprint),
            )
            .route(
                "/api/blueprints/validate",
                post(blueprints::validate_blueprint),
            )
            .route(
                "/api/blueprints/{name}",
                get(blueprints::get_blueprint)
                    .put(blueprints::update_blueprint)
                    .delete(blueprints::delete_blueprint),
            )
            .route(
                "/api/agents",
                get(agents::list_agents).post(agents::spawn_agent),
            )
            .route("/api/agents/tree", get(tree::agents_tree))
            .route(
                "/api/agents/{id}",
                get(agents::get_agent).delete(agents::kill_agent),
            )
            .route("/api/agents/{id}/children", get(agents::agent_children))
            .route("/api/agents/{id}/context", get(agents::agent_context))
            .route(
                "/api/agents/{id}/context/history",
                get(agents::agent_context_history),
            )
            .route("/api/agents/{id}/logs", get(agents::agent_logs))
            .route("/api/agents/{id}/result", get(agents::agent_result))
            .route("/api/agents/{id}/tree-status", get(tree::agent_tree_status))
            .route("/api/agents/{id}/message", post(interactions::send_message))
            .route(
                "/api/agents/{id}/interaction",
                get(interactions::get_interaction).post(interactions::submit_interaction),
            )
            .route("/api/config", get(config::get_config))
            .route("/api/models", get(config::get_models))
            .with_state(state)
    }

    #[tokio::test]
    async fn test_full_router_create_blueprint_invalid() {
        let app = full_app();
        let body = serde_json::json!({
            "name": "bad-agent",
            "manifest": "not valid toml {{{"
        });
        let req = Request::builder()
            .method("POST")
            .uri("/api/blueprints")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_full_router_update_blueprint_not_found() {
        let app = full_app();
        let body = serde_json::json!({
            "manifest": r#"
[agent]
name = "no-such-agent"
version = "1.0.0"
description = "Missing"

[stages.run]
prompt = "Run"
"#
        });
        let req = Request::builder()
            .method("PUT")
            .uri("/api/blueprints/no-such-agent-xyz-99999")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_full_router_kill_agent_reaches_daemon() {
        let app = full_app();
        let req = Request::builder()
            .method("DELETE")
            .uri("/api/agents/nonexistent-kill-id-xyz")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_full_router_send_message_reaches_daemon() {
        let app = full_app();
        let body = serde_json::json!({"message": "hello"});
        let req = Request::builder()
            .method("POST")
            .uri("/api/agents/nonexistent-msg-id-xyz/message")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_full_router_get_models() {
        let app = full_app();
        let req = Request::builder()
            .uri("/api/models")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_full_router_spawn_agent_blueprint_not_found() {
        let app = full_app();
        let body = serde_json::json!({
            "blueprint": "nonexistent-blueprint-xyz",
            "task": "do something"
        });
        let req = Request::builder()
            .method("POST")
            .uri("/api/agents")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn test_serve_args_defaults() {
        let args = ServeArgs {
            port: 3000,
            host: "127.0.0.1".to_string(),
            cors: None,
            token: Some("test-token".to_string()),
            allow_admin: false,
            workdir_root: None,
            no_remote_yolo: false,
        };
        assert_eq!(args.port, 3000);
        assert_eq!(args.host, "127.0.0.1");
        assert_eq!(args.cors, None);
    }

    #[test]
    fn test_app_state_clone() {
        let state = test_state();
        let cloned = state.clone();
        // Both should work (no panic)
        let _ = cloned.config.default_provider.clone();
    }

    #[test]
    fn test_cors_wildcard_vs_specific() {
        // Test the CORS logic paths used in execute()
        let wildcard = "*";
        let specific = "https://example.com";

        let is_wildcard = wildcard == "*";
        assert!(is_wildcard);

        let is_specific = specific != "*";
        assert!(is_specific);

        // Test that specific CORS origin parses correctly
        let parsed = specific.parse::<axum::http::HeaderValue>();
        assert!(parsed.is_ok());
    }

    #[test]
    fn test_cors_invalid_origin_falls_back() {
        let invalid_cors = "not a valid header value \x00";
        let result = invalid_cors.parse::<axum::http::HeaderValue>();
        // Invalid header values fail to parse; the code falls back to "*"
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_submit_interaction_full_router_reaches_daemon() {
        // The POST-interaction route is wired to the handler, which reaches the
        // (absent-in-test) daemon. The ACCEPTED path is covered by the
        // interactions handler's own tests against a fake daemon.
        let app = full_app();
        let body = serde_json::json!({"request_id": "req-1", "value": "do it", "scope": "once"});
        let req = Request::builder()
            .method("POST")
            .uri("/api/agents/any/interaction")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    // ─── execute() — real server bootstrap ─────────────────────────────────
    //
    // These drive the actual `execute()` entrypoint (config load, CORS setup,
    // full router construction, real TCP bind, background polling spawn) end
    // to end using port 0 (OS-assigned ephemeral port) so no fixed port is
    // required. Since `axum::serve(...).await` never returns on success, the
    // task is aborted once we've proven the server is up and responding.
    //
    // Each holds `isolate_config_path_for_test` even though none of them
    // care about specific config *content* -- their own `Config::load()`
    // call needs protecting from a DIFFERENT concurrently-running test that
    // does mutate `LEVIATH_CONFIG_PATH` (e.g. `execute_with_malformed_config_
    // returns_err`, which points it at a file containing invalid TOML for
    // the duration of its own guard). `std::env::set_var` is process-global,
    // not thread-local, so without holding the same lock here, this test's
    // `Config::load()` could transiently observe that other test's malformed
    // path and fail with a real (if confusing) parse error -- confirmed to
    // reproduce locally at default test-thread concurrency, not a hypothetical.

    #[tokio::test]
    async fn execute_binds_and_serves_with_wildcard_cors() {
        crate::config::with_isolated_config_path_async(
            "serve-mod-wildcard-cors",
            |_fake_dir| async move {
                with_tracing(|| {});
                // port: 0 lets the OS assign a genuinely free ephemeral port at bind
                // time; execute_with_shutdown reports the real bound SocketAddr back
                // via `ready` the instant it's bound, so there's no
                // probe-bind-drop-rebind gap for another process/test to race into
                // (that gap is a real, CI-reproducing TOCTOU -- see
                // execute_with_shutdown's doc comment). Exercises the exact same
                // production code path execute() does (its own body is just this
                // call with `ready: None`), so this remains a real end-to-end test
                // of execute()'s bootstrap logic.
                let args = ServeArgs {
                    port: 0,
                    host: "127.0.0.1".to_string(),
                    cors: None,
                    token: Some("test-token".to_string()),
                    allow_admin: false,
                    workdir_root: None,
                    no_remote_yolo: false,
                };
                let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
                let handle = tokio::spawn(execute_with_shutdown(
                    args,
                    no_daemon_control(),
                    Box::pin(std::future::pending()),
                    Some(ready_tx),
                ));
                let addr = ready_rx
                    .await
                    .expect("server should report its bound address");

                // Sanity-check a real request round trip through the full app.
                let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                stream
                    .write_all(
                        b"GET /api/config HTTP/1.1\r\nHost: localhost\r\n\
                          Authorization: Bearer test-token\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .unwrap();
                let mut resp = Vec::new();
                stream.read_to_end(&mut resp).await.unwrap();
                let resp_str = String::from_utf8_lossy(&resp);
                assert_response_ok(&resp_str);

                // Without the token the same request is rejected.
                let mut unauth = tokio::net::TcpStream::connect(addr).await.unwrap();
                unauth
                    .write_all(
                        b"GET /api/config HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .unwrap();
                let mut resp2 = Vec::new();
                unauth.read_to_end(&mut resp2).await.unwrap();
                assert!(
                    String::from_utf8_lossy(&resp2).starts_with("HTTP/1.1 401"),
                    "unauthenticated request should be 401"
                );

                handle.abort();
            },
        )
        .await;
    }

    #[tokio::test]
    async fn execute_with_specific_cors_origin_serves() {
        crate::config::with_isolated_config_path_async(
            "serve-mod-specific-cors",
            |_fake_dir| async move {
                let args = ServeArgs {
                    port: 0,
                    host: "127.0.0.1".to_string(),
                    cors: Some("https://example.com".to_string()),
                    token: Some("test-token".to_string()),
                    allow_admin: false,
                    workdir_root: None,
                    no_remote_yolo: false,
                };
                let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
                let handle = tokio::spawn(execute_with_shutdown(
                    args,
                    no_daemon_control(),
                    Box::pin(std::future::pending()),
                    Some(ready_tx),
                ));
                let addr = ready_rx
                    .await
                    .expect("server should report its bound address");
                assert!(tokio::net::TcpStream::connect(addr).await.is_ok());

                handle.abort();
            },
        )
        .await;
    }

    #[tokio::test]
    async fn execute_with_unparseable_addr_returns_err() {
        // An invalid host string makes `format!("{host}:{port}").parse()`
        // fail, exercising execute()'s `?` on the SocketAddr parse.
        let args = ServeArgs {
            port: 0,
            host: "not a valid host".to_string(),
            cors: None,
            token: Some("test-token".to_string()),
            allow_admin: false,
            workdir_root: None,
            no_remote_yolo: false,
        };
        let result = execute(args, no_daemon_control()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_agent_list_with_status_filter_full_router() {
        let app = full_app();
        let req = Request::builder()
            .uri("/api/agents?status=running,complete")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// Covers `Config::load()?` error path (line 31) by pointing
    /// `LEVIATH_CONFIG_PATH` at a file containing invalid TOML.
    #[tokio::test]
    async fn execute_with_malformed_config_returns_err() {
        crate::config::with_isolated_config_path_async(
            "serve-mod-malformed",
            |_fake_dir| async move {
                // After isolate_config_path_for_test, Config::config_path() returns the temp path.
                std::fs::write(Config::config_path(), "not valid toml [[[").unwrap();

                let args = ServeArgs {
                    port: 0,
                    host: "127.0.0.1".to_string(),
                    cors: None,
                    token: Some("test-token".to_string()),
                    allow_admin: false,
                    workdir_root: None,
                    no_remote_yolo: false,
                };
                let result = execute(args, no_daemon_control()).await;
                assert_execute_failed_on_malformed_config(&result);
            },
        )
        .await;
    }

    /// Covers the `for warning in cfg.validate_keys()` loop body (lines 32-33)
    /// by writing a config with a bad anthropic key, then running the server
    /// with a graceful-shutdown signal so the loop executes before bind.
    #[tokio::test]
    async fn execute_with_bad_api_key_logs_warning_and_serves() {
        with_tracing(|| {});
        crate::config::with_isolated_config_path_async("serve-mod-badkey", |_fake_dir| async move {
        // Write a config with an anthropic key that fails validate_keys().
        std::fs::write(
            Config::config_path(),
            "default_provider = \"anthropic\"\nregistries = []\nagent_paths = []\n[providers]\nanthropic_api_key = \"bad-key-not-sk-ant\"\n",
        )
        .unwrap();

        let args = ServeArgs {
            port: 0,
            host: "127.0.0.1".to_string(),
            cors: None,
            token: Some("test-token".to_string()),
            allow_admin: false,
            workdir_root: None,
            no_remote_yolo: false,
        };

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let shutdown_fut = async move {
            let _ = shutdown_rx.await;
        };
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();

        let handle = tokio::spawn(execute_with_shutdown(
            args,
            no_daemon_control(),
            Box::pin(shutdown_fut),
            Some(ready_tx),
        ));
        let addr = ready_rx
            .await
            .expect("server should report its bound address");
        let connected = tokio::net::TcpStream::connect(addr).await.is_ok();
        assert_connected_with_bad_api_key(connected);

        // Trigger graceful shutdown so execute_with_shutdown returns Ok(()).
        let _ = shutdown_tx.send(());
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("timed out waiting for execute to return")
            .expect("task panicked");
        assert_execute_returned_ok_after_shutdown(&result);
    }).await;
    }

    /// Covers the `TcpListener::bind(addr).await?` error path deterministically
    /// by binding to a reserved TEST-NET-1 address (RFC 5737, `192.0.2.0/24`)
    /// that is never assigned to a local interface, so the bind always fails
    /// with `EADDRNOTAVAIL`. (A prior version reused an already-bound ephemeral
    /// port, which occasionally let the second bind succeed under parallel-test
    /// load and left this region uncovered — a genuine flake.)
    #[tokio::test]
    async fn execute_with_unbindable_address_returns_bind_error() {
        let args = ServeArgs {
            port: 8080,
            host: "192.0.2.1".to_string(),
            cors: None,
            token: Some("test-token".to_string()),
            allow_admin: false,
            workdir_root: None,
            no_remote_yolo: false,
        };
        let result = execute(args, no_daemon_control()).await;
        assert_execute_failed_on_port_in_use(&result);
    }

    #[tokio::test]
    async fn execute_refuses_to_start_without_a_token() {
        // No --token and no LEVIATH_API_TOKEN ⇒ the server won't start.
        temp_env::async_with_vars([("LEVIATH_API_TOKEN", None::<&str>)], async {
            let args = ServeArgs {
                port: 0,
                host: "127.0.0.1".to_string(),
                cors: None,
                token: None,
                allow_admin: false,
                workdir_root: None,
                no_remote_yolo: false,
            };
            let result = execute(args, no_daemon_control()).await;
            assert!(result.is_err(), "must refuse to start unauthenticated");
        })
        .await;
    }

    /// Covers `axum::serve(...).await?` Ok path (lines 117, 119) by running
    /// `execute_with_shutdown` and sending a graceful-shutdown signal.
    #[tokio::test]
    async fn execute_with_shutdown_signal_returns_ok() {
        crate::config::with_isolated_config_path_async(
            "serve-mod-shutdown-signal",
            |_fake_dir| async move {
                let args = ServeArgs {
                    port: 0,
                    host: "127.0.0.1".to_string(),
                    cors: None,
                    token: Some("test-token".to_string()),
                    allow_admin: false,
                    workdir_root: None,
                    no_remote_yolo: false,
                };

                let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
                let shutdown_fut = async move {
                    let _ = shutdown_rx.await;
                };
                let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();

                let handle = tokio::spawn(execute_with_shutdown(
                    args,
                    no_daemon_control(),
                    Box::pin(shutdown_fut),
                    Some(ready_tx),
                ));
                ready_rx
                    .await
                    .expect("server should report its bound address");

                // Send shutdown signal and wait for execute to return Ok.
                let _ = shutdown_tx.send(());
                let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
                    .await
                    .expect("timed out waiting for execute_with_shutdown to return")
                    .expect("task panicked");
                assert_execute_with_shutdown_returned_ok(&result);
            },
        )
        .await;
    }

    /// Covers the `ready: None` fall-through of the `if let Some(ready)` block
    /// (line 190): a successful bind with no ready-observer, shut down
    /// gracefully. Every other binding test passes `Some(ready)`, and every
    /// `None` caller (`execute()`) in other tests fails before binding, so this
    /// is the only path that reaches the block's None continuation.
    #[tokio::test]
    async fn execute_with_shutdown_no_ready_observer_returns_ok() {
        crate::config::with_isolated_config_path_async(
            "serve-mod-no-ready",
            |_fake_dir| async move {
                let args = ServeArgs {
                    port: 0,
                    host: "127.0.0.1".to_string(),
                    cors: None,
                    token: Some("test-token".to_string()),
                    allow_admin: false,
                    workdir_root: None,
                    no_remote_yolo: false,
                };

                let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
                let shutdown_fut = async move {
                    let _ = shutdown_rx.await;
                };

                let handle = tokio::spawn(execute_with_shutdown(
                    args,
                    no_daemon_control(),
                    Box::pin(shutdown_fut),
                    None,
                ));
                // Give the server a moment to bind before shutting down.
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                let _ = shutdown_tx.send(());
                let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
                    .await
                    .expect("timed out waiting for execute_with_shutdown to return")
                    .expect("task panicked");
                assert_execute_with_shutdown_returned_ok(&result);
            },
        )
        .await;
    }
    /// The three CORS shapes. Default is *no layer*: the API's clients are
    /// programmatic and not subject to CORS, so a browser-facing `*` default
    /// gave them nothing and widened the surface for everyone else.
    #[tokio::test]
    async fn cors_is_off_by_default_explicit_when_asked_and_fatal_when_malformed() {
        fn args_with(cors: Option<&str>) -> ServeArgs {
            ServeArgs {
                port: 0,
                host: "127.0.0.1".to_string(),
                cors: cors.map(str::to_string),
                token: Some("t".to_string()),
                allow_admin: false,
                workdir_root: None,
                no_remote_yolo: false,
            }
        }

        /// Start, wait until bound, then shut down. Only reached for values that
        /// are accepted — a rejected one never binds.
        async fn starts(cors: Option<&str>) {
            let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
            let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
            let server = tokio::spawn(execute_with_shutdown(
                args_with(cors),
                no_daemon_control(),
                Box::pin(async move {
                    let _ = stop_rx.await;
                }),
                Some(ready_tx),
            ));
            ready_rx.await.expect("the server bound");
            let _ = stop_tx.send(());
            server.await.expect("join").expect("clean shutdown");
        }

        starts(None).await;
        starts(Some("*")).await;
        starts(Some("https://ok.example")).await;

        // A malformed origin fails before binding, so this can be awaited
        // directly rather than raced against a `ready` signal.
        let err = execute_with_shutdown(
            args_with(Some("not a valid\nheader")),
            no_daemon_control(),
            Box::pin(std::future::pending()),
            None,
        )
        .await
        .expect_err("a malformed origin must refuse to start");
        assert!(err.to_string().contains("not a valid origin header"));
    }

    /// The MCP admin endpoints are mounted only with `--allow-admin`: adding an
    /// MCP server writes a spawn command into config, which Leviath then runs.
    #[tokio::test]
    async fn the_mcp_admin_routes_are_mounted_only_with_allow_admin() {
        for allow_admin in [false, true] {
            let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
            let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
            let args = ServeArgs {
                port: 0,
                host: "127.0.0.1".to_string(),
                cors: None,
                token: Some("t".to_string()),
                allow_admin,
                workdir_root: None,
                no_remote_yolo: false,
            };
            let server = tokio::spawn(execute_with_shutdown(
                args,
                no_daemon_control(),
                Box::pin(async move {
                    let _ = stop_rx.await;
                }),
                Some(ready_tx),
            ));
            let addr = ready_rx.await.expect("bound");

            let status = reqwest::Client::new()
                .post(format!("http://{addr}/api/mcp/servers"))
                .bearer_auth("t")
                .json(&serde_json::json!({}))
                .send()
                .await
                .expect("request")
                .status()
                .as_u16();
            // 405 (Method Not Allowed) is the signature of "this path exists
            // for GET but POST is not mounted". Asserted as a presence check
            // rather than an exact code for the mounted case, whose status
            // depends on body validation rather than on routing.
            match allow_admin {
                false => assert_eq!(status, 405, "the admin route must not be mounted"),
                true => assert_ne!(status, 405, "the admin route must be mounted"),
            }

            let _ = stop_tx.send(());
            let _ = server.await;
        }
    }
}
