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
use tracing::{info, warn};

use crate::config::Config;

// ─── Entrypoint ──────────────────────────────────────────────────────────────

pub async fn execute(args: ServeArgs) -> anyhow::Result<()> {
    let cfg = Config::load()?;
    for warning in cfg.validate_keys() {
        warn!("{}", warning);
    }

    let (event_tx, _) = broadcast::channel::<ServerEvent>(1024);

    let state = AppState {
        config: Arc::new(cfg),
        event_tx: event_tx.clone(),
    };

    // Background polling loop
    let poll_state = state.clone();
    tokio::spawn(async move {
        polling::polling_loop(poll_state).await;
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
    info!("Listening on http://{}", addr);
    println!("Leviath API server listening on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

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
        };
        let json2 = serde_json::to_string(&event2).unwrap();
        assert!(json2.contains("\"type\":\"tokens\""));
        assert!(json2.contains("\"prompt_tokens\":5000"));
    }
}
