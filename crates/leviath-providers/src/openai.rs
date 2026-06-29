//! OpenAI provider implementation.

use crate::openai_compat::{build_openai_request_body, parse_openai_response, OpenAiSseStream};
#[cfg(test)]
use crate::provider::FinishReason;
use crate::provider::{
    check_http_response, InferenceRequest, InferenceResponse, ModelCapabilities, ModelInfo,
    Provider, ProviderConfig, ProviderError, Result, StreamChunk,
};
use crate::rate_limit::RateLimiter;
use async_trait::async_trait;
use futures_core::Stream;
use reqwest::Client;
use std::collections::HashMap;
use std::pin::Pin;

/// OpenAI provider.
pub struct OpenAIProvider {
    /// HTTP client
    client: Client,

    /// API key
    api_key: String,

    /// API base URL
    base_url: String,

    /// Rate limiter
    rate_limiter: Option<RateLimiter>,

    /// Per-model capability overrides
    capability_overrides: HashMap<String, ModelCapabilities>,
}

impl OpenAIProvider {
    /// Create a new OpenAI provider.
    pub fn new(api_key: String) -> Self {
        Self {
            client: Client::new(),
            api_key,
            base_url: "https://api.openai.com/v1".to_string(),
            rate_limiter: None,
            capability_overrides: HashMap::new(),
        }
    }

    /// Create a new OpenAI provider with full configuration.
    pub fn with_config(config: ProviderConfig) -> Self {
        let rate_limiter = config.rate_limit.as_ref().map(RateLimiter::new);
        Self {
            client: Client::new(),
            api_key: config.api_key,
            base_url: config
                .base_url
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
            rate_limiter,
            capability_overrides: HashMap::new(),
        }
    }

    /// Create a new OpenAI provider with per-model capability overrides.
    pub fn with_overrides(api_key: String, overrides: HashMap<String, ModelCapabilities>) -> Self {
        Self {
            client: Client::new(),
            api_key,
            base_url: "https://api.openai.com/v1".to_string(),
            rate_limiter: None,
            capability_overrides: overrides,
        }
    }

    /// Return built-in capability defaults for a model.
    fn builtin_capabilities(&self, model: &str) -> ModelCapabilities {
        // GPT-5.5 — flagship, 1M+ context, 128K output (check before generic gpt-5)
        if model.starts_with("gpt-5.5") {
            ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 1_050_000,
                max_output_tokens: 128_000,
            }
        // GPT-5.x family (5.4, 5.4-mini, 5.4-nano, 5-mini) — 400K context, 128K output
        } else if model.starts_with("gpt-5") {
            ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 400_000,
                max_output_tokens: 128_000,
            }
        // GPT-4.1 family — 1M context (must check before generic gpt-4)
        } else if model.starts_with("gpt-4.1") {
            ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 1_047_576,
                max_output_tokens: 32_768,
            }
        // o-series reasoning models (o3, o4) — no temperature, 200K context
        } else if model.starts_with("o3") || model.starts_with("o4") {
            ModelCapabilities {
                supports_temperature: false,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 200_000,
                max_output_tokens: 100_000,
            }
        } else {
            ModelCapabilities::default()
        }
    }
}

#[async_trait]
impl Provider for OpenAIProvider {
    async fn infer(&self, request: InferenceRequest) -> Result<InferenceResponse> {
        tracing::debug!(model = %request.model, "Calling OpenAI API");

        if let Some(limiter) = &self.rate_limiter {
            limiter.acquire().await?;
        }

        let body = build_openai_request_body(&request);

        let response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::RequestFailed(e.to_string()))?;

        let response = check_http_response(response, self.rate_limiter.as_ref()).await?;

        if let Some(limiter) = &self.rate_limiter {
            limiter.reset_backoff().await;
        }

