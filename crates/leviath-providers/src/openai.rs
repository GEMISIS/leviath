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
        tracing::debug!(model = %request.model, "Calling OpenAI API (mock mode)");
        
        // Mock response for testing the pipeline
        use crate::provider::{TokenUsage, FinishReason};
        
        let prompt_tokens = request.messages.iter()
            .map(|m| self.count_tokens(&m.content, &request.model))
            .sum();
        
        let response_content = format!(
            "Mock OpenAI {} response for: {}",
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
        // OpenAI: ~4 chars per token (use tiktoken for accuracy in production)
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
