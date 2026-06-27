//! OpenRouter provider implementation.
//!
//! OpenRouter provides access to multiple models through a unified API.

use crate::provider::{Provider, InferenceRequest, InferenceResponse, Result, TokenUsage, FinishReason};
use async_trait::async_trait;
use reqwest::Client;

/// OpenRouter provider.
pub struct OpenRouterProvider {
    /// HTTP client
    client: Client,

    /// API key
    api_key: String,

    /// API base URL
    base_url: String,
}

impl OpenRouterProvider {
    /// Create a new OpenRouter provider.
    pub fn new(api_key: String) -> Self {
        Self {
            client: Client::new(),
            api_key,
            base_url: "https://openrouter.ai/api/v1".to_string(),
        }
    }
}

#[async_trait]
impl Provider for OpenRouterProvider {
    async fn infer(&self, request: InferenceRequest) -> Result<InferenceResponse> {
        tracing::debug!(model = %request.model, "Calling OpenRouter API (mock mode)");
        
        // Mock response for testing the pipeline
        let prompt_tokens = request.messages.iter()
            .map(|m| self.count_tokens(&m.content, &request.model))
            .sum();
        
        let response_content = format!(
            "Mock OpenRouter {} response for: {}",
            request.model,
            request.messages.last()
                .map(|m| m.content.chars().take(50).collect::<String>())
                .unwrap_or_default()
        );
        
        let completion_tokens = self.count_tokens(&response_content, &request.model);
        
        Ok(InferenceResponse {
            content: response_content,
            tool_calls: Vec::new(),
            tokens_used: TokenUsage {
                prompt_tokens,
                completion_tokens,
                total_tokens: prompt_tokens + completion_tokens,
            },
            finish_reason: FinishReason::Complete,
        })
    }

    fn count_tokens(&self, text: &str, _model: &str) -> usize {
        // Approximate counting (provider-specific tokenizers not available)
        text.len() / 4
    }

    fn max_context_tokens(&self, _model: &str) -> usize {
        // Model-dependent, varies by underlying provider
        // Conservative default
        128_000
    }

    fn name(&self) -> &str {
        "openrouter"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_creation() {
        let provider = OpenRouterProvider::new("test-key".to_string());
        assert_eq!(provider.name(), "openrouter");
    }
}
