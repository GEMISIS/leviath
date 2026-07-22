//! Config and models endpoints.

use axum::extract::State;
use axum::response::Json;

use super::types::*;
use crate::commands::run::build_provider_registry_from_config;

pub(super) async fn get_config(State(state): State<AppState>) -> Json<RedactedConfig> {
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

pub(super) async fn get_models(State(state): State<AppState>) -> Json<Vec<ModelEntry>> {
    let registry = build_provider_registry_from_config(&state.config);
    let mut models = Vec::new();

    for provider_name in registry.provider_names() {
        let provider = registry
            .get(provider_name)
            .expect("provider_names returns registered names");
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

    Json(models)
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

    use crate::commands::serve::types::ServerEvent;
    use crate::config::Config;

    fn test_state() -> AppState {
        let (tx, _) = broadcast::channel::<ServerEvent>(64);
        AppState {
            config: Arc::new(Config::default()),
            event_tx: tx,
            control: crate::commands::serve::testutil::no_daemon_client(),
            mcp: crate::commands::serve::mcp::McpAdmin::default(),
        }
    }

    fn test_state_with_keys() -> AppState {
        let (tx, _) = broadcast::channel::<ServerEvent>(64);
        AppState {
            config: Arc::new(Config {
                providers: crate::config::ProviderConfig {
                    anthropic_api_key: Some("sk-ant-test".to_string()),
                    openai_api_key: Some("sk-openai-test".to_string()),
                    google_api_key: None,
                    claude_code_enabled: false,
                    claude_code_binary: None,
                    claude_code_effort: None,
                },
                openrouter_api_key: Some("sk-or-test".to_string()),
                ollama_base_url: Some("http://localhost:11434".to_string()),
                mcp_servers: vec![],
                ..Default::default()
            }),
            event_tx: tx,
            control: crate::commands::serve::testutil::no_daemon_client(),
            mcp: crate::commands::serve::mcp::McpAdmin::default(),
        }
    }

    // ─── get_config endpoint ──────────────────────────────────────────────────

    #[tokio::test]
    async fn get_config_default_returns_ok() {
        let app = Router::new()
            .route("/api/config", get(get_config))
            .with_state(test_state());
        let req = Request::builder()
            .uri("/api/config")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let config: RedactedConfig = serde_json::from_slice(&body).unwrap();
        assert_eq!(config.default_provider, "anthropic");
        assert!(!config.has_anthropic_key);
        assert!(!config.has_openai_key);
        assert!(!config.has_openrouter_key);
        assert!(config.ollama_base_url.is_none());
    }

    #[tokio::test]
    async fn get_config_with_keys_shows_has_key_true() {
        let app = Router::new()
            .route("/api/config", get(get_config))
            .with_state(test_state_with_keys());
        let req = Request::builder()
            .uri("/api/config")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let config: RedactedConfig = serde_json::from_slice(&body).unwrap();
        assert!(config.has_anthropic_key);
        assert!(config.has_openai_key);
        assert!(config.has_openrouter_key);
        assert_eq!(
            config.ollama_base_url.as_deref(),
            Some("http://localhost:11434")
        );
        // Must not contain actual key values
        let raw = std::str::from_utf8(&body).unwrap();
        assert!(!raw.contains("sk-ant-test"));
        assert!(!raw.contains("sk-openai-test"));
    }

    #[tokio::test]
    async fn get_config_agent_paths_included() {
        let (tx, _) = broadcast::channel::<ServerEvent>(64);
        let state = AppState {
            config: Arc::new(Config {
                agent_paths: vec![
                    std::path::PathBuf::from("/my/agents"),
                    std::path::PathBuf::from("/other/agents"),
                ],
                ..Default::default()
            }),
            event_tx: tx,
            control: crate::commands::serve::testutil::no_daemon_client(),
            mcp: crate::commands::serve::mcp::McpAdmin::default(),
        };
        let app = Router::new()
            .route("/api/config", get(get_config))
            .with_state(state);
        let req = Request::builder()
            .uri("/api/config")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let config: RedactedConfig = serde_json::from_slice(&body).unwrap();
        assert_eq!(config.agent_paths.len(), 2);
    }

    // ─── get_models endpoint ──────────────────────────────────────────────────

    /// AppState whose registry has a provider that actually enumerates models,
    /// so the `/api/models` handler's list-building loop runs. `claude-code`
    /// needs no API key and `list_models` returns its three known models.
    fn test_state_listing_models() -> AppState {
        let (tx, _) = broadcast::channel::<ServerEvent>(64);
        AppState {
            config: Arc::new(Config {
                providers: crate::config::ProviderConfig {
                    claude_code_enabled: true,
                    ..Config::default().providers
                },
                ..Config::default()
            }),
            event_tx: tx,
            control: crate::commands::serve::testutil::no_daemon_client(),
            mcp: crate::commands::serve::mcp::McpAdmin::default(),
        }
    }

    #[tokio::test]
    async fn get_models_returns_ok() {
        let app = Router::new()
            .route("/api/models", get(get_models))
            .with_state(test_state());
        let req = Request::builder()
            .uri("/api/models")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let models: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        // With default config (no API keys, claude-code off), providers may
        // return empty lists, but the endpoint itself should succeed.
        let _ = models;
    }

    #[tokio::test]
    async fn get_models_enumerates_when_a_provider_lists_models() {
        let app = Router::new()
            .route("/api/models", get(get_models))
            .with_state(test_state_listing_models());
        let req = Request::builder()
            .uri("/api/models")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let models: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        // claude-code enumerates its three known models, so the handler's
        // per-model mapping loop actually runs and produces entries.
        assert!(!models.is_empty());
        assert!(models.iter().any(|m| m["provider"] == "claude-code"));
        assert!(models.iter().all(|m| m["id"].is_string()));
    }

    #[test]
    fn redacted_config_hides_keys() {
        let config = RedactedConfig {
            default_provider: "anthropic".to_string(),
            has_anthropic_key: true,
            has_openai_key: false,
            has_openrouter_key: false,
            ollama_base_url: None,
            agent_paths: vec![],
            registries: vec![],
            mcp_server_count: 2,
        };
        let json = serde_json::to_string(&config).unwrap();
        // Must NOT contain actual key values
        assert!(!json.contains("sk-"));
        assert!(json.contains("\"has_anthropic_key\":true"));
        assert!(json.contains("\"has_openai_key\":false"));
        assert!(json.contains("\"mcp_server_count\":2"));
    }

    #[test]
    fn redacted_config_with_ollama_url() {
        let config = RedactedConfig {
            default_provider: "ollama".to_string(),
            has_anthropic_key: false,
            has_openai_key: false,
            has_openrouter_key: false,
            ollama_base_url: Some("http://localhost:11434".to_string()),
            agent_paths: vec![],
            registries: vec!["https://registry.example.com".to_string()],
            mcp_server_count: 0,
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("\"ollama_base_url\":\"http://localhost:11434\""));
        assert!(json.contains("registry.example.com"));
    }
}
