//! OpenAI provider implementation.

use crate::openai_compat::{
    OpenAiSseStream, TokenLimitField, build_openai_request_body_with, parse_openai_response,
    send_chat_request, tools_refused_over_reasoning_effort,
};
#[cfg(test)]
use crate::provider::FinishReason;
use crate::provider::{
    InferenceRequest, InferenceResponse, ModelCapabilities, ModelInfo, Provider, ProviderConfig,
    ProviderError, Result, StreamChunk,
};
use crate::rate_limit::RateLimiter;
use async_trait::async_trait;
use futures_core::Stream;
use std::collections::HashMap;
use std::pin::Pin;

/// OpenAI provider.
pub struct OpenAIProvider {
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

    /// Models the API has refused tools for until told `reasoning_effort:
    /// "none"`, learned from its own error rather than declared up front.
    ///
    /// Remembered so the cost is one extra round trip per model per process,
    /// not one per inference: a run makes many calls and they would each pay it.
    /// A `HashSet` behind a lock rather than a field on the request, because the
    /// provider is shared across every agent talking to it.
    reasoning_effort_none: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
}

impl OpenAIProvider {
    /// Create a new OpenAI provider.
    pub fn new(client: reqwest::Client, api_key: String) -> Self {
        Self {
            client,
            api_key,
            base_url: "https://api.openai.com/v1".to_string(),
            rate_limiter: None,
            capability_overrides: HashMap::new(),
            reasoning_effort_none: Default::default(),
        }
    }

    /// Create a new OpenAI provider with full configuration.
    pub fn with_config(client: reqwest::Client, config: ProviderConfig) -> Self {
        let rate_limiter = config.rate_limit.as_ref().map(RateLimiter::new);
        Self {
            client,
            api_key: config.api_key,
            base_url: config
                .base_url
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
            rate_limiter,
            capability_overrides: HashMap::new(),
            reasoning_effort_none: Default::default(),
        }
    }

    /// Create a new OpenAI provider with per-model capability overrides.
    pub fn with_overrides(
        client: reqwest::Client,
        api_key: String,
        overrides: HashMap<String, ModelCapabilities>,
        rate_limit: Option<&crate::provider::RateLimitConfig>,
    ) -> Self {
        Self {
            client,
            api_key,
            base_url: "https://api.openai.com/v1".to_string(),
            rate_limiter: rate_limit.map(crate::rate_limit::RateLimiter::new),
            capability_overrides: overrides,
            reasoning_effort_none: Default::default(),
        }
    }

