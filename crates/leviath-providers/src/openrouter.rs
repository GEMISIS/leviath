//! OpenRouter provider implementation.
//!
//! OpenRouter provides access to multiple models through a unified API.
//! Uses OpenAI-compatible format with additional headers.

use crate::openai_compat::{OpenAiSseStream, parse_openai_response, send_chat_request};
use crate::provider::{
    InferenceRequest, InferenceResponse, ModelCapabilities, ModelInfo, Provider, ProviderConfig,
    ProviderError, Result, StreamChunk,
};
use crate::rate_limit::RateLimiter;
use async_trait::async_trait;
use futures_core::Stream;
use std::collections::HashMap;
use std::pin::Pin;

/// OpenRouter provider.
pub struct OpenRouterProvider {
    /// HTTP client
    client: reqwest::Client,

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
            client: crate::provider::build_http_client(None),
            api_key,
            base_url: "https://openrouter.ai/api/v1".to_string(),
            rate_limiter: None,
            capability_overrides: HashMap::new(),
        }
    }

    /// Create a new OpenRouter provider with full configuration.
    pub fn with_config(config: ProviderConfig) -> Self {
        let rate_limiter = config.rate_limit.as_ref().map(RateLimiter::new);
        let client = crate::provider::build_http_client(config.request_timeout_secs);
        Self {
            client,
            api_key: config.api_key,
            base_url: config
                .base_url
                .unwrap_or_else(|| "https://openrouter.ai/api/v1".to_string()),
            rate_limiter,
            capability_overrides: HashMap::new(),
        }
    }

    /// Create a new OpenRouter provider with per-model capability overrides.
    pub fn with_overrides(
        api_key: String,
        overrides: HashMap<String, ModelCapabilities>,
        timeout_secs: Option<u64>,
        rate_limit: Option<&crate::provider::RateLimitConfig>,
    ) -> Self {
        Self {
            client: crate::provider::build_http_client(timeout_secs),
            api_key,
            base_url: "https://openrouter.ai/api/v1".to_string(),
            rate_limiter: rate_limit.map(crate::rate_limit::RateLimiter::new),
            capability_overrides: overrides,
        }
    }

    /// Build the request body (OpenAI-compatible format).
    ///
    /// For Anthropic models (detected by `claude` in name), pass through
    /// cache breakpoint markers as content-block cache_control annotations.
    fn build_request_body(&self, request: &InferenceRequest) -> serde_json::Value {
        let is_anthropic = request.model.contains("claude");
        let mut breakpoint_count = 0usize;

        let mut messages: Vec<serde_json::Value> = Vec::new();
        // System blocks go first; dropping them silently loses the system prompt.
        for block in &request.system {
            messages.push(serde_json::json!({ "role": "system", "content": block.text }));
        }
        for msg in &request.messages {
            match &msg.content {
                // A cache-breakpointed text turn (Anthropic-via-OpenRouter) keeps
                // its ephemeral cache_control wrapper.
                crate::provider::MessageContent::Text(text)
                    if is_anthropic && msg.cache_breakpoint && breakpoint_count < 4 =>
                {
                    breakpoint_count += 1;
                    messages.push(serde_json::json!({
                        "role": msg.role,
                        "content": [{
                            "type": "text",
                            "text": text,
                            "cache_control": { "type": "ephemeral" }
                        }],
                    }));
                }
                // Everything else - including tool_use/tool_result block history -
                // goes through the OpenAI-format conversion so it round-trips on
                // non-Anthropic models instead of being sent as raw block JSON.
                _ => messages.extend(crate::openai_compat::message_to_openai(
                    &msg.role,
                    &msg.content,
                )),
            }
        }

        let caps = self.capabilities(&request.model);
        let mut body = if caps.supports_temperature {
            serde_json::json!({
                "model": request.model,
                "max_tokens": request.max_tokens,
                "temperature": request.temperature,
                "messages": messages,
            })
        } else {
            serde_json::json!({
                "model": request.model,
                "max_tokens": request.max_tokens,
                "messages": messages,
            })
        };

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

        // Pass through extra model parameters (top_p, stop, seed, …).
        crate::openai_compat::merge_extra_params(
            body.as_object_mut()
                .expect("an OpenRouter request body is always a JSON object"),
            &request.extra,
        );
        body
    }
}

