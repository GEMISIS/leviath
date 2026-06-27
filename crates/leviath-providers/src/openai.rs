//! OpenAI provider implementation.

use crate::provider::{Provider, InferenceRequest, InferenceResponse, Result, ProviderError};
use async_trait::async_trait;
use reqwest::Client;

/// OpenAI provider.
pub struct OpenAIProvider {
    /// HTTP client
    client: Client,

    /// API key
    api_key: String,

    /// API base URL
    base_url: String,
}

impl OpenAIProvider {
    /// Create a new OpenAI provider.
    pub fn new(api_key: String) -> Self {
        Self {
            client: Client::new(),
            api_key,
            base_url: "https://api.openai.com/v1".to_string(),
        }
    }

    /// Construct a request for the OpenAI API.
    fn build_request(&self, request: &InferenceRequest) -> serde_json::Value {
        // TODO: Implement proper message construction
        // - Build messages from context regions
        // - Include tools in OpenAI format
        // - Handle Responses API vs standard completion
        
        serde_json::json!({
            "model": request.model,
            "max_tokens": request.max_tokens,
            "messages": request.messages,
            "temperature": request.temperature,
        })
    }
}

#[async_trait]
impl Provider for OpenAIProvider {
    async fn infer(&self, request: InferenceRequest) -> Result<InferenceResponse> {
        // TODO: Implement actual API call
        tracing::debug!(model = %request.model, "Calling OpenAI API");
        
        // Placeholder response
        Err(ProviderError::Other("Not implemented".to_string()))
    }

    fn count_tokens(&self, text: &str, model: &str) -> usize {
        // TODO: Use tiktoken for accurate counting
        // For now, rough estimate
        text.len() / 4
    }

    fn max_context_tokens(&self, model: &str) -> usize {
        // Known context limits for OpenAI models
        match model {
            m if m.starts_with("gpt-4") => 128_000,
            m if m.starts_with("gpt-3.5") => 16_384,
            _ => 128_000, // Default
        }
    }

    fn name(&self) -> &str {
        "openai"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_creation() {
        let provider = OpenAIProvider::new("test-key".to_string());
        assert_eq!(provider.name(), "openai");
    }

    #[test]
    fn test_context_limits() {
        let provider = OpenAIProvider::new("test-key".to_string());
        assert_eq!(provider.max_context_tokens("gpt-4"), 128_000);
    }
}