    /// Return built-in capability defaults for a model.
    fn builtin_capabilities(&self, model: &str) -> ModelCapabilities {
        // GPT-5.5 - flagship, 1M+ context, 128K output (check before generic gpt-5)
        if model.starts_with("gpt-5.5") {
            ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 1_050_000,
                max_output_tokens: 128_000,
            }
        // GPT-5.x family (5.4, 5.4-mini, 5.4-nano, 5-mini) - 400K context, 128K output
        } else if model.starts_with("gpt-5") {
            ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 400_000,
                max_output_tokens: 128_000,
            }
        // GPT-4.1 family - 1M context (must check before generic gpt-4)
        } else if model.starts_with("gpt-4.1") {
            ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 1_047_576,
                max_output_tokens: 32_768,
            }
        // o-series reasoning models (o3, o4) - no temperature, 200K context
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

    /// POST a chat-completions body, teaching the retry described on
    /// [`tools_refused_over_reasoning_effort`].
    ///
    /// Shared by both entry points so streaming and non-streaming cannot learn
    /// different things about the same model.
    async fn post_chat(
        &self,
        request: &InferenceRequest,
        mut body: serde_json::Value,
    ) -> Result<reqwest::Response> {
        let url = format!("{}/chat/completions", self.base_url);
        let headers = [
            ("Authorization", format!("Bearer {}", self.api_key)),
            ("Content-Type", "application/json".to_string()),
        ];

        // A model that takes no temperature is sent none, rather than being sent
        // zero. `build_openai_request_body_with` always writes the key and the
        // runtime substitutes `0.0` where a model declares no support, but "not
        // supported" is not a value: the o-series accepts only its default and
        // rejects `0.0` exactly as firmly as `0.7`, so the one flag that exists
        // to protect these models was what broke them. Omitting is what the
        // OpenRouter provider has always done for the same models.
        if !self.capabilities(&request.model).supports_temperature
            && let Some(fields) = body.as_object_mut()
        {
            fields.remove("temperature");
        }

        // Already learned for this model: pay nothing and send it up front.
        if self.needs_reasoning_effort_none(&request.model) {
            set_reasoning_effort_none(&mut body);
        }
        // A caller who set `reasoning_effort` themselves (via the manifest's
        // `[model.parameters]`) has said what they want. Overriding it, or
        // retrying to override it, would quietly ignore them - so the retry is
        // only for a body that never mentioned the field.
        let ours_to_set = body.get("reasoning_effort").is_none();

        let sent = send_chat_request(
            &self.client,
            "openai",
            &url,
            &headers,
            &body,
            self.rate_limiter.as_ref(),
            request.request_timeout_secs,
        )
        .await;

        match sent {
            Err(ProviderError::ApiError(detail))
                if ours_to_set && tools_refused_over_reasoning_effort(&detail) =>
            {
                tracing::debug!(
                    model = %request.model,
                    "OpenAI refused tools alongside a reasoning effort; retrying with none"
                );
                self.remember_reasoning_effort_none(&request.model);
                set_reasoning_effort_none(&mut body);
                send_chat_request(
                    &self.client,
                    "openai",
                    &url,
                    &headers,
                    &body,
                    self.rate_limiter.as_ref(),
                    request.request_timeout_secs,
                )
                .await
            }
            other => other,
        }
    }

    /// Whether this model has already refused tools over a reasoning effort.
    fn needs_reasoning_effort_none(&self, model: &str) -> bool {
        leviath_core::sync::lock(&self.reasoning_effort_none).contains(model)
    }

    /// Record that it did, for the rest of this process.
    fn remember_reasoning_effort_none(&self, model: &str) {
        leviath_core::sync::lock(&self.reasoning_effort_none).insert(model.to_string());
    }
}

/// Say "no reasoning" in the field the API rejected the request over.
fn set_reasoning_effort_none(body: &mut serde_json::Value) {
    body["reasoning_effort"] = serde_json::Value::String("none".to_string());
}

#[async_trait]
impl Provider for OpenAIProvider {
    async fn infer(&self, request: &InferenceRequest) -> Result<InferenceResponse> {
        tracing::debug!(model = %request.model, "Calling OpenAI API");

        if let Some(limiter) = &self.rate_limiter {
            limiter.acquire().await?;
        }

        let body = build_openai_request_body_with(request, TokenLimitField::MaxCompletionTokens);
        let response = self.post_chat(request, body).await?;

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
        tracing::debug!(model = %request.model, "Calling OpenAI API (streaming)");

        if let Some(limiter) = &self.rate_limiter {
            limiter.acquire().await?;
        }

        let mut body =
            build_openai_request_body_with(request, TokenLimitField::MaxCompletionTokens);
        body["stream"] = serde_json::Value::Bool(true);
        body["stream_options"] = serde_json::json!({ "include_usage": true });
        let response = self.post_chat(request, body).await?;

        let byte_stream = response.bytes_stream();
        let stream = OpenAiSseStream::new(byte_stream);

        Ok(Box::pin(stream))
    }

