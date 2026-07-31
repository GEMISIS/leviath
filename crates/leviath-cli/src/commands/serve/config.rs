//! Config and models endpoints.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;

use super::types::*;
use crate::commands::run::build_provider_registry_from_config;
use crate::config::Config;

/// Redacted view of a config — booleans for keys, never their values.
fn redact(c: &Config) -> RedactedConfig {
    RedactedConfig {
        default_provider: c.default_provider.clone(),
        has_anthropic_key: c.providers.anthropic_api_key.is_some(),
        has_openai_key: c.providers.openai_api_key.is_some(),
        has_google_key: c.providers.google_api_key.is_some(),
        has_openrouter_key: c.openrouter_api_key.is_some(),
        ollama_base_url: c.ollama_base_url.clone(),
        agent_paths: c.agent_paths.clone(),
        mcp_server_count: c.mcp_servers.len(),
    }
}

pub(super) async fn get_config(State(state): State<AppState>) -> Json<RedactedConfig> {
    Json(redact(&state.config))
}

/// `PUT /api/config` (admin-only). Loads the on-disk config, applies every
/// present field, and writes it back with the file's `0600` permissions — the
/// same file `lev setup` and MCP admin edits. Returns the new redacted config.
pub(super) async fn put_config(
    State(state): State<AppState>,
    Json(req): Json<WriteConfigReq>,
) -> Result<Json<RedactedConfig>, (StatusCode, Json<ErrorResponse>)> {
    let path = &state.mcp.config_path;
    let mut config = Config::load_from_path_public(path).map_err(|e| {
        err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to read config: {e}"),
        )
    })?;

    if let Some(v) = req.default_provider {
        config.default_provider = v;
    }
    if let Some(v) = req.default_model {
        config.default_model = Some(v);
    }
    if let Some(v) = req.anthropic_key {
        config.providers.anthropic_api_key = Some(v);
    }
    if let Some(v) = req.openai_key {
        config.providers.openai_api_key = Some(v);
    }
    if let Some(v) = req.google_key {
        config.providers.google_api_key = Some(v);
    }
    if let Some(v) = req.openrouter_key {
        config.openrouter_api_key = Some(v);
    }
    if let Some(v) = req.ollama_base_url {
        config.ollama_base_url = Some(v);
    }

    config.save_to_path_public(path).map_err(|e| {
        err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to write config: {e}"),
        )
    })?;
    Ok(Json(redact(&config)))
}

/// Format-only validation of a provider key (no network call, no persistence).
fn validate_key_format(provider: &str, key: &str) -> (bool, Option<String>) {
    match provider {
        "anthropic" => {
            if key.starts_with("sk-ant-") {
                (true, None)
            } else {
                (
                    false,
                    Some("Anthropic keys start with `sk-ant-`.".to_string()),
                )
            }
        }
        "openai" => {
            if key.starts_with("sk-") {
                (true, None)
            } else {
                (false, Some("OpenAI keys start with `sk-`.".to_string()))
            }
        }
        "google" | "openrouter" => {
            if key.trim().is_empty() {
                (false, Some("Key must not be empty.".to_string()))
            } else {
                (true, None)
            }
        }
        other => (false, Some(format!("Unknown provider `{other}`."))),
    }
}