#[async_trait]
impl Provider for OpenRouterProvider {
    async fn infer(&self, request: &InferenceRequest) -> Result<InferenceResponse> {
        tracing::debug!(model = %request.model, "Calling OpenRouter API");

        if let Some(limiter) = &self.rate_limiter {
            limiter.acquire().await?;
        }

        let body = self.build_request_body(request);
        let url = format!("{}/chat/completions", self.base_url);

        let response = send_chat_request(
            &self.client,
            "openrouter",
            &url,
            &[
                ("Authorization", format!("Bearer {}", self.api_key)),
                // OpenRouter attributes a request to an app by this pair, and
                // sending only the referer left every Leviath call unnamed on
                // the account's activity page.
                ("HTTP-Referer", "https://leviath.dev".to_string()),
                ("X-Title", "Leviath".to_string()),
                ("Content-Type", "application/json".to_string()),
            ],
            &body,
            self.rate_limiter.as_ref(),
            request.request_timeout_secs,
        )
        .await?;

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
        request: &InferenceRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>> {
        tracing::debug!(model = %request.model, "Calling OpenRouter API (streaming)");

        if let Some(limiter) = &self.rate_limiter {
            limiter.acquire().await?;
        }

        let mut body = self.build_request_body(request);
        body["stream"] = serde_json::Value::Bool(true);
        let url = format!("{}/chat/completions", self.base_url);

        let response = send_chat_request(
            &self.client,
            "openrouter",
            &url,
            &[
                ("Authorization", format!("Bearer {}", self.api_key)),
                // OpenRouter attributes a request to an app by this pair, and
                // sending only the referer left every Leviath call unnamed on
                // the account's activity page.
                ("HTTP-Referer", "https://leviath.dev".to_string()),
                ("X-Title", "Leviath".to_string()),
                ("Content-Type", "application/json".to_string()),
            ],
            &body,
            self.rate_limiter.as_ref(),
            request.request_timeout_secs,
        )
        .await?;

        // Reuse OpenAI SSE parser since the format is identical
        let byte_stream = response.bytes_stream();
        let stream = OpenAiSseStream::new(byte_stream);

        Ok(Box::pin(stream))
    }

    async fn count_tokens(&self, text: &str, _model: &str) -> usize {
        // OpenRouter fronts many models and exposes no token-count endpoint;
        // approximate locally (provider-specific tokenizers not available).
        leviath_core::estimate_tokens(text)
    }

    fn max_context_tokens(&self, model: &str) -> usize {
        // Delegate to the per-model capabilities table rather than a flat 128K,
        // which badly under-budgeted large-context models (Llama-4 ~10M, several
        // ~1M) and over-budgeted small ones.
        self.capabilities(model).max_context_tokens
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
        // ── Meta Llama 4 Scout - 10M context ─────────────────────────────────
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
        // ── Meta Llama 4 (Maverick + others) - 1M context ────────────────────
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
        // ── DeepSeek R1 - reasoning-only, no tools, no temperature ───────────
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
        // ── DeepSeek V4 Pro - 1M context, 384K output ────────────────────────
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
        // ── Qwen 3.6+ / Qwen3 Coder - 1M context ────────────────────────────
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
        // ── Anthropic models via OpenRouter - inherit direct-provider flags ───
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
        // ── OpenAI o-series via OpenRouter - no temperature ───────────────────
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
    use crate::test_support::always_on_tracing_guard;
    use leviath_testkit::{spawn_mock_server, spawn_mock_server_truncated_body};

    #[test]
    fn test_provider_creation() {
        let provider = OpenRouterProvider::new("test-key".to_string());
        assert_eq!(provider.name(), "openrouter");
    }

    #[test]
    fn test_build_request_body() {
        let provider = OpenRouterProvider::new("test-key".to_string());
        let request = InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Hello".into(),
                cache_breakpoint: false,
            }],
            model: "anthropic/claude-sonnet-4".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        assert_eq!(body["model"], "anthropic/claude-sonnet-4");
    }