    async fn count_tokens(&self, text: &str, model: &str) -> usize {
        // tiktoken is exact for OpenAI models and runs locally - no network call.
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
    use crate::test_support::always_on_tracing_guard;
    use leviath_testkit::{spawn_mock_server, spawn_mock_server_truncated_body};

    #[test]
    fn test_provider_creation() {
        let provider = OpenAIProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        assert_eq!(provider.name(), "openai");
    }

    #[test]
    fn test_context_limits() {
        let provider = OpenAIProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        assert_eq!(provider.max_context_tokens("gpt-5.4-mini"), 400_000);
    }

    #[test]
    fn test_build_request_body() {
        let request = InferenceRequest {
            system: vec![],
            messages: vec![
                crate::provider::Message {
                    role: "system".to_string(),
                    content: "You are helpful.".into(),
                    cache_breakpoint: false,
                },
                crate::provider::Message {
                    role: "user".to_string(),
                    content: "Hello".into(),
                    cache_breakpoint: false,
                },
            ],
            model: "gpt-5.4-mini".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = build_openai_request_body_with(&request, TokenLimitField::MaxCompletionTokens);
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
        assert_eq!(response.finish_reason, FinishReason::Complete);
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
        assert_eq!(response.finish_reason, FinishReason::ToolCall);
    }

    #[test]
    fn test_builtin_capabilities_gpt55() {
        let provider = OpenAIProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let caps = provider.builtin_capabilities("gpt-5.5");
        assert!(caps.supports_temperature);
        assert!(caps.supports_streaming);
        assert_eq!(caps.max_context_tokens, 1_050_000);
        assert_eq!(caps.max_output_tokens, 128_000);
    }

    #[test]
    fn test_builtin_capabilities_gpt54_mini() {
        let provider = OpenAIProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let caps = provider.builtin_capabilities("gpt-5.4-mini");
        assert!(caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 400_000);
        assert_eq!(caps.max_output_tokens, 128_000);
    }

    #[test]
    fn test_builtin_capabilities_gpt54_nano() {
        let provider = OpenAIProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let caps = provider.builtin_capabilities("gpt-5.4-nano");
        assert!(caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 400_000);
        assert_eq!(caps.max_output_tokens, 128_000);
    }

    #[test]
    fn test_builtin_capabilities_gpt41() {
        let provider = OpenAIProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let caps = provider.builtin_capabilities("gpt-4.1");
        assert!(caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 1_047_576);
        assert_eq!(caps.max_output_tokens, 32_768);
    }

    #[test]
    fn test_builtin_capabilities_o4_mini() {
        let provider = OpenAIProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let caps = provider.builtin_capabilities("o4-mini");
        assert!(!caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 200_000);
        assert_eq!(caps.max_output_tokens, 100_000);
    }

    #[test]
    fn test_builtin_capabilities_o3() {
        let provider = OpenAIProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
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
        let provider = OpenAIProvider::with_overrides(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
            overrides,
            None,
        );
        let caps = provider.capabilities("gpt-5.4-mini");
        assert!(!caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 1);
    }

    #[test]
    fn test_parse_response_with_cached_tokens() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "Hello!"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 20,
                "total_tokens": 120,
                "prompt_tokens_details": {
                    "cached_tokens": 80
                }
            }
        });

        let response = parse_openai_response(&body).unwrap();
        assert_eq!(response.tokens_used.prompt_tokens, 100);
        assert_eq!(response.tokens_used.cached_tokens, 80);
        assert_eq!(response.tokens_used.cache_write_tokens, 0);
    }

    #[test]
    fn test_parse_response_without_cached_tokens() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "Hello!"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 20,
                "total_tokens": 120
            }
        });

        let response = parse_openai_response(&body).unwrap();
        assert_eq!(response.tokens_used.cached_tokens, 0);
    }

    // ── Additional coverage tests ──────────────────────────────────────────

    #[test]
    fn test_name() {
        let provider = OpenAIProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        assert_eq!(provider.name(), "openai");
    }

    #[test]
    fn test_with_config_default_url() {
        let config = ProviderConfig {
            api_key: "key".to_string(),
            base_url: None,
            rate_limit: None,
            request_timeout_secs: None,
        };
        let provider = OpenAIProvider::with_config(
            crate::provider::build_http_client(None).expect("a test client builds"),
            config,
        );
        assert_eq!(provider.base_url, "https://api.openai.com/v1");
    }

    #[test]
    fn test_with_config_custom_url() {
        let config = ProviderConfig {
            api_key: "key".to_string(),
            base_url: Some("https://custom.openai.com".to_string()),
            rate_limit: None,
            request_timeout_secs: None,
        };
        let provider = OpenAIProvider::with_config(
            crate::provider::build_http_client(None).expect("a test client builds"),
            config,
        );
        assert_eq!(provider.base_url, "https://custom.openai.com");
    }

    #[tokio::test]
    async fn test_count_tokens_uses_tiktoken() {
        let provider = OpenAIProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let tokens = provider.count_tokens("Hello, world!", "gpt-5.4-mini").await;
        assert!(tokens > 0);
        assert!(tokens < 20);
    }

    #[tokio::test]
    async fn test_count_tokens_empty() {
        let provider = OpenAIProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let tokens = provider.count_tokens("", "gpt-5.4-mini").await;
        assert_eq!(tokens, 0);
    }

    #[test]
    fn test_max_context_tokens_delegates_to_capabilities() {
        let provider = OpenAIProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        assert_eq!(provider.max_context_tokens("gpt-5.5"), 1_050_000);
        assert_eq!(provider.max_context_tokens("gpt-4.1"), 1_047_576);
        assert_eq!(provider.max_context_tokens("o3-mini"), 200_000);
    }

    #[test]
    fn test_builtin_capabilities_unknown_model() {
        let provider = OpenAIProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let caps = provider.builtin_capabilities("totally-unknown");
        let default = ModelCapabilities::default();
        assert_eq!(caps.max_context_tokens, default.max_context_tokens);
    }

    #[test]
    fn test_capabilities_uses_override() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "gpt-5.5".to_string(),
            ModelCapabilities {
                supports_temperature: false,
                supports_streaming: false,
                supports_tools: false,
                supports_system_prompt: false,
                max_context_tokens: 1,
                max_output_tokens: 1,
            },
        );
        let provider = OpenAIProvider::with_overrides(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
            overrides,
            None,
        );
        let caps = provider.capabilities("gpt-5.5");
        assert_eq!(caps.max_context_tokens, 1);
    }

    #[test]
    fn test_parse_response_no_content() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 5,
                "completion_tokens": 0,
                "total_tokens": 5
            }
        });
        let response = parse_openai_response(&body).unwrap();
        assert_eq!(response.content, "");
    }

    #[test]
    fn test_parse_response_finish_reason_length() {
        let body = serde_json::json!({
            "choices": [{
                "message": { "content": "truncated" },
                "finish_reason": "length"
            }],
            "usage": { "prompt_tokens": 5, "completion_tokens": 5, "total_tokens": 10 }
        });
        let response = parse_openai_response(&body).unwrap();
        assert_eq!(response.finish_reason, FinishReason::TokenLimit);
    }

    #[test]
    fn test_gpt5_family_context() {
        let provider = OpenAIProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        // gpt-5.4, gpt-5-mini, etc. should all match gpt-5 pattern
        assert_eq!(provider.max_context_tokens("gpt-5.4"), 400_000);
        assert_eq!(provider.max_context_tokens("gpt-5-mini"), 400_000);
    }

    #[test]
    fn test_o4_mini_capabilities() {
        let provider = OpenAIProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let caps = provider.builtin_capabilities("o4-mini");
        assert!(!caps.supports_temperature);
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 200_000);
        assert_eq!(caps.max_output_tokens, 100_000);
    }

    // ─── HTTP-call-level tests via a raw-TCP mock server ───────────────────

    fn provider_with_url(url: String) -> OpenAIProvider {
        OpenAIProvider::with_config(
            crate::provider::build_http_client(None).expect("a test client builds"),
            ProviderConfig {
                api_key: "test-key".to_string(),
                base_url: Some(url),
                rate_limit: None,
                request_timeout_secs: None,
            },
        )
    }

    fn simple_request() -> InferenceRequest {
        InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "hi".into(),
                cache_breakpoint: false,
            }],
            model: "gpt-5.4".to_string(),
            max_tokens: 100,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        }
    }

    // ─── A model that takes no temperature ──────────────────────────────────

    #[tokio::test]
    async fn a_model_taking_no_temperature_is_sent_none() {
        // o3 declares `supports_temperature: false`, and the runtime turns that
        // into `0.0` - a value it rejects as firmly as any other, since it takes
        // only its own default. Omitting is the only thing that works.
        let (url, bodies) =
            leviath_testkit::spawn_mock_sequence(vec![(200, "OK", OK_BODY.to_vec())]).await;
        let provider = provider_with_url(url);
        let request = InferenceRequest {
            model: "o3".to_string(),
            ..simple_request()
        };
        provider.infer(&request).await.unwrap();

        let sent = bodies.lock().expect("recorder").clone();
        let body = &sent[0];
        assert!(!body.contains("temperature"), "{body}");
    }

    #[tokio::test]
    async fn a_model_taking_a_temperature_still_gets_one() {
        // The other half, so "omit it" cannot quietly become "omit it always".
        let (url, bodies) =
            leviath_testkit::spawn_mock_sequence(vec![(200, "OK", OK_BODY.to_vec())]).await;
        let provider = provider_with_url(url);
        let request = InferenceRequest {
            model: "gpt-4o".to_string(),
            temperature: 0.5,
            ..simple_request()
        };
        provider.infer(&request).await.unwrap();

        let sent = bodies.lock().expect("recorder").clone();
        let body = &sent[0];
        assert!(body.contains(r#""temperature":0.5"#), "{body}");
    }

    #[tokio::test]
    async fn streaming_omits_it_too() {
        let sse = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
        let (url, bodies) =
            leviath_testkit::spawn_mock_sequence(vec![(200, "OK", sse.to_vec())]).await;
        let provider = provider_with_url(url);
        let request = InferenceRequest {
            model: "o4-mini".to_string(),
            ..simple_request()
        };
        assert!(provider.infer_stream(&request).await.is_ok());

        let sent = bodies.lock().expect("recorder").clone();
        let body = &sent[0];
        assert!(!body.contains("temperature"), "{body}");
    }

    // ─── Tools refused over a reasoning effort ──────────────────────────────

    /// Verbatim from `api.openai.com`, captured while reproducing #333.
    const TOOLS_REFUSED: &[u8] = br#"{"error":{"message":"Function tools with reasoning_effort are not supported for gpt-5.6-terra in /v1/chat/completions. To use function tools, use /v1/responses or set reasoning_effort to 'none'.","type":"invalid_request_error","param":"reasoning_effort","code":null}}"#;

    const OK_BODY: &[u8] = br#"{"choices":[{"message":{"content":"hi there"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2}}"#;

    fn request_with_a_tool() -> InferenceRequest {
        InferenceRequest {
            tools: vec![crate::provider::Tool {
                name: "get_time".to_string(),
                description: "Get the time".to_string(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            }],
            ..simple_request()
        }
    }

    #[tokio::test]
    async fn a_refusal_over_reasoning_effort_is_retried_with_none() {
        let _guard = always_on_tracing_guard();
        let (url, bodies) = leviath_testkit::spawn_mock_sequence(vec![
            (400, "Bad Request", TOOLS_REFUSED.to_vec()),
            (200, "OK", OK_BODY.to_vec()),
        ])
        .await;
        let provider = provider_with_url(url);
        let resp = provider.infer(&request_with_a_tool()).await.unwrap();
        assert_eq!(resp.content, "hi there");

        // The assertion that matters. "It eventually succeeded" would also pass
        // against a retry that resent the identical body.
        let sent = bodies.lock().expect("recorder").clone();
        assert_eq!(sent.len(), 2, "expected exactly one retry: {sent:?}");
        // Bound rather than indexed inside the message: a message expression
        // only runs when the assert fails, so `sent[0]` there would be a region
        // no passing run ever reaches.
        let (first, retry) = (&sent[0], &sent[1]);
        assert!(
            !first.contains("reasoning_effort"),
            "the first attempt should not mention the field: {first}"
        );
        assert!(
            retry.contains(r#""reasoning_effort":"none""#),
            "the retry should carry it: {retry}"
        );
    }

    #[tokio::test]
    async fn the_second_call_for_a_learned_model_asks_once() {
        // The point of remembering: a run makes many inferences and only the
        // first should pay for the discovery.
        let (url, bodies) = leviath_testkit::spawn_mock_sequence(vec![
            (400, "Bad Request", TOOLS_REFUSED.to_vec()),
            (200, "OK", OK_BODY.to_vec()),
            (200, "OK", OK_BODY.to_vec()),
        ])
        .await;
        let provider = provider_with_url(url);
        provider.infer(&request_with_a_tool()).await.unwrap();
        provider.infer(&request_with_a_tool()).await.unwrap();

        let sent = bodies.lock().expect("recorder").clone();
        assert_eq!(
            sent.len(),
            3,
            "the second inference should not retry: {sent:?}"
        );
        let third = &sent[2];
        assert!(
            third.contains(r#""reasoning_effort":"none""#),
            "the learned setting should be sent up front: {third}"
        );
    }

    #[tokio::test]
    async fn an_unrelated_bad_request_is_not_retried() {
        // A model that takes a reasoning effort but not the value `none` says so
        // in a message that never mentions tools. Retrying it with `none` would
        // resend the same rejection.
        let other = br#"{"error":{"message":"Unsupported value: 'reasoning_effort' does not support 'none' with this model. Supported values are: 'low', 'medium', 'high', and 'xhigh'.","type":"invalid_request_error","param":"reasoning_effort"}}"#;
        let (url, bodies) = leviath_testkit::spawn_mock_sequence(vec![
            (400, "Bad Request", other.to_vec()),
            (200, "OK", OK_BODY.to_vec()),
        ])
        .await;
        let provider = provider_with_url(url);
        let err = provider.infer(&request_with_a_tool()).await.unwrap_err();
        assert!(err.to_string().contains("API error:"), "{err}");
        assert_eq!(
            bodies.lock().expect("recorder").len(),
            1,
            "should not retry"
        );
    }

    #[tokio::test]
    async fn a_caller_supplied_reasoning_effort_is_left_alone() {
        // `[model.parameters] reasoning_effort = "low"` is the caller saying
        // what they want. Overriding it would ignore them silently.
        let (url, bodies) = leviath_testkit::spawn_mock_sequence(vec![
            (400, "Bad Request", TOOLS_REFUSED.to_vec()),
            (200, "OK", OK_BODY.to_vec()),
        ])
        .await;
        let provider = provider_with_url(url);
        let request = InferenceRequest {
            extra: serde_json::json!({ "reasoning_effort": "low" }),
            ..request_with_a_tool()
        };
        let err = provider.infer(&request).await.unwrap_err();
        assert!(err.to_string().contains("API error:"), "{err}");
        let sent = bodies.lock().expect("recorder").clone();
        assert_eq!(sent.len(), 1, "should not retry over the caller's setting");
        let first = &sent[0];
        assert!(first.contains(r#""reasoning_effort":"low""#), "{first}");
    }

    #[tokio::test]
    async fn streaming_learns_the_same_thing() {
        let _guard = always_on_tracing_guard();
        let sse = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
        let (url, bodies) = leviath_testkit::spawn_mock_sequence(vec![
            (400, "Bad Request", TOOLS_REFUSED.to_vec()),
            (200, "OK", sse.to_vec()),
        ])
        .await;
        let provider = provider_with_url(url);
        assert!(provider.infer_stream(&request_with_a_tool()).await.is_ok());
        let sent = bodies.lock().expect("recorder").clone();
        assert_eq!(sent.len(), 2);
        let retry = &sent[1];
        assert!(retry.contains(r#""reasoning_effort":"none""#), "{retry}");
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

    #[tokio::test]
    async fn infer_non_success_status_returns_error() {
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
    async fn infer_stream_non_success_status_returns_error() {
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
        let body = br#"{"data":[{"id":"gpt-5.4"}]}"#;
        let url = spawn_mock_server(200, "OK", body).await;
        let provider = provider_with_url(url);
        let models = provider.list_models().await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gpt-5.4");
        assert_eq!(models[0].provider, "openai");
    }

    #[tokio::test]
    async fn list_models_non_success_status_returns_error() {
        let url = spawn_mock_server(401, "Unauthorized", b"bad key").await;
        let provider = provider_with_url(url);
        let err = provider.list_models().await.unwrap_err();
        assert!(err.to_string().contains("401"));
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

    #[tokio::test]
    async fn list_models_skips_entries_without_id() {
        let body = br#"{"data":[{"no_id": true}, {"id":"valid-model"}]}"#;
        let url = spawn_mock_server(200, "OK", body).await;
        let provider = provider_with_url(url);
        let models = provider.list_models().await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "valid-model");
    }

    #[tokio::test]
    async fn list_models_skips_entries_with_non_string_id() {
        // covers the `.as_str()?` None branch in the filter_map
        let body = br#"{"data":[{"id": 42}, {"id":"valid-model"}]}"#;
        let url = spawn_mock_server(200, "OK", body).await;
        let provider = provider_with_url(url);
        let models = provider.list_models().await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "valid-model");
    }

    // ─── transport-failure arms (connection refused, no server listening) ──

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

    // ─── "unknown error" fallback when the error body can't be read ────────

    #[tokio::test]
    async fn list_models_non_success_body_read_error_falls_back_to_unknown_error() {
        let url = spawn_mock_server_truncated_body(500, "Internal Server Error").await;
        let provider = provider_with_url(url);
        let err = provider.list_models().await.unwrap_err();
        assert!(err.to_string().contains("unknown error"));
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
        let limited = OpenAIProvider::with_overrides(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "k".to_string(),
            HashMap::new(),
            Some(&cfg),
        );
        assert!(limited.rate_limiter.is_some());
        let unlimited = OpenAIProvider::with_overrides(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "k".to_string(),
            HashMap::new(),
            None,
        );
        assert!(unlimited.rate_limiter.is_none());
    }
}
