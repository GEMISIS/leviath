//! Google Gemini provider implementation (via OpenAI-compatible endpoint).

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

/// Google Gemini provider using the OpenAI-compatible endpoint.
pub struct GeminiProvider {
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

impl GeminiProvider {
    /// Create a new Gemini provider.
    pub fn new(api_key: String) -> Self {
        Self {
            client: Client::new(),
            api_key,
            base_url: "https://generativelanguage.googleapis.com/v1beta/openai".to_string(),
            rate_limiter: None,
            capability_overrides: HashMap::new(),
        }
    }

    /// Create a new Gemini provider with full configuration.
    pub fn with_config(config: ProviderConfig) -> Self {
        let rate_limiter = config.rate_limit.as_ref().map(RateLimiter::new);
        Self {
            client: Client::new(),
            api_key: config.api_key,
            base_url: config.base_url.unwrap_or_else(|| {
                "https://generativelanguage.googleapis.com/v1beta/openai".to_string()
            }),
            rate_limiter,
            capability_overrides: HashMap::new(),
        }
    }

    /// Create a new Gemini provider with per-model capability overrides.
    pub fn with_overrides(api_key: String, overrides: HashMap<String, ModelCapabilities>) -> Self {
        Self {
            client: Client::new(),
            api_key,
            base_url: "https://generativelanguage.googleapis.com/v1beta/openai".to_string(),
            rate_limiter: None,
            capability_overrides: overrides,
        }
    }

    /// Return built-in capability defaults for a model.
    ///
    /// Current model families (all share 1M context, 65K output, full tool/streaming support):
    /// - gemini-3.5-flash (latest flash, near-Pro intelligence)
    /// - gemini-3.1-pro-preview (latest pro, reasoning-first)
    /// - gemini-3-flash (complex multimodal/agentic)
    /// - gemini-3.1-flash-lite (cost-efficient, high-volume)
    ///
    /// All Gemini models currently expose identical capabilities via the
    /// OpenAI-compatible endpoint, so a single default covers all families.
    /// When ModelCapabilities gains a `supports_thinking` field, split the
    /// flash-lite variants into their own branch.
    fn builtin_capabilities(&self, _model: &str) -> ModelCapabilities {
        ModelCapabilities {
            supports_temperature: true,
            supports_streaming: true,
            supports_tools: true,
            supports_system_prompt: true,
            max_context_tokens: 1_048_576,
            max_output_tokens: 65_535,
        }
    }
}

#[async_trait]
impl Provider for GeminiProvider {
    async fn infer(&self, request: InferenceRequest) -> Result<InferenceResponse> {
        tracing::debug!(model = %request.model, "Calling Gemini API");

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
        tracing::debug!(model = %request.model, "Calling Gemini API (streaming)");

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
        "google"
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
                    provider: "google".into(),
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

    /// No-op `tracing::Subscriber` that reports every callsite enabled.
    /// Without a registered subscriber during tests, `tracing::debug!`/`warn!`
    /// short-circuit field-expression evaluation before the enclosing
    /// branch's field-list lines ever execute, even though the branch itself
    /// demonstrably runs -- this makes those lines genuinely exercised
    /// instead of an unexplained coverage gap.
    struct AlwaysOnSubscriber;
    impl tracing::Subscriber for AlwaysOnSubscriber {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
        fn event(&self, _event: &tracing::Event<'_>) {}
        fn enter(&self, _span: &tracing::span::Id) {}
        fn exit(&self, _span: &tracing::span::Id) {}
    }

    #[test]
    fn always_on_subscriber_span_methods_are_all_no_ops() {
        use tracing::Subscriber;
        let sub = AlwaysOnSubscriber;
        let span = tracing::span::Id::from_u64(1);
        let _guard = tracing::subscriber::set_default(AlwaysOnSubscriber);
        let s = tracing::info_span!("test-span", field = tracing::field::Empty);
        s.record("field", 1);
        s.in_scope(|| {});
        sub.enter(&span);
        sub.exit(&span);
        sub.record_follows_from(&span, &span);
    }

    #[test]
    fn test_provider_name() {
        let provider = GeminiProvider::new("test-key".to_string());
        assert_eq!(provider.name(), "google");
    }

    #[test]
    fn test_default_base_url() {
        let provider = GeminiProvider::new("test-key".to_string());
        assert_eq!(
            provider.base_url,
            "https://generativelanguage.googleapis.com/v1beta/openai"
        );
    }

    #[test]
    fn test_builtin_capabilities_gemini_35_flash() {
        let provider = GeminiProvider::new("test-key".to_string());
        let caps = provider.builtin_capabilities("gemini-3.5-flash");
        assert!(caps.supports_temperature);
        assert!(caps.supports_streaming);
        assert!(caps.supports_tools);
        assert!(caps.supports_system_prompt);
        assert_eq!(caps.max_context_tokens, 1_048_576);
        assert_eq!(caps.max_output_tokens, 65_535);
    }

    #[test]
    fn test_builtin_capabilities_gemini_31_pro() {
        let provider = GeminiProvider::new("test-key".to_string());
        let caps = provider.builtin_capabilities("gemini-3.1-pro-preview");
        assert!(caps.supports_temperature);
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 1_048_576);
        assert_eq!(caps.max_output_tokens, 65_535);
    }

    #[test]
    fn test_builtin_capabilities_gemini_3_flash() {
        let provider = GeminiProvider::new("test-key".to_string());
        let caps = provider.builtin_capabilities("gemini-3-flash");
        assert!(caps.supports_temperature);
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 1_048_576);
        assert_eq!(caps.max_output_tokens, 65_535);
    }

