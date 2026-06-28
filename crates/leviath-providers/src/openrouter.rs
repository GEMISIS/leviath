//! OpenRouter provider implementation.
//!
//! OpenRouter provides access to multiple models through a unified API.
//! Uses OpenAI-compatible format with additional headers.

use crate::openai::OpenAiSseStream;
use crate::provider::{
    FinishReason, InferenceRequest, InferenceResponse, ModelCapabilities, ModelInfo, Provider,
    ProviderConfig, ProviderError, Result, StreamChunk, TokenUsage, ToolCall,
};
use crate::rate_limit::RateLimiter;
use async_trait::async_trait;
use futures_core::Stream;
use reqwest::Client;
use std::collections::HashMap;
use std::pin::Pin;

/// OpenRouter provider.
pub struct OpenRouterProvider {
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

impl OpenRouterProvider {
    /// Create a new OpenRouter provider.
    pub fn new(api_key: String) -> Self {
        Self {
            client: Client::new(),
            api_key,
            base_url: "https://openrouter.ai/api/v1".to_string(),
            rate_limiter: None,
            capability_overrides: HashMap::new(),
        }
    }

    /// Create a new OpenRouter provider with full configuration.
    pub fn with_config(config: ProviderConfig) -> Self {
        let rate_limiter = config.rate_limit.as_ref().map(RateLimiter::new);
        Self {
            client: Client::new(),
            api_key: config.api_key,
            base_url: config
                .base_url
                .unwrap_or_else(|| "https://openrouter.ai/api/v1".to_string()),
            rate_limiter,
            capability_overrides: HashMap::new(),
        }
    }

    /// Create a new OpenRouter provider with per-model capability overrides.
    pub fn with_overrides(api_key: String, overrides: HashMap<String, ModelCapabilities>) -> Self {
        Self {
            client: Client::new(),
            api_key,
            base_url: "https://openrouter.ai/api/v1".to_string(),
            rate_limiter: None,
            capability_overrides: overrides,
        }
    }