    #[test]
    fn test_build_request_body_passes_through_extra_params() {
        let provider = OpenRouterProvider::new("test-key".to_string());
        let request = InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Hello".into(),
                cache_breakpoint: false,
            }],
            model: "openai/gpt-4o".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::json!({ "top_p": 0.9, "seed": 3 }),
            request_timeout_secs: None,
        };
        let body = provider.build_request_body(&request);
        assert_eq!(body["top_p"], serde_json::json!(0.9));
        assert_eq!(body["seed"], serde_json::json!(3));
    }

    #[test]
    fn test_build_request_body_prepends_system_blocks() {
        let provider = OpenRouterProvider::new("test-key".to_string());
        let request = InferenceRequest {
            system: vec![crate::provider::SystemBlock {
                text: "You are a helpful assistant.".to_string(),
                cache_hint: leviath_core::CacheHint::Never,
            }],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Hello".into(),
                cache_breakpoint: false,
            }],
            model: "openai/gpt-4o".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        let messages = body["messages"].as_array().unwrap();
        // System block is delivered as the first message, not dropped.
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "You are a helpful assistant.");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "Hello");
    }

    #[test]
    fn test_build_request_body_serializes_tool_history() {
        let provider = OpenRouterProvider::new("test-key".to_string());
        let request = InferenceRequest {
            system: vec![],
            messages: vec![
                crate::provider::Message {
                    role: "assistant".to_string(),
                    content: crate::provider::MessageContent::Blocks(vec![
                        crate::provider::ContentBlock::ToolUse {
                            id: "call_1".to_string(),
                            name: "get_weather".to_string(),
                            input: serde_json::json!({ "city": "Paris" }),
                            thought_signature: None,
                        },
                    ]),
                    cache_breakpoint: false,
                },
                crate::provider::Message {
                    role: "user".to_string(),
                    content: crate::provider::MessageContent::Blocks(vec![
                        crate::provider::ContentBlock::ToolResult {
                            tool_use_id: "call_1".to_string(),
                            content: "sunny".to_string(),
                            is_error: false,
                        },
                    ]),
                    cache_breakpoint: false,
                },
            ],
            model: "openai/gpt-4o".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        let messages = body["messages"].as_array().unwrap();
        // Tool call → assistant message with an OpenAI `tool_calls` array.
        assert_eq!(messages[0]["role"], "assistant");
        assert_eq!(messages[0]["tool_calls"][0]["id"], "call_1");
        assert_eq!(
            messages[0]["tool_calls"][0]["function"]["name"],
            "get_weather"
        );
        // Tool result → a `tool`-role message keyed by the call id.
        assert_eq!(messages[1]["role"], "tool");
        assert_eq!(messages[1]["tool_call_id"], "call_1");
        assert_eq!(messages[1]["content"], "sunny");
    }

    // ── Additional coverage tests ──────────────────────────────────────────

    #[test]
    fn test_name() {
        let provider = OpenRouterProvider::new("key".to_string());
        assert_eq!(provider.name(), "openrouter");
    }

    #[tokio::test]
    async fn test_count_tokens() {
        let provider = OpenRouterProvider::new("key".to_string());
        let tokens = provider.count_tokens("Hello, world!", "any-model").await;
        assert_eq!(tokens, 4); // ceil(13 / 4): the shared estimate rounds up
    }

    #[tokio::test]
    async fn test_count_tokens_empty() {
        let provider = OpenRouterProvider::new("key".to_string());
        assert_eq!(provider.count_tokens("", "any-model").await, 0);
    }

    #[test]
    fn test_max_context_tokens() {
        let provider = OpenRouterProvider::new("key".to_string());
        // Unknown model ⇒ the conservative fallback.
        assert_eq!(provider.max_context_tokens("any-model"), 128_000);
        // A known large-context model reports its real size (not a flat 128K) -
        // it now delegates to the per-model capabilities table.
        assert_eq!(
            provider.max_context_tokens("meta-llama/llama-4-scout"),
            provider
                .capabilities("meta-llama/llama-4-scout")
                .max_context_tokens
        );
        assert!(provider.max_context_tokens("meta-llama/llama-4-scout") > 128_000);
    }

    #[test]
    fn test_with_config_default_url() {
        let config = ProviderConfig {
            api_key: "key".to_string(),
            base_url: None,
            rate_limit: None,
            request_timeout_secs: None,
        };
        let provider = OpenRouterProvider::with_config(config);
        assert_eq!(provider.base_url, "https://openrouter.ai/api/v1");
    }

    #[test]
    fn test_with_config_custom_url() {
        let config = ProviderConfig {
            api_key: "key".to_string(),
            base_url: Some("https://custom.openrouter.ai".to_string()),
            rate_limit: None,
            request_timeout_secs: None,
        };
        let provider = OpenRouterProvider::with_config(config);
        assert_eq!(provider.base_url, "https://custom.openrouter.ai");
    }

    #[test]
    fn test_with_overrides() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "custom/model".to_string(),
            ModelCapabilities {
                supports_temperature: false,
                supports_streaming: false,
                supports_tools: false,
                supports_system_prompt: false,
                max_context_tokens: 99,
                max_output_tokens: 10,
            },
        );
        let provider = OpenRouterProvider::with_overrides("key".to_string(), overrides, None, None);
        let caps = provider.capabilities("custom/model");
        assert_eq!(caps.max_context_tokens, 99);
    }

    #[test]
    fn test_capabilities_google_gemini() {
        let provider = OpenRouterProvider::new("key".to_string());
        let caps = provider.capabilities("google/gemini-3.5-flash");
        assert!(caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 1_048_576);
        assert_eq!(caps.max_output_tokens, 65_536);
    }

    #[test]
    fn test_capabilities_llama4_scout() {
        let provider = OpenRouterProvider::new("key".to_string());
        let caps = provider.capabilities("meta-llama/llama-4-scout-17b");
        assert_eq!(caps.max_context_tokens, 10_000_000);
    }

    #[test]
    fn test_capabilities_llama4_maverick() {
        let provider = OpenRouterProvider::new("key".to_string());
        let caps = provider.capabilities("meta-llama/llama-4-maverick");
        assert_eq!(caps.max_context_tokens, 1_048_576);
    }

    #[test]
    fn test_capabilities_deepseek_r1() {
        let provider = OpenRouterProvider::new("key".to_string());
        let caps = provider.capabilities("deepseek/deepseek-r1");
        assert!(!caps.supports_temperature);
        assert!(!caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 163_840);
    }

    #[test]
    fn test_capabilities_deepseek_v4_pro() {
        let provider = OpenRouterProvider::new("key".to_string());
        let caps = provider.capabilities("deepseek/deepseek-v4-pro");
        assert_eq!(caps.max_context_tokens, 1_048_576);
        assert_eq!(caps.max_output_tokens, 393_216);
    }

    #[test]
    fn test_capabilities_deepseek_v_series() {
        let provider = OpenRouterProvider::new("key".to_string());
        let caps = provider.capabilities("deepseek/deepseek-v3");
        assert_eq!(caps.max_context_tokens, 1_048_576);
        assert_eq!(caps.max_output_tokens, 65_536);
    }

    #[test]
    fn test_capabilities_mistral_large() {
        let provider = OpenRouterProvider::new("key".to_string());
        let caps = provider.capabilities("mistralai/mistral-large-latest");
        assert_eq!(caps.max_context_tokens, 262_144);
    }

    #[test]
    fn test_capabilities_mistralai_general() {
        let provider = OpenRouterProvider::new("key".to_string());
        let caps = provider.capabilities("mistralai/mistral-small-latest");
        assert_eq!(caps.max_context_tokens, 131_072);
    }

    #[test]
    fn test_capabilities_qwen36() {
        let provider = OpenRouterProvider::new("key".to_string());
        let caps = provider.capabilities("qwen/qwen3.6-235b");
        assert_eq!(caps.max_context_tokens, 1_048_576);
    }

    #[test]
    fn test_capabilities_qwen3_coder() {
        let provider = OpenRouterProvider::new("key".to_string());
        let caps = provider.capabilities("qwen/qwen3-coder-plus");
        assert_eq!(caps.max_context_tokens, 1_048_576);
    }

    #[test]
    fn test_capabilities_qwen_general() {
        let provider = OpenRouterProvider::new("key".to_string());
        let caps = provider.capabilities("qwen/qwen3-32b");
        assert_eq!(caps.max_context_tokens, 131_072);
    }

    #[test]
    fn test_capabilities_anthropic_via_openrouter() {
        let provider = OpenRouterProvider::new("key".to_string());
        let caps = provider.capabilities("anthropic/claude-sonnet-4-6");
        assert!(caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 1_000_000);
        assert_eq!(caps.max_output_tokens, 128_000);
    }

    #[test]
    fn test_capabilities_anthropic_no_temp() {
        let provider = OpenRouterProvider::new("key".to_string());
        let caps = provider.capabilities("anthropic/claude-opus-4-8");
        assert!(!caps.supports_temperature);
    }

    #[test]
    fn test_capabilities_anthropic_fable5_no_temp() {
        let provider = OpenRouterProvider::new("key".to_string());
        let caps = provider.capabilities("anthropic/claude-fable-5");
        assert!(!caps.supports_temperature);
    }

    #[test]
    fn test_capabilities_openai_o_series() {
        let provider = OpenRouterProvider::new("key".to_string());
        let caps = provider.capabilities("openai/o3-mini");
        assert!(!caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 200_000);
    }

    #[test]
    fn test_capabilities_openai_gpt5() {
        let provider = OpenRouterProvider::new("key".to_string());
        let caps = provider.capabilities("openai/gpt-5.4-mini");
        assert!(caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 1_050_000);
    }

    #[test]
    fn test_capabilities_unknown_fallback() {
        let provider = OpenRouterProvider::new("key".to_string());
        let caps = provider.capabilities("totally/unknown-model");
        assert!(caps.supports_temperature);
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 128_000);
        assert_eq!(caps.max_output_tokens, 8_192);
    }

    #[test]
    fn test_build_request_body_anthropic_cache_breakpoint() {
        let provider = OpenRouterProvider::new("key".to_string());
        let request = InferenceRequest {
            system: vec![],
            messages: vec![
                crate::provider::Message {
                    role: "user".to_string(),
                    content: "First".into(),
                    cache_breakpoint: true,
                },
                crate::provider::Message {
                    role: "user".to_string(),
                    content: "Second".into(),
                    cache_breakpoint: false,
                },
            ],
            model: "anthropic/claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        let msgs = body["messages"].as_array().unwrap();
        // First message should have cache_control in content block (anthropic model)
        assert!(msgs[0]["content"].is_array());
        assert_eq!(msgs[0]["content"][0]["cache_control"]["type"], "ephemeral");
        // Second message should be simple string content
        assert!(msgs[1]["content"].is_string());
    }

    #[test]
    fn test_build_request_body_non_anthropic_no_cache_breakpoint() {
        let provider = OpenRouterProvider::new("key".to_string());
        let request = InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Hello".into(),
                cache_breakpoint: true,
            }],
            model: "openai/gpt-5.4-mini".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        let msgs = body["messages"].as_array().unwrap();
        // Non-anthropic model should not get cache_control blocks
        assert!(msgs[0]["content"].is_string());
    }

    #[test]
    fn test_build_request_body_max_4_breakpoints() {
        let provider = OpenRouterProvider::new("key".to_string());
        let messages: Vec<crate::provider::Message> = (0..6)
            .map(|i| crate::provider::Message {
                role: "user".to_string(),
                content: format!("msg {}", i).into(),
                cache_breakpoint: true,
            })
            .collect();

        let request = InferenceRequest {
            system: vec![],
            messages,
            model: "anthropic/claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        let msgs = body["messages"].as_array().unwrap();
        let bp_count = msgs.iter().filter(|m| m["content"].is_array()).count();
        assert_eq!(bp_count, 4);
    }

    #[test]
    fn test_build_request_body_with_tools() {
        let provider = OpenRouterProvider::new("key".to_string());
        let request = InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Search".into(),
                cache_breakpoint: false,
            }],
            model: "openai/gpt-5.4-mini".to_string(),
            max_tokens: 512,
            temperature: 0.5,
            tools: vec![crate::provider::Tool {
                name: "search".to_string(),
                description: "Search the web".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            }],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["function"]["name"], "search");
    }

    #[test]
    fn test_build_request_body_no_temp_for_deepseek_r1() {
        let provider = OpenRouterProvider::new("key".to_string());
        let request = InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Think".into(),
                cache_breakpoint: false,
            }],
            model: "deepseek/deepseek-r1".to_string(),
            max_tokens: 512,
            temperature: 0.5,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        // deepseek-r1 doesn't support temperature
        assert!(body.get("temperature").is_none());
    }

    // ─── HTTP-call-level tests via a raw-TCP mock server ───────────────────

    fn provider_with_url(url: String) -> OpenRouterProvider {
        OpenRouterProvider::with_config(ProviderConfig {
            api_key: "test-key".to_string(),
            base_url: Some(url),
            rate_limit: None,
            request_timeout_secs: None,
        })
    }

    fn simple_request() -> InferenceRequest {
        InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "hi".into(),
                cache_breakpoint: false,
            }],
            model: "openai/gpt-4o".to_string(),
            max_tokens: 100,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        }
    }

    #[tokio::test]
    async fn infer_success_parses_response() {
        // Registers a real Subscriber so the tracing::debug! call's field
        // arguments at the top of infer() are actually exercised.
        let _guard = always_on_tracing_guard();
        let body = br#"{"choices":[{"message":{"content":"hi there"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2}}"#;
        let url = spawn_mock_server(200, "OK", body).await;
        let provider = provider_with_url(url);
        let resp = provider.infer(&simple_request()).await.unwrap();
        assert_eq!(resp.content, "hi there");
    }

    // ─── HTTP error paths (connection refused) ─────────────────────────────

    #[tokio::test]
    async fn infer_connection_refused_returns_error() {
        let provider = provider_with_url("http://127.0.0.1:19997".to_string());
        let err = provider.infer(&simple_request()).await.unwrap_err();
        assert!(err.to_string().contains("Request failed:"));
    }

    #[tokio::test]
    async fn infer_stream_connection_refused_returns_error() {
        let provider = provider_with_url("http://127.0.0.1:19997".to_string());
        let result = provider.infer_stream(&simple_request()).await;
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("Request failed:")
        );
    }

    #[tokio::test]
    async fn list_models_connection_refused_returns_error() {
        let provider = provider_with_url("http://127.0.0.1:19997".to_string());
        let err = provider.list_models().await.unwrap_err();
        assert!(err.to_string().contains("Request failed:"));
    }

    #[tokio::test]
    async fn list_models_non_success_body_read_error_falls_back_to_unknown_error() {
        let url = spawn_mock_server_truncated_body(500, "Internal Server Error").await;
        let provider = provider_with_url(url);
        let err = provider.list_models().await.unwrap_err();
        assert!(err.to_string().contains("unknown error"));
    }

    #[tokio::test]
    async fn infer_non_success_status_returns_api_error() {
        let url = spawn_mock_server(500, "Internal Server Error", b"boom").await;
        let provider = provider_with_url(url);
        let err = provider.infer(&simple_request()).await.unwrap_err();
        assert!(err.to_string().contains("API error:"));
    }

    #[tokio::test]
    async fn infer_malformed_json_returns_invalid_response() {
        let url = spawn_mock_server(200, "OK", b"not json").await;
        let provider = provider_with_url(url);
        let err = provider.infer(&simple_request()).await.unwrap_err();
        assert!(err.to_string().contains("Invalid response:"));
    }

    #[tokio::test]
    async fn infer_stream_non_success_status_returns_api_error() {
        let url = spawn_mock_server(503, "Service Unavailable", b"down").await;
        let provider = provider_with_url(url);
        let result = provider.infer_stream(&simple_request()).await;
        assert!(result.is_err());
        assert!(result.err().unwrap().to_string().contains("API error:"));
    }

    #[tokio::test]
    async fn infer_stream_success_yields_chunks() {
        // Registers a real Subscriber so the tracing::debug! call's field
        // arguments at the top of infer_stream() are actually exercised.
        let _guard = always_on_tracing_guard();
        let sse_body =
            b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
        let url = spawn_mock_server(200, "OK", sse_body).await;
        let provider = provider_with_url(url);
        let mut stream = provider.infer_stream(&simple_request()).await.unwrap();
        use tokio_stream::StreamExt;
        let chunk = stream.next().await.unwrap().unwrap();
        assert_eq!(chunk.delta, "hi");
    }

    #[tokio::test]
    async fn list_models_success_returns_models() {
        let body = br#"{"data":[{"id":"openai/gpt-4o","name":"GPT-4o","context_length":128000,"top_provider":{"max_completion_tokens":16384}},{"id":"anthropic/claude-3","context_length":200000}]}"#;
        let url = spawn_mock_server(200, "OK", body).await;
        let provider = provider_with_url(url);
        let models = provider.list_models().await.unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "openai/gpt-4o");
        assert_eq!(models[0].display_name, Some("GPT-4o".to_string()));
        assert_eq!(models[0].capabilities.max_output_tokens, 16384);
        assert_eq!(models[1].id, "anthropic/claude-3");
        assert_eq!(models[1].display_name, None);
        assert_eq!(models[1].capabilities.max_output_tokens, 8192);
    }

    #[tokio::test]
    async fn list_models_non_success_status_returns_error() {
        let url = spawn_mock_server(401, "Unauthorized", b"nope").await;
        let provider = provider_with_url(url);
        let err = provider.list_models().await.unwrap_err();
        assert!(err.to_string().contains("API error:"));
    }

    #[tokio::test]
    async fn list_models_malformed_json_returns_error() {
        let url = spawn_mock_server(200, "OK", b"not json").await;
        let provider = provider_with_url(url);
        let err = provider.list_models().await.unwrap_err();
        assert!(err.to_string().contains("Invalid response:"));
    }

    #[tokio::test]
    async fn list_models_missing_data_field_returns_error() {
        let url = spawn_mock_server(200, "OK", b"{}").await;
        let provider = provider_with_url(url);
        let err = provider.list_models().await.unwrap_err();
        assert!(err.to_string().contains("Invalid response:"));
    }

    #[test]
    fn with_overrides_wires_the_rate_limiter() {
        // The daemon path constructs providers exclusively through
        // with_overrides, so a rate limit that stops here is a rate limit
        // nobody gets.
        let cfg = crate::provider::RateLimitConfig {
            requests_per_minute: 5,
            tokens_per_minute: 1_000,
        };
        let limited =
            OpenRouterProvider::with_overrides("k".to_string(), HashMap::new(), None, Some(&cfg));
        assert!(limited.rate_limiter.is_some());
        let unlimited =
            OpenRouterProvider::with_overrides("k".to_string(), HashMap::new(), None, None);
        assert!(unlimited.rate_limiter.is_none());
    }
}