        let response_body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| ProviderError::InvalidResponse(e.to_string()))?;

        let result = parse_openai_response(&response_body)?;

        if let Some(limiter) = &self.rate_limiter {
            limiter.record_tokens(result.tokens_used.total_tokens).await;
        }

        Ok(result)
    }

    async fn infer_stream(
        &self,
        request: InferenceRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>> {
        tracing::debug!(model = %request.model, "Calling OpenAI API (streaming)");

        if let Some(limiter) = &self.rate_limiter {
            limiter.acquire().await?;
        }

        let mut body = build_openai_request_body(&request);
        body["stream"] = serde_json::Value::Bool(true);
        body["stream_options"] = serde_json::json!({ "include_usage": true });

        let response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::RequestFailed(e.to_string()))?;

        let response = check_http_response(response, self.rate_limiter.as_ref()).await?;

        if let Some(limiter) = &self.rate_limiter {
            limiter.reset_backoff().await;
        }

        let byte_stream = response.bytes_stream();
        let stream = OpenAiSseStream::new(byte_stream);

        Ok(Box::pin(stream))
    }

    fn count_tokens(&self, text: &str, model: &str) -> usize {
        crate::tokenizer::count_tokens(text, model)
    }

    fn max_context_tokens(&self, model: &str) -> usize {
        self.capabilities(model).max_context_tokens
    }

    fn name(&self) -> &str {
        "openai"
    }

    fn capabilities(&self, model: &str) -> ModelCapabilities {
        if let Some(caps) = self.capability_overrides.get(model) {
            caps.clone()
        } else {
            self.builtin_capabilities(model)
        }
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let response = self
            .client
            .get(format!("{}/models", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .send()
            .await
            .map_err(|e| ProviderError::RequestFailed(e.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            let error_body = response
                .text()
                .await
                .unwrap_or_else(|_| "unknown error".to_string());
            return Err(ProviderError::ApiError(format!(
                "HTTP {}: {}",
                status, error_body
            )));
        }

        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| ProviderError::InvalidResponse(e.to_string()))?;

        let models = body
            .get("data")
            .and_then(|d| d.as_array())
            .ok_or_else(|| {
                ProviderError::InvalidResponse("No data field in models response".to_string())
            })?
            .iter()
            .filter_map(|item| {
                let id = item.get("id")?.as_str()?.to_string();
                let capabilities = self.capabilities(&id);
                Some(ModelInfo {
                    id: id.clone(),
                    display_name: None,
                    provider: "openai".into(),
                    capabilities,
                })
            })
            .collect();

        Ok(models)
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
        assert_eq!(provider.max_context_tokens("gpt-5.4-mini"), 400_000);
    }

    #[test]
    fn test_build_request_body() {
        let request = InferenceRequest {
            messages: vec![
                crate::provider::Message {
                    role: "system".to_string(),
                    content: "You are helpful.".to_string(),
                },
                crate::provider::Message {
                    role: "user".to_string(),
                    content: "Hello".to_string(),
                },
            ],
            model: "gpt-5.4-mini".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
        };

        let body = build_openai_request_body(&request);
        assert_eq!(body["model"], "gpt-5.4-mini");
        assert_eq!(body["messages"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_parse_response() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "Hello!"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15
            }
        });

        let response = parse_openai_response(&body).unwrap();
        assert_eq!(response.content, "Hello!");
        assert_eq!(response.tokens_used.prompt_tokens, 10);
        assert!(matches!(response.finish_reason, FinishReason::Complete));
    }

    #[test]
    fn test_parse_response_with_tool_calls() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_123",
                        "type": "function",
                        "function": {
                            "name": "search",
                            "arguments": "{\"query\": \"rust\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 20, "completion_tokens": 15, "total_tokens": 35 }
        });

        let response = parse_openai_response(&body).unwrap();
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].name, "search");
        assert!(matches!(response.finish_reason, FinishReason::ToolCall));
    }

    #[test]
    fn test_builtin_capabilities_gpt55() {
        let provider = OpenAIProvider::new("test-key".to_string());
        let caps = provider.builtin_capabilities("gpt-5.5");
        assert!(caps.supports_temperature);
        assert!(caps.supports_streaming);
        assert_eq!(caps.max_context_tokens, 1_050_000);
        assert_eq!(caps.max_output_tokens, 128_000);
    }

    #[test]
    fn test_builtin_capabilities_gpt54_mini() {
        let provider = OpenAIProvider::new("test-key".to_string());
        let caps = provider.builtin_capabilities("gpt-5.4-mini");
        assert!(caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 400_000);
        assert_eq!(caps.max_output_tokens, 128_000);
    }

    #[test]
    fn test_builtin_capabilities_gpt54_nano() {
        let provider = OpenAIProvider::new("test-key".to_string());
        let caps = provider.builtin_capabilities("gpt-5.4-nano");
        assert!(caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 400_000);
        assert_eq!(caps.max_output_tokens, 128_000);
    }

    #[test]
    fn test_builtin_capabilities_gpt41() {
        let provider = OpenAIProvider::new("test-key".to_string());
        let caps = provider.builtin_capabilities("gpt-4.1");
        assert!(caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 1_047_576);
        assert_eq!(caps.max_output_tokens, 32_768);
    }

    #[test]
    fn test_builtin_capabilities_o4_mini() {
        let provider = OpenAIProvider::new("test-key".to_string());
        let caps = provider.builtin_capabilities("o4-mini");
        assert!(!caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 200_000);
        assert_eq!(caps.max_output_tokens, 100_000);
    }

    #[test]
    fn test_builtin_capabilities_o3() {
        let provider = OpenAIProvider::new("test-key".to_string());
        let caps = provider.builtin_capabilities("o3-mini");
        assert!(!caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 200_000);
        assert_eq!(caps.max_output_tokens, 100_000);
    }

    #[test]
    fn test_capabilities_override() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "gpt-5.4-mini".to_string(),
            ModelCapabilities {
                supports_temperature: false,
                supports_streaming: false,
                supports_tools: false,
                supports_system_prompt: false,
                max_context_tokens: 1,
                max_output_tokens: 1,
            },
        );
        let provider = OpenAIProvider::with_overrides("test-key".to_string(), overrides);
        let caps = provider.capabilities("gpt-5.4-mini");
        assert!(!caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 1);
    }
}
