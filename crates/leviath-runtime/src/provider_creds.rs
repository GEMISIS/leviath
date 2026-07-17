//! Decoupled provider credentials + registry construction.
//!
//! [`ProviderCreds`] is the plain-data seam that lets the run engine build a
//! [`ProviderRegistry`] without depending on the CLI's `Config`/`ProviderConfig`
//! types. The CLI owns the `Config -> Vec<ProviderCreds>` translation
//! (`provider_creds_from_config`); this module owns everything downstream of it.

use crate::ProviderRegistry;
use std::sync::Arc;

/// Decoupled provider credentials.
///
/// Plain data so [`build_provider_registry`] can instantiate providers without
/// depending on the CLI's `Config`/`ProviderConfig` types. Build one per
/// provider that should be registered.
#[derive(Clone, Debug)]
pub struct ProviderCreds {
    /// Provider identifier: `anthropic` | `openai` | `google` | `openrouter` |
    /// `ollama` | `claude-code`. Selects which provider is instantiated.
    pub name: String,
    /// API key, when the provider needs one (`None` for `ollama`/`claude-code`).
    pub api_key: Option<String>,
    /// Base URL override (used by `ollama`; `None` uses the built-in default).
    pub base_url: Option<String>,
    /// Per-model capability overrides forwarded to the provider.
    pub model_capabilities: std::collections::HashMap<String, leviath_providers::ModelCapabilities>,
    /// HTTP request timeout in seconds (`None` uses the provider default).
    pub request_timeout_secs: Option<u64>,
}

/// Build a [`ProviderRegistry`] from decoupled [`ProviderCreds`].
pub fn build_provider_registry(creds: &[ProviderCreds]) -> ProviderRegistry {
    let mut registry = ProviderRegistry::new();

    for c in creds {
        let caps = c.model_capabilities.clone();
        let timeout = c.request_timeout_secs;
        match c.name.as_str() {
            "anthropic" => {
                if let Some(ref key) = c.api_key {
                    registry.register(
                        "anthropic".to_string(),
                        Arc::new(leviath_providers::AnthropicProvider::with_overrides(
                            key.clone(),
                            caps,
                            timeout,
                        )),
                    );
                }
            }
            "openai" => {
                if let Some(ref key) = c.api_key {
                    registry.register(
                        "openai".to_string(),
                        Arc::new(leviath_providers::OpenAIProvider::with_overrides(
                            key.clone(),
                            caps,
                            timeout,
                        )),
                    );
                }
            }
            "google" => {
                if let Some(ref key) = c.api_key {
                    registry.register(
                        "google".to_string(),
                        Arc::new(leviath_providers::GeminiProvider::with_overrides(
                            key.clone(),
                            caps,
                            timeout,
                        )),
                    );
                }
            }
            "openrouter" => {
                if let Some(ref key) = c.api_key {
                    registry.register(
                        "openrouter".to_string(),
                        Arc::new(leviath_providers::OpenRouterProvider::with_overrides(
                            key.clone(),
                            caps,
                            timeout,
                        )),
                    );
                }
            }
            "ollama" => {
                let url = c
                    .base_url
                    .clone()
                    .unwrap_or_else(|| "http://localhost:11434".to_string());
                registry.register(
                    "ollama".to_string(),
                    Arc::new(leviath_providers::OllamaProvider::with_overrides(
                        url, caps, timeout,
                    )),
                );
            }
            "claude-code" => {
                registry.register(
                    "claude-code".to_string(),
                    Arc::new(leviath_providers::ClaudeCodeProvider::new()),
                );
            }
            _ => {}
        }
    }

    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_provider_registry_from_creds_slice() {
        // Drives `build_provider_registry(&[ProviderCreds])` directly:
        // every keyed provider, the ollama-with-default-url arm, claude-code,
        // and an unknown provider name (the catch-all no-op arm).
        let caps = std::collections::HashMap::new();
        let creds = vec![
            ProviderCreds {
                name: "anthropic".to_string(),
                api_key: Some("sk-ant".to_string()),
                base_url: None,
                model_capabilities: caps.clone(),
                request_timeout_secs: Some(30),
            },
            ProviderCreds {
                name: "openai".to_string(),
                api_key: Some("sk-oa".to_string()),
                base_url: None,
                model_capabilities: caps.clone(),
                request_timeout_secs: None,
            },
            ProviderCreds {
                name: "google".to_string(),
                api_key: Some("AIza".to_string()),
                base_url: None,
                model_capabilities: caps.clone(),
                request_timeout_secs: None,
            },
            ProviderCreds {
                name: "openrouter".to_string(),
                api_key: Some("sk-or".to_string()),
                base_url: None,
                model_capabilities: caps.clone(),
                request_timeout_secs: None,
            },
            ProviderCreds {
                name: "ollama".to_string(),
                api_key: None,
                base_url: None, // exercise the default-URL fallback
                model_capabilities: caps.clone(),
                request_timeout_secs: None,
            },
            ProviderCreds {
                name: "claude-code".to_string(),
                api_key: None,
                base_url: None,
                model_capabilities: caps.clone(),
                request_timeout_secs: None,
            },
            ProviderCreds {
                name: "totally-unknown".to_string(),
                api_key: Some("x".to_string()),
                base_url: None,
                model_capabilities: caps,
                request_timeout_secs: None,
            },
        ];
        let registry = build_provider_registry(&creds);
        assert!(registry.has("anthropic"));
        assert!(registry.has("openai"));
        assert!(registry.has("google"));
        assert!(registry.has("openrouter"));
        assert!(registry.has("ollama"));
        assert!(registry.has("claude-code"));
        assert!(!registry.has("totally-unknown"));
    }
}