pub(super) async fn validate_config_key(Json(req): Json<ValidateKeyReq>) -> Json<ValidateKeyResp> {
    let (valid, message) = validate_key_format(&req.provider, &req.key);
    Json(ValidateKeyResp { valid, message })
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
            limits: Default::default(),
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
            limits: Default::default(),
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
            limits: Default::default(),
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
            limits: Default::default(),
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
            has_google_key: false,
            has_openrouter_key: false,
            ollama_base_url: None,
            agent_paths: vec![],
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
            has_google_key: false,
            has_openrouter_key: false,
            ollama_base_url: Some("http://localhost:11434".to_string()),
            agent_paths: vec![],
            mcp_server_count: 0,
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("\"ollama_base_url\":\"http://localhost:11434\""));
    }

    // ─── put_config endpoint ──────────────────────────────────────────────────

    fn state_with_config_path(path: std::path::PathBuf) -> AppState {
        let (tx, _) = broadcast::channel::<ServerEvent>(64);
        AppState {
            config: Arc::new(Config::default()),
            event_tx: tx,
            control: crate::commands::serve::testutil::no_daemon_client(),
            mcp: crate::commands::serve::mcp::McpAdmin {
                config_path: path,
                ..Default::default()
            },
            limits: Default::default(),
        }
    }

    async fn put_config_request(state: AppState, body: &str) -> axum::http::Response<Body> {
        let app = Router::new()
            .route("/api/config", axum::routing::put(put_config))
            .with_state(state);
        let req = Request::builder()
            .method("PUT")
            .uri("/api/config")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        app.oneshot(req).await.unwrap()
    }

    #[tokio::test]
    async fn put_config_writes_all_present_fields_and_redacts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Config::default().save_to_path_public(&path).unwrap();

        let body = serde_json::json!({
            "default_provider": "openai",
            "default_model": "gpt-5",
            "anthropic_key": "sk-ant-x",
            "openai_key": "sk-openai-x",
            "google_key": "g-x",
            "openrouter_key": "or-x",
            "ollama_base_url": "http://ollama:11434"
        })
        .to_string();
        let resp = put_config_request(state_with_config_path(path.clone()), &body).await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let raw = std::str::from_utf8(&bytes).unwrap();
        assert!(!raw.contains("sk-ant-x"), "must not leak key values");
        let rc: RedactedConfig = serde_json::from_slice(&bytes).unwrap();
        assert!(
            rc.has_anthropic_key && rc.has_openai_key && rc.has_google_key && rc.has_openrouter_key
        );
        assert_eq!(rc.default_provider, "openai");

        let saved = Config::load_from_path_public(&path).unwrap();
        assert_eq!(
            saved.providers.anthropic_api_key.as_deref(),
            Some("sk-ant-x")
        );
        assert_eq!(
            saved.providers.openai_api_key.as_deref(),
            Some("sk-openai-x")
        );
        assert_eq!(saved.providers.google_api_key.as_deref(), Some("g-x"));
        assert_eq!(saved.openrouter_api_key.as_deref(), Some("or-x"));
        assert_eq!(saved.default_model.as_deref(), Some("gpt-5"));
        assert_eq!(
            saved.ollama_base_url.as_deref(),
            Some("http://ollama:11434")
        );
    }

    #[tokio::test]
    async fn put_config_empty_body_leaves_existing_config_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let base = Config {
            providers: crate::config::ProviderConfig {
                anthropic_api_key: Some("sk-ant-keep".to_string()),
                openai_api_key: None,
                google_api_key: None,
                claude_code_enabled: false,
                claude_code_binary: None,
                claude_code_effort: None,
            },
            ..Default::default()
        };
        base.save_to_path_public(&path).unwrap();

        let resp = put_config_request(state_with_config_path(path.clone()), "{}").await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let saved = Config::load_from_path_public(&path).unwrap();
        assert_eq!(
            saved.providers.anthropic_api_key.as_deref(),
            Some("sk-ant-keep")
        );
    }

    #[tokio::test]
    async fn put_config_read_failure_is_500() {
        // config_path points at a directory, so reading it as a file fails.
        let dir = tempfile::tempdir().unwrap();
        let resp = put_config_request(state_with_config_path(dir.path().to_path_buf()), "{}").await;
        assert_eq!(resp.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn put_config_write_failure_is_500() {
        // The config file's parent is itself a file, so saving fails while
        // reading (a non-existent file) succeeds as defaults.
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let path = blocker.join("config.toml");
        let resp = put_config_request(state_with_config_path(path), "{}").await;
        assert_eq!(resp.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    }

    // ─── config key validation ────────────────────────────────────────────────

    #[test]
    fn validate_key_format_covers_every_provider() {
        assert_eq!(validate_key_format("anthropic", "sk-ant-1"), (true, None));
        assert!(!validate_key_format("anthropic", "nope").0);
        assert_eq!(validate_key_format("openai", "sk-1"), (true, None));
        assert!(!validate_key_format("openai", "nope").0);
        assert_eq!(validate_key_format("google", "g"), (true, None));
        assert!(!validate_key_format("google", "  ").0);
        assert_eq!(validate_key_format("openrouter", "or"), (true, None));
        assert!(!validate_key_format("unknown", "x").0);
    }

    #[tokio::test]
    async fn validate_config_key_endpoint_returns_result() {
        let app = Router::new().route(
            "/api/config/validate",
            axum::routing::post(validate_config_key),
        );
        let req = Request::builder()
            .method("POST")
            .uri("/api/config/validate")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({"provider":"anthropic","key":"bad"}).to_string(),
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: ValidateKeyResp = serde_json::from_slice(&bytes).unwrap();
        assert!(!v.valid);
        assert!(v.message.is_some());
    }
}
