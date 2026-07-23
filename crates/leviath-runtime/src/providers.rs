//! The [`ProviderRegistry`]: a name → [`Provider`] lookup shared by the ECS
//! pipeline (as the `Providers` resource) and the CLI/daemon spawn path.

use leviath_providers::Provider;
use std::collections::HashMap;
use std::sync::Arc;

/// Registry of inference providers, keyed by provider name (e.g. `"anthropic"`).
///
/// The pipeline resolves each agent's stage `ModelConfig` to a concrete
/// provider through this registry.
#[derive(Clone, Default)]
pub struct ProviderRegistry {
    providers: HashMap<String, Arc<dyn Provider>>,
}

impl ProviderRegistry {
    /// Create a new empty provider registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a provider by name.
    pub fn register(&mut self, name: String, provider: Arc<dyn Provider>) {
        self.providers.insert(name, provider);
    }

    /// Get a provider by name.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Provider>> {
        self.providers.get(name)
    }

    /// Check if a provider is registered.
    pub fn has(&self, name: &str) -> bool {
        self.providers.contains_key(name)
    }

    /// Get all registered provider names.
    pub fn provider_names(&self) -> Vec<&str> {
        self.providers.keys().map(|k| k.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_providers::{
        InferenceRequest, InferenceResponse, ModelCapabilities, ProviderError,
    };

    struct StubProvider;

    #[async_trait::async_trait]
    impl Provider for StubProvider {
        async fn infer(
            &self,
            _request: InferenceRequest,
        ) -> Result<InferenceResponse, ProviderError> {
            Err(ProviderError::ApiError("stub".to_string()))
        }
        async fn count_tokens(&self, text: &str, _model: &str) -> usize {
            text.len()
        }
        fn max_context_tokens(&self, _model: &str) -> usize {
            8192
        }
        fn name(&self) -> &str {
            "stub"
        }
        fn capabilities(&self, _model: &str) -> ModelCapabilities {
            ModelCapabilities::default()
        }
    }

    fn mock() -> Arc<dyn Provider> {
        Arc::new(StubProvider)
    }

    #[test]
    fn register_get_has_and_names() {
        let mut reg = ProviderRegistry::new();
        assert!(!reg.has("anthropic"));
        assert!(reg.get("anthropic").is_none());
        reg.register("anthropic".to_string(), mock());
        assert!(reg.has("anthropic"));
        assert!(reg.get("anthropic").is_some());
        assert_eq!(reg.provider_names(), vec!["anthropic"]);
    }

    #[test]
    fn default_is_empty() {
        let reg = ProviderRegistry::default();
        assert!(reg.provider_names().is_empty());
    }

    #[tokio::test]
    async fn stub_provider_methods_are_exercised() {
        let p = StubProvider;
        assert_eq!(p.name(), "stub");
        assert_eq!(p.count_tokens("abcd", "m").await, 4);
        assert_eq!(p.max_context_tokens("m"), 8192);
        let _ = p.capabilities("m");
        let request = InferenceRequest {
            system: Vec::new(),
            messages: Vec::new(),
            model: "m".to_string(),
            max_tokens: 10,
            temperature: 0.0,
            tools: Vec::new(),
            extra: serde_json::Value::Null,
        };
        assert!(p.infer(request).await.is_err());
    }
}