    /// Build the request body (OpenAI-compatible format).
    fn build_request_body(&self, request: &InferenceRequest) -> serde_json::Value {
        let messages: Vec<serde_json::Value> = request
            .messages
            .iter()
            .map(|msg| {
                serde_json::json!({
                    "role": msg.role,
                    "content": msg.content,
                })
            })
            .collect();

        let mut body = serde_json::json!({
            "model": request.model,
            "max_tokens": request.max_tokens,
            "temperature": request.temperature,
            "messages": messages,
        });

        if !request.tools.is_empty() {
            let tools: Vec<serde_json::Value> = request
                .tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters,
                        }
                    })
                })
                .collect();
            body["tools"] = serde_json::Value::Array(tools);
        }

        body
    }

    /// Parse response (OpenAI-compatible format).
    fn parse_response(&self, body: &serde_json::Value) -> Result<InferenceResponse> {
        let choice = body
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|c| c.first())
            .ok_or_else(|| ProviderError::InvalidResponse("No choices in response".to_string()))?;

        let message = choice.get("message").ok_or_else(|| {
            ProviderError::InvalidResponse("No message in choice".to_string())
        })?;

        let content = message
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();

        let mut tool_calls = Vec::new();
        if let Some(tcs) = message.get("tool_calls").and_then(|tc| tc.as_array()) {
            for tc in tcs {
                let id = tc
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let function = tc.get("function").unwrap_or(&serde_json::Value::Null);
                let name = function
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let arguments_str = function
                    .get("arguments")
                    .and_then(|v| v.as_str())
                    .unwrap_or("{}");
                let arguments: serde_json::Value =
                    serde_json::from_str(arguments_str).unwrap_or(serde_json::Value::Object(
                        serde_json::Map::new(),
                    ));
                tool_calls.push(ToolCall {
                    id,
                    name,
                    arguments,
                });
            }
        }

        let usage = body.get("usage");
        let prompt_tokens = usage
            .and_then(|u| u.get("prompt_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let completion_tokens = usage
            .and_then(|u| u.get("completion_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;

        let finish_reason = choice
            .get("finish_reason")
            .and_then(|v| v.as_str())
            .unwrap_or("stop");

        let fr = match finish_reason {
            "stop" => FinishReason::Complete,
            "tool_calls" => FinishReason::ToolCall,
            "length" => FinishReason::TokenLimit,
            _ => FinishReason::Complete,
        };

        Ok(InferenceResponse {
            content,
            tool_calls,
            tokens_used: TokenUsage {
                prompt_tokens,
                completion_tokens,
                total_tokens: prompt_tokens + completion_tokens,
            },
            finish_reason: fr,
        })
    }
}

#[async_trait]
impl Provider for OpenRouterProvider {
    async fn infer(&self, request: InferenceRequest) -> Result<InferenceResponse> {
        tracing::debug!(model = %request.model, "Calling OpenRouter API");

        if let Some(limiter) = &self.rate_limiter {
            limiter.acquire().await?;
        }

        let body = self.build_request_body(&request);

        let response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("HTTP-Referer", "https://leviath.dev")
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::RequestFailed(e.to_string()))?;

        let status = response.status();

        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            if let Some(limiter) = &self.rate_limiter {
                limiter.handle_rate_limit(retry_after).await;
            }
            return Err(ProviderError::RateLimitExceeded);
        }

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

        if let Some(limiter) = &self.rate_limiter {
            limiter.reset_backoff().await;
        }

        let response_body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| ProviderError::InvalidResponse(e.to_string()))?;

        let result = self.parse_response(&response_body)?;

        if let Some(limiter) = &self.rate_limiter {
            limiter
                .record_tokens(result.tokens_used.total_tokens)
                .await;
        }

        Ok(result)
    }

    async fn infer_stream(
        &self,
        request: InferenceRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>> {
        tracing::debug!(model = %request.model, "Calling OpenRouter API (streaming)");

        if let Some(limiter) = &self.rate_limiter {
            limiter.acquire().await?;
        }

        let mut body = self.build_request_body(&request);
        body["stream"] = serde_json::Value::Bool(true);

        let response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("HTTP-Referer", "https://leviath.dev")
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::RequestFailed(e.to_string()))?;

        let status = response.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            if let Some(limiter) = &self.rate_limiter {
                limiter.handle_rate_limit(retry_after).await;
            }
            return Err(ProviderError::RateLimitExceeded);
        }

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

        if let Some(limiter) = &self.rate_limiter {
            limiter.reset_backoff().await;
        }

        // Reuse OpenAI SSE parser since the format is identical
        let byte_stream = response.bytes_stream();
        let stream = OpenAiSseStream::new(byte_stream);

        Ok(Box::pin(stream))
    }

    fn count_tokens(&self, text: &str, _model: &str) -> usize {
        // Approximate counting (provider-specific tokenizers not available)
        text.len() / 4
    }

    fn max_context_tokens(&self, _model: &str) -> usize {
        128_000
    }

    fn name(&self) -> &str {
        "openrouter"
    }

    fn capabilities(&self, model: &str) -> ModelCapabilities {
        if let Some(overrides) = self.capability_overrides.get(model) {
            return overrides.clone();
        }
        // ── Google Gemini ─────────────────────────────────────────────────────
        if model.starts_with("google/gemini") {
            return ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 1_048_576,
                max_output_tokens: 65_536,
            };
        }
        // ── Meta Llama 4 Scout — 10M context ─────────────────────────────────
        if model.contains("llama-4-scout") {
            return ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 10_000_000,
                max_output_tokens: 32_768,
            };
        }
        // ── Meta Llama 4 (Maverick + others) — 1M context ────────────────────
        if model.starts_with("meta-llama/llama-4") {
            return ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 1_048_576,
                max_output_tokens: 32_768,
            };
        }
        // ── DeepSeek R1 — reasoning-only, no tools, no temperature ───────────
        if model.contains("deepseek-r1") {
            return ModelCapabilities {
                supports_temperature: false,
                supports_streaming: true,
                supports_tools: false,
                supports_system_prompt: true,
                max_context_tokens: 163_840,
                max_output_tokens: 32_768,
            };
        }
        // ── DeepSeek V4 Pro — 1M context, 384K output ────────────────────────
        if model.contains("deepseek-v4-pro") {
            return ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 1_048_576,
                max_output_tokens: 393_216,
            };
        }
        // ── DeepSeek V4 Flash / V3.x ─────────────────────────────────────────
        if model.starts_with("deepseek/deepseek-v") {
            return ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 1_048_576,
                max_output_tokens: 65_536,
            };
        }
        // ── Mistral Large ─────────────────────────────────────────────────────
        if model.contains("mistral-large") {
            return ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 262_144,
                max_output_tokens: 32_768,
            };
        }
        // ── Mistral Medium / Small ────────────────────────────────────────────
        if model.starts_with("mistralai/") {
            return ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 131_072,
                max_output_tokens: 32_768,
            };
        }
        // ── Qwen 3.6+ / Qwen3 Coder — 1M context ────────────────────────────
        if model.contains("qwen3.6") || model.contains("qwen3-coder") {
            return ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 1_048_576,
                max_output_tokens: 65_536,
            };
        }
        // ── Qwen3 general ─────────────────────────────────────────────────────
        if model.starts_with("qwen/") {
            return ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 131_072,
                max_output_tokens: 32_768,
            };
        }
        // ── Anthropic models via OpenRouter — inherit direct-provider flags ───
        let anthropic_no_temp = model.contains("claude-opus-4-8")
            || model.contains("claude-opus-4-7")
            || model.contains("claude-fable-5")
            || model.contains("claude-mythos-5");
        if model.starts_with("anthropic/") {
            return ModelCapabilities {
                supports_temperature: !anthropic_no_temp,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 1_000_000,
                max_output_tokens: 128_000,
            };
        }
        // ── OpenAI o-series via OpenRouter — no temperature ───────────────────
        if model.starts_with("openai/o") {
            return ModelCapabilities {
                supports_temperature: false,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 200_000,
                max_output_tokens: 100_000,
            };
        }
        // ── OpenAI GPT-5.x via OpenRouter ────────────────────────────────────
        if model.starts_with("openai/gpt-5") {
            return ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 1_050_000,
                max_output_tokens: 128_000,
            };
        }
        // ── Conservative fallback for unknown OpenRouter models ───────────────
        ModelCapabilities {
            supports_temperature: true,
            supports_streaming: true,
            supports_tools: true,
            supports_system_prompt: true,
            max_context_tokens: 128_000,
            max_output_tokens: 8_192,
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

        let data = body
            .get("data")
            .and_then(|d| d.as_array())
            .ok_or_else(|| ProviderError::InvalidResponse("Missing 'data' array".to_string()))?;

        let mut models = Vec::with_capacity(data.len());
        for entry in data {
            let id = entry
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let name = entry
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let context_length = entry
                .get("context_length")
                .and_then(|v| v.as_u64())
                .unwrap_or(128_000) as usize;
            let max_completion_tokens = entry
                .get("top_provider")
                .and_then(|tp| tp.get("max_completion_tokens"))
                .and_then(|v| v.as_u64())
                .map(|v| v as usize);

            let base_caps = self.capabilities(&id);
            let capabilities = ModelCapabilities {
                max_context_tokens: context_length,
                max_output_tokens: max_completion_tokens.unwrap_or(8192),
                ..base_caps
            };

            models.push(ModelInfo {
                id,
                display_name: name,
                provider: "openrouter".into(),
                capabilities,
            });
        }

        Ok(models)
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

    #[test]
    fn test_build_request_body() {
        let provider = OpenRouterProvider::new("test-key".to_string());
        let request = InferenceRequest {
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Hello".to_string(),
            }],
            model: "anthropic/claude-sonnet-4".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
        };

        let body = provider.build_request_body(&request);
        assert_eq!(body["model"], "anthropic/claude-sonnet-4");
    }
}
