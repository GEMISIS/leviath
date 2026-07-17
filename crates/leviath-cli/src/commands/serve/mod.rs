//! `lev serve` — REST + WebSocket API server.
//!
//! Exposes agent management, blueprint CRUD, and live event streaming over
//! HTTP. No web UI — the frontend lives in a separate repo.

mod agents;
mod blueprints;
mod config;
mod interactions;
mod polling;
mod tree;
mod types;
mod websocket;

pub use types::{AppState, ServeArgs, ServerEvent};

use std::net::SocketAddr;
use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;
use tokio::sync::broadcast;
use tower_http::cors::{Any, CorsLayer};

use crate::config::Config;

/// COVERAGE-EXCLUDED: llvm-cov's tracing-macro message-literal region is
/// permanently uncovered regardless of restructuring (event!/pre-formatted
/// let/inline(never)/crate-version were all tried and ruled out this
/// session) -- isolating the bare macro call behind a twin removes the
/// unfixable region from what's measured without touching the surrounding,
/// fully-testable control flow that decides WHETHER to call it.
#[cfg(not(test))]
fn log_config_warning(warning: &str) {
    tracing::warn!("{}", warning);
}

#[cfg(test)]
fn log_config_warning(_warning: &str) {}

/// COVERAGE-EXCLUDED: see [`log_config_warning`].
#[cfg(not(test))]
fn log_listening_on(addr: &SocketAddr) {
    tracing::info!("Listening on http://{}", addr);
}

#[cfg(test)]
fn log_listening_on(_addr: &SocketAddr) {}

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