    #[test]
    fn test_builtin_capabilities_gemini_31_flash_lite() {
        let provider = GeminiProvider::new("test-key".to_string());
        let caps = provider.builtin_capabilities("gemini-3.1-flash-lite");
        assert!(caps.supports_temperature);
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 1_048_576);
        assert_eq!(caps.max_output_tokens, 65_535);
    }

    #[test]
    fn test_default_capabilities() {
        let provider = GeminiProvider::new("test-key".to_string());
        let caps = provider.builtin_capabilities("gemini-future-model");
        assert!(caps.supports_temperature);
        assert!(caps.supports_streaming);
        assert!(caps.supports_tools);
        assert!(caps.supports_system_prompt);
        assert_eq!(caps.max_context_tokens, 1_048_576);
        assert_eq!(caps.max_output_tokens, 65_535);
    }

    #[test]
    fn test_capabilities_override() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "gemini-3.5-flash".to_string(),
            ModelCapabilities {
                supports_temperature: false,
                supports_streaming: false,
                supports_tools: false,
                supports_system_prompt: false,
                max_context_tokens: 1,
                max_output_tokens: 1,
            },
        );
        let provider = GeminiProvider::with_overrides("test-key".to_string(), overrides);
        let caps = provider.capabilities("gemini-3.5-flash");
        assert!(!caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 1);
    }

    #[test]
    fn test_build_request_body() {
        let request = InferenceRequest {
            messages: vec![
                crate::provider::Message {
                    role: "system".to_string(),
                    content: "You are helpful.".to_string(),
                    cache_breakpoint: false,
                },
                crate::provider::Message {
                    role: "user".to_string(),
                    content: "Hello".to_string(),
                    cache_breakpoint: false,
                },
            ],
            model: "gemini-3.5-flash".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
        };

        let body = build_openai_request_body(&request);
        assert_eq!(body["model"], "gemini-3.5-flash");
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
    fn test_context_limits() {
        let provider = GeminiProvider::new("test-key".to_string());
        assert_eq!(provider.max_context_tokens("gemini-3.5-flash"), 1_048_576);
        assert_eq!(
            provider.max_context_tokens("gemini-3.1-pro-preview"),
            1_048_576
        );
    }

    // ── Additional coverage tests ──────────────────────────────────────────

    #[test]
    fn test_with_config_default_url() {
        let config = ProviderConfig {
            api_key: "key".to_string(),
            base_url: None,
            rate_limit: None,
        };
        let provider = GeminiProvider::with_config(config);
        assert!(provider
            .base_url
            .contains("generativelanguage.googleapis.com"));
    }

    #[test]
    fn test_with_config_custom_url() {
        let config = ProviderConfig {
            api_key: "key".to_string(),
            base_url: Some("https://custom.google.com".to_string()),
            rate_limit: None,
        };
        let provider = GeminiProvider::with_config(config);
        assert_eq!(provider.base_url, "https://custom.google.com");
    }

    #[test]
    fn test_with_config_with_rate_limit() {
        let config = ProviderConfig {
            api_key: "key".to_string(),
            base_url: None,
            rate_limit: Some(crate::provider::RateLimitConfig {
                requests_per_minute: 30,
                tokens_per_minute: 50000,
            }),
        };
        let provider = GeminiProvider::with_config(config);
        assert!(provider.rate_limiter.is_some());
    }

    #[test]
    fn test_count_tokens_uses_tiktoken() {
        let provider = GeminiProvider::new("key".to_string());
        let tokens = provider.count_tokens("Hello, world!", "gemini-3.5-flash");
        assert!(tokens > 0);
    }

    #[test]
    fn test_count_tokens_empty() {
        let provider = GeminiProvider::new("key".to_string());
        let tokens = provider.count_tokens("", "gemini-3.5-flash");
        assert_eq!(tokens, 0);
    }

    #[test]
    fn test_capabilities_override_takes_precedence() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "gemini-custom".to_string(),
            ModelCapabilities {
                supports_temperature: false,
                supports_streaming: false,
                supports_tools: false,
                supports_system_prompt: false,
                max_context_tokens: 42,
                max_output_tokens: 10,
            },
        );
        let provider = GeminiProvider::with_overrides("key".to_string(), overrides);
        let caps = provider.capabilities("gemini-custom");
        assert_eq!(caps.max_context_tokens, 42);
        assert!(!caps.supports_temperature);
    }

    #[test]
    fn test_capabilities_builtin_fallthrough() {
        let provider = GeminiProvider::with_overrides("key".to_string(), HashMap::new());
        let caps = provider.capabilities("gemini-3.5-flash");
        assert_eq!(caps.max_context_tokens, 1_048_576);
    }

    #[test]
    fn test_max_context_tokens_delegates() {
        let provider = GeminiProvider::new("key".to_string());
        assert_eq!(provider.max_context_tokens("gemini-3.5-flash"), 1_048_576);
    }

    #[test]
    fn test_parse_response_no_choices() {
        let body = serde_json::json!({});
        let result = parse_openai_response(&body);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_response_finish_reason_length() {
        let body = serde_json::json!({
            "choices": [{
                "message": { "content": "truncated" },
                "finish_reason": "length"
            }],
            "usage": { "prompt_tokens": 5, "completion_tokens": 100 }
        });
        let response = parse_openai_response(&body).unwrap();
        assert_eq!(response.finish_reason, FinishReason::TokenLimit);
    }

    // ─── HTTP-call-level tests via a raw-TCP mock server ───────────────────

    async fn spawn_mock_server(status: u16, reason: &str, body: &'static [u8]) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            status, reason, body.len()
        )
        .into_bytes();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = [0u8; 8192];
            let _ = socket.read(&mut buf).await;
            let _ = socket.write_all(&response).await;
            let _ = socket.write_all(body).await;
            let _ = socket.flush().await;
            let _ = socket.shutdown().await;
        });
        format!("http://{}", addr)
    }

    fn provider_with_url(url: String) -> GeminiProvider {
        GeminiProvider::with_config(ProviderConfig {
            api_key: "test-key".to_string(),
            base_url: Some(url),
            rate_limit: None,
        })
    }

    fn simple_request() -> InferenceRequest {
        InferenceRequest {
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "hi".to_string(),
                cache_breakpoint: false,
            }],
            model: "gemini-3.5-flash".to_string(),
            max_tokens: 100,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::Value::Null,
        }
    }

    #[tokio::test]
    async fn infer_success_parses_response() {
        // Registers a real Subscriber so the tracing::debug! call's field
        // arguments at the top of infer() are actually exercised.
        let _guard = tracing::subscriber::set_default(AlwaysOnSubscriber);
        let body = br#"{"choices":[{"message":{"content":"hi there"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2}}"#;
        let url = spawn_mock_server(200, "OK", body).await;
        let provider = provider_with_url(url);
        let resp = provider.infer(simple_request()).await.unwrap();
        assert_eq!(resp.content, "hi there");
    }

    #[tokio::test]
    async fn infer_non_success_status_returns_error() {
        let url = spawn_mock_server(500, "Internal Server Error", b"boom").await;
        let provider = provider_with_url(url);
        let err = provider.infer(simple_request()).await.unwrap_err();
        assert!(err.to_string().contains("API error:"));
    }

    #[tokio::test]
    async fn infer_malformed_json_returns_invalid_response() {
        let url = spawn_mock_server(200, "OK", b"not json").await;
        let provider = provider_with_url(url);
        let err = provider.infer(simple_request()).await.unwrap_err();
        assert!(err.to_string().contains("Invalid response:"));
    }

    #[tokio::test]
    async fn infer_stream_non_success_status_returns_error() {
        let url = spawn_mock_server(503, "Service Unavailable", b"down").await;
        let provider = provider_with_url(url);
        let result = provider.infer_stream(simple_request()).await;
        assert!(result.is_err());
        assert!(result.err().unwrap().to_string().contains("API error:"));
    }

    #[tokio::test]
    async fn infer_stream_success_yields_chunks() {
        // Registers a real Subscriber so the tracing::debug! call's field
        // arguments at the top of infer_stream() are actually exercised.
        let _guard = tracing::subscriber::set_default(AlwaysOnSubscriber);
        let sse_body =
            b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
        let url = spawn_mock_server(200, "OK", sse_body).await;
        let provider = provider_with_url(url);
        let mut stream = provider.infer_stream(simple_request()).await.unwrap();
        use tokio_stream::StreamExt;
        let chunk = stream.next().await.unwrap().unwrap();
        assert_eq!(chunk.delta, "hi");
    }

    #[tokio::test]
    async fn list_models_success_returns_models() {
        let body = br#"{"data":[{"id":"gemini-3.5-flash"}]}"#;
        let url = spawn_mock_server(200, "OK", body).await;
        let provider = provider_with_url(url);
        let models = provider.list_models().await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gemini-3.5-flash");
        assert_eq!(models[0].provider, "google");
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

    // ─── HTTP error paths (connection refused) ─────────────────────────────

    #[tokio::test]
    async fn infer_connection_refused_returns_error() {
        let provider = provider_with_url("http://127.0.0.1:19997".to_string());
        let err = provider.infer(simple_request()).await.unwrap_err();
        assert!(err.to_string().contains("Request failed:"));
    }

    #[tokio::test]
    async fn infer_stream_connection_refused_returns_error() {
        let provider = provider_with_url("http://127.0.0.1:19997".to_string());
        let result = provider.infer_stream(simple_request()).await;
        assert!(result
            .err()
            .unwrap()
            .to_string()
            .contains("Request failed:"));
    }

    #[tokio::test]
    async fn list_models_connection_refused_returns_error() {
        let provider = provider_with_url("http://127.0.0.1:19997".to_string());
        let err = provider.list_models().await.unwrap_err();
        assert!(err.to_string().contains("Request failed:"));
    }

    /// Declares a `Content-Length` larger than the bytes actually sent, then
    /// closes -- forcing a genuine mid-body I/O error when the caller reads
    /// the response body, rather than a merely garbled-but-readable one.
    async fn spawn_mock_server_truncated_body(status: u16, reason: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: 9999\r\nConnection: close\r\n\r\n",
            status, reason
        )
        .into_bytes();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = [0u8; 8192];
            let _ = socket.read(&mut buf).await;
            let _ = socket.write_all(&response).await;
            let _ = socket.write_all(b"short").await;
            let _ = socket.flush().await;
            let _ = socket.shutdown().await;
        });
        format!("http://{}", addr)
    }

    #[tokio::test]
    async fn list_models_non_success_body_read_error_falls_back_to_unknown_error() {
        let url = spawn_mock_server_truncated_body(500, "Internal Server Error").await;
        let provider = provider_with_url(url);
        let err = provider.list_models().await.unwrap_err();
        assert!(err.to_string().contains("unknown error"));
    }
}
