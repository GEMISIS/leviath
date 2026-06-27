//! Ollama provider implementation.
//!
//! Ollama provides local LLM execution.

use crate::provider::{Provider, InferenceRequest, InferenceResponse, Result, TokenUsage, FinishReason};
use async_trait::async_trait;
use reqwest::Client;

/// Ollama provider for local LLM execution.
pub struct OllamaProvider {
    /// HTTP client
    client: Client,

    /// API base URL (defaults to local)
    base_url: String,
}

impl OllamaProvider {
    /// Create a new Ollama provider.
    pub fn new() -> Self {
        Self {
            client: Client::new(),
            base_url: "http://localhost:11434".to_string(),
        }
    }

    /// Create a new Ollama provider with custom base URL.
    pub fn with_base_url(base_url: String) -> Self {
        Self {
            client: Client::new(),
            base_url,
        }
    }
}

impl Default for OllamaProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for OllamaProvider {
    async fn infer(&self, request: InferenceRequest) -> Result<InferenceResponse> {
        tracing::debug!(model = %request.model, "Calling Ollama API (mock mode)");
        
        // Mock response for testing the pipeline
        let prompt_tokens = request.messages.iter()
            .map(|m| self.count_tokens(&m.content, &request.model))
            .sum();
        
        let response_content = format!(
            "Mock Ollama {} response for: {}",
            request.model,
            request.messages.last()
                .map(|m| m.content.chars().take(50).collect::<String>())
                .unwrap_or_default()
        );
        
        let completion_tokens = self.count_tokens(&response_content, &request.model);
        
        Ok(InferenceResponse {
            content: response_content,
            tool_calls: Vec::new(), // Most local models don't support tool calling
            tokens_used: TokenUsage {
                prompt_tokens,
                completion_tokens,
                total_tokens: prompt_tokens + completion_tokens,
            },
            finish_reason: FinishReason::Complete,
        })
    }

    fn count_tokens(&self, text: &str, _model: &str) -> usize {
        // Approximate counting (model-dependent)
        text.len() / 4
    }

    fn max_context_tokens(&self, _model: &str) -> usize {
        // Model-dependent, query Ollama API in production
        // Conservative default for common models
        4096
    }

    fn name(&self) -> &str {
        "ollama"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_creation() {
        let provider = OllamaProvider::new();
        assert_eq!(provider.name(), "ollama");
        assert!(provider.base_url.contains("localhost"));
    }

    #[test]
    fn test_custom_base_url() {
        let provider = OllamaProvider::with_base_url("http://custom:11434".to_string());
        assert_eq!(provider.base_url, "http://custom:11434");
    }
}