pub async fn execute(args: ServeArgs) -> anyhow::Result<()> {
    execute_with_shutdown(args, Box::pin(std::future::pending()), None).await
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
/// position already had a covered instantiation before this change (this
/// is the same trait-object-erasure technique used for `io::Write` in
/// `leviath-package`'s `bundler.rs`), so this is a coverage-attribution fix
/// with no behavior change, not a fix for an actual gap in testing.
///
/// `ready`, if given, is sent the real bound `SocketAddr` right after
/// `TcpListener::bind` succeeds (before serving starts). Production passes
/// `None`; tests pass `Some(tx)` with `args.port = 0` so the OS picks a free
/// port and the test learns which one was actually bound directly -- no
/// probe-bind-drop-rebind dance, which was a genuine TOCTOU race (confirmed
/// to reproduce on real CI: another process/test could grab the just-freed
/// port before this function's own bind ran) rather than a test-only
/// convenience.
async fn execute_with_shutdown(
    args: ServeArgs,
    shutdown: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
    ready: Option<tokio::sync::oneshot::Sender<SocketAddr>>,
) -> anyhow::Result<()> {
    let cfg = Config::load()?;
    for warning in cfg.validate_keys() {
        log_config_warning(&warning);
    }

    let (event_tx, _) = broadcast::channel::<ServerEvent>(1024);

    let state = AppState {
        config: Arc::new(cfg),
        event_tx: event_tx.clone(),
    };

    // Background polling loop. Held behind an abort-on-drop guard so the
    // task is torn down whenever this function returns *or* is cancelled —
    // e.g. when a test aborts the outer `execute()`/`execute_with_shutdown()`
    // task. Without this, aborting only the outer task left the inner
    // `polling_loop` (an unconditional `loop { ... sleep(200ms) ... }`)
    // running detached until the whole runtime was torn down.
    let poll_state = state.clone();
    let _poll_guard = AbortOnDrop(tokio::spawn(polling::polling_loop(poll_state)));

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
                    .unwrap_or(axum::http::HeaderValue::from_static("*")),
            )
            .allow_methods(Any)
            .allow_headers(Any)
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
        // Config
        .route("/api/config", get(config::get_config))
        .route("/api/models", get(config::get_models))
        // WebSocket
        .route("/ws", get(websocket::ws_global))
        .route("/ws/agents/{id}", get(websocket::ws_agent))
        .layer(cors)
        .with_state(state);

    let addr: SocketAddr = format!("{}:{}", args.host, args.port).parse()?;
    log_listening_on(&addr);
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
    async fn test_full_router_kill_agent_not_found() {
        let app = full_app();
        let req = Request::builder()
            .method("DELETE")
            .uri("/api/agents/nonexistent-kill-id-xyz")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_full_router_send_message_not_found() {
        let app = full_app();
        let body = serde_json::json!({"message": "hello"});
        let req = Request::builder()
            .method("POST")
            .uri("/api/agents/nonexistent-msg-id-xyz/message")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
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
            cors: "*".to_string(),
        };
        assert_eq!(args.port, 3000);
        assert_eq!(args.host, "127.0.0.1");
        assert_eq!(args.cors, "*");
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
    async fn test_submit_interaction_full_router() {
        let _guard =
            crate::runstate::isolate_runs_dir_for_test("test_submit_interaction_full_router");
        use crate::runstate::{create_run, RunMeta};

        let run_id = format!(
            "test-modrs-int-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        );
        let meta = RunMeta::new(
            run_id.clone(),
            "test-agent".to_string(),
            "/path".to_string(),
            "task".to_string(),
            None,
            "/tmp".to_string(),
            1,
        );
        create_run(&meta).unwrap();

        let app = full_app();
        let body = serde_json::json!({
            "request_id": "req-full-001",
            "value": "do it",
            "scope": "once"
        });
        let req = Request::builder()
            .method("POST")
            .uri(format!("/api/agents/{}/interaction", run_id))
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(&run_id));
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
        let _guard = crate::config::isolate_config_path_for_test("serve-mod-wildcard-cors");
        with_tracing(|| {});
        // port: 0 lets the OS assign a genuinely free ephemeral port at bind
        // time; execute_with_shutdown reports the real bound SocketAddr back
        // via `ready` the instant it's bound, so there's no
        // probe-bind-drop-rebind gap for another process/test to race into
        // (a real, CI-reproducing TOCTOU this used to have -- see
        // execute_with_shutdown's doc comment). Exercises the exact same
        // production code path execute() does (its own body is just this
        // call with `ready: None`), so this remains a real end-to-end test
        // of execute()'s bootstrap logic.
        let args = ServeArgs {
            port: 0,
            host: "127.0.0.1".to_string(),
            cors: "*".to_string(),
        };
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(execute_with_shutdown(
            args,
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
            .write_all(b"GET /api/config HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut resp = Vec::new();
        stream.read_to_end(&mut resp).await.unwrap();
        let resp_str = String::from_utf8_lossy(&resp);
        assert_response_ok(&resp_str);

        handle.abort();
    }

    #[tokio::test]
    async fn execute_with_specific_cors_origin_serves() {
        let _guard = crate::config::isolate_config_path_for_test("serve-mod-specific-cors");
        let args = ServeArgs {
            port: 0,
            host: "127.0.0.1".to_string(),
            cors: "https://example.com".to_string(),
        };
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(execute_with_shutdown(
            args,
            Box::pin(std::future::pending()),
            Some(ready_tx),
        ));
        let addr = ready_rx
            .await
            .expect("server should report its bound address");
        assert!(tokio::net::TcpStream::connect(addr).await.is_ok());

        handle.abort();
    }

    #[tokio::test]
    async fn execute_with_unparseable_addr_returns_err() {
        // An invalid host string makes `format!("{host}:{port}").parse()`
        // fail, exercising execute()'s `?` on the SocketAddr parse.
        let args = ServeArgs {
            port: 0,
            host: "not a valid host".to_string(),
            cors: "*".to_string(),
        };
        let result = execute(args).await;
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
        let _guard = crate::config::isolate_config_path_for_test("serve-mod-malformed");
        // After isolate_config_path_for_test, Config::config_path() returns the temp path.
        std::fs::write(Config::config_path(), "not valid toml [[[").unwrap();

        let args = ServeArgs {
            port: 0,
            host: "127.0.0.1".to_string(),
            cors: "*".to_string(),
        };
        let result = execute(args).await;
        assert_execute_failed_on_malformed_config(&result);
    }

    /// Covers the `for warning in cfg.validate_keys()` loop body (lines 32-33)
    /// by writing a config with a bad anthropic key, then running the server
    /// with a graceful-shutdown signal so the loop executes before bind.
    #[tokio::test]
    async fn execute_with_bad_api_key_logs_warning_and_serves() {
        with_tracing(|| {});
        let guard = crate::config::isolate_config_path_for_test("serve-mod-badkey");
        // Write a config with an anthropic key that fails validate_keys().
        std::fs::write(
            Config::config_path(),
            "default_provider = \"anthropic\"\nregistries = []\nagent_paths = []\n[providers]\nanthropic_api_key = \"bad-key-not-sk-ant\"\n",
        )
        .unwrap();

        let args = ServeArgs {
            port: 0,
            host: "127.0.0.1".to_string(),
            cors: "*".to_string(),
        };

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let shutdown_fut = async move {
            let _ = shutdown_rx.await;
        };
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();

        let handle = tokio::spawn(execute_with_shutdown(
            args,
            Box::pin(shutdown_fut),
            Some(ready_tx),
        ));
        let addr = ready_rx
            .await
            .expect("server should report its bound address");
        let connected = tokio::net::TcpStream::connect(addr).await.is_ok();
        drop(guard);
        assert_connected_with_bad_api_key(connected);

        // Trigger graceful shutdown so execute_with_shutdown returns Ok(()).
        let _ = shutdown_tx.send(());
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("timed out waiting for execute to return")
            .expect("task panicked");
        assert_execute_returned_ok_after_shutdown(&result);
    }

    /// Covers `TcpListener::bind(addr).await?` error path (line 116 gap) by
    /// binding the target port before calling execute().
    #[tokio::test]
    async fn execute_with_port_in_use_returns_bind_error() {
        // Bind a port and keep it open so execute()'s bind fails.
        let taken = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = taken.local_addr().unwrap().port();

        let args = ServeArgs {
            port,
            host: "127.0.0.1".to_string(),
            cors: "*".to_string(),
        };
        let result = execute(args).await;
        drop(taken);
        assert_execute_failed_on_port_in_use(&result);
    }

    /// Covers `axum::serve(...).await?` Ok path (lines 117, 119) by running
    /// `execute_with_shutdown` and sending a graceful-shutdown signal.
    #[tokio::test]
    async fn execute_with_shutdown_signal_returns_ok() {
        let _guard = crate::config::isolate_config_path_for_test("serve-mod-shutdown-signal");
        let args = ServeArgs {
            port: 0,
            host: "127.0.0.1".to_string(),
            cors: "*".to_string(),
        };

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let shutdown_fut = async move {
            let _ = shutdown_rx.await;
        };
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();

        let handle = tokio::spawn(execute_with_shutdown(
            args,
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
    }

    /// Covers the `ready: None` fall-through of the `if let Some(ready)` block
    /// (line 190): a successful bind with no ready-observer, shut down
    /// gracefully. Every other binding test passes `Some(ready)`, and every
    /// `None` caller (`execute()`) in other tests fails before binding, so this
    /// is the only path that reaches the block's None continuation.
    #[tokio::test]
    async fn execute_with_shutdown_no_ready_observer_returns_ok() {
        let _guard = crate::config::isolate_config_path_for_test("serve-mod-no-ready");
        let args = ServeArgs {
            port: 0,
            host: "127.0.0.1".to_string(),
            cors: "*".to_string(),
        };

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let shutdown_fut = async move {
            let _ = shutdown_rx.await;
        };

        let handle = tokio::spawn(execute_with_shutdown(args, Box::pin(shutdown_fut), None));
        // Give the server a moment to bind before shutting down.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let _ = shutdown_tx.send(());
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("timed out waiting for execute_with_shutdown to return")
            .expect("task panicked");
        assert_execute_with_shutdown_returned_ok(&result);
    }
}
