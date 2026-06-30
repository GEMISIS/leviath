//! Config and models endpoints.

use axum::extract::State;
use axum::response::Json;

use super::types::*;
use crate::commands::run::build_provider_registry;

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

#[cfg(test)]
mod tests {
    use super::*;

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
