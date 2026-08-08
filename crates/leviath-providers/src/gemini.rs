//! Google Gemini provider implementation (via OpenAI-compatible endpoint).

use crate::openai_compat::{
    OpenAiSseStream, build_openai_request_body, parse_openai_response, send_chat_request,
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

/// Gemini model family, classified from a model id, used to pick per-family
/// capability defaults. Values are identical across families today; the split
/// exists so a family's limits can diverge without reworking the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GeminiFamily {
    /// Cost-efficient, high-volume variants (`*-flash-lite`).
    FlashLite,
    /// Reasoning-first pro variants (`*-pro*`).
    Pro,
    /// Standard flash variants (`*-flash*`, excluding flash-lite).
    Flash,
    /// Anything else / future models.
    Other,
}

impl GeminiFamily {
    /// `(max_context_tokens, max_output_tokens)` for the Flash-Lite family.
    const FLASH_LITE_LIMITS: (usize, usize) = (1_048_576, 65_535);
    /// The same, for Pro.
    const PRO_LIMITS: (usize, usize) = (1_048_576, 65_535);
    /// The same, for Flash.
    const FLASH_LIMITS: (usize, usize) = (1_048_576, 65_535);
    /// The same, for anything this classifier does not recognise.
    const OTHER_LIMITS: (usize, usize) = (1_048_576, 65_535);

    /// This family's context and output ceilings.
    ///
    /// Four named constants that happen to agree today, rather than one shared
    /// value: the point is that a family's limits can move without disturbing
    /// the others, and four constants say that where four identical match arms
    /// only looked like an oversight - which is what the lint kept reporting.
    const fn limits(self) -> (usize, usize) {
        match self {
            Self::FlashLite => Self::FLASH_LITE_LIMITS,
            Self::Pro => Self::PRO_LIMITS,
            Self::Flash => Self::FLASH_LIMITS,
            Self::Other => Self::OTHER_LIMITS,
        }
    }

    fn classify(model: &str) -> Self {
        if model.contains("flash-lite") {
            GeminiFamily::FlashLite
        } else if model.contains("pro") {
            GeminiFamily::Pro
        } else if model.contains("flash") {
            GeminiFamily::Flash
        } else {
            GeminiFamily::Other
        }
    }
}

/// Google Gemini provider using the OpenAI-compatible endpoint.
pub struct GeminiProvider {
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

impl GeminiProvider {
    /// Create a new Gemini provider.
    pub fn new(client: reqwest::Client, api_key: String) -> Self {
        Self {
            client,
            api_key,
            base_url: "https://generativelanguage.googleapis.com/v1beta/openai".to_string(),
            rate_limiter: None,
            capability_overrides: HashMap::new(),
        }
    }

    /// Create a new Gemini provider with full configuration.
    pub fn with_config(client: reqwest::Client, config: ProviderConfig) -> Self {
        let rate_limiter = config.rate_limit.as_ref().map(RateLimiter::new);
        Self {
            client,
            api_key: config.api_key,
            base_url: config.base_url.unwrap_or_else(|| {
                "https://generativelanguage.googleapis.com/v1beta/openai".to_string()
            }),
            rate_limiter,
            capability_overrides: HashMap::new(),
        }
    }

    /// Create a new Gemini provider with per-model capability overrides.
    pub fn with_overrides(
        client: reqwest::Client,
        api_key: String,
        overrides: HashMap<String, ModelCapabilities>,
        rate_limit: Option<&crate::provider::RateLimitConfig>,
    ) -> Self {
        Self {
            client,
            api_key,
            base_url: "https://generativelanguage.googleapis.com/v1beta/openai".to_string(),
            rate_limiter: rate_limit.map(crate::rate_limit::RateLimiter::new),
            capability_overrides: overrides,
        }
    }

    /// Return built-in capability defaults for a model, by family.
    ///
    /// Current model families all share 1M context / 65K output / full
    /// tool+streaming support, so the values are identical today. The per-family
    /// branching exists so a family can diverge (e.g. a smaller flash-lite output
    /// cap, or a future `supports_thinking` flag) without another refactor.
    /// `list_models` fetches the *authoritative* per-model limits from the native
    /// API; this is the offline default used for the sync `capabilities()` path.
    /// - gemini-3.5-flash (latest flash, near-Pro intelligence)
    /// - gemini-3.1-pro-preview (latest pro, reasoning-first)
    /// - gemini-3-flash (complex multimodal/agentic)
    /// - gemini-3.1-flash-lite (cost-efficient, high-volume)
    fn builtin_capabilities(&self, model: &str) -> ModelCapabilities {
        let (max_context_tokens, max_output_tokens) = GeminiFamily::classify(model).limits();
        ModelCapabilities {
            supports_temperature: true,
            supports_streaming: true,
            supports_tools: true,
            supports_system_prompt: true,
            max_context_tokens,
            max_output_tokens,
        }
    }

    /// Derive the native Gemini API base (`.../v1beta`) from the OpenAI-compatible
    /// base (`.../v1beta/openai`). Native-only endpoints (`:countTokens`, the
    /// per-model `models` listing) live under the former. Returns `None` when the
    /// configured base doesn't follow the `/openai` convention (e.g. a custom
    /// proxy), so callers fall back rather than guessing a wrong URL.
    fn native_base(&self) -> Option<String> {
        self.base_url.strip_suffix("/openai").map(|s| s.to_string())
    }

    /// Call Gemini's exact native `:countTokens` endpoint for `text`.
    ///
    /// Wraps the text as a single user content part. Returns the reported
    /// `totalTokens`, or an error the caller turns into a heuristic fallback.
    async fn count_tokens_remote(&self, text: &str, model: &str) -> Result<usize> {
        let native = self.native_base().ok_or_else(|| {
            ProviderError::Other(
                "non-standard base_url; native countTokens unavailable".to_string(),
            )
        })?;
        let url = format!("{}/models/{}:countTokens", native, model);
        let body = serde_json::json!({
            "contents": [{ "role": "user", "parts": [{ "text": text }] }],
        });
        let response = self
            .client
            .post(&url)
            .header("x-goog-api-key", &self.api_key)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::RequestFailed(e.to_string()))?;
        let response = crate::provider::check_http_response(response, None).await?;
        let value: serde_json::Value = response
            .json()
            .await
            .map_err(|e| ProviderError::InvalidResponse(e.to_string()))?;
        value
            .get("totalTokens")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .ok_or_else(|| {
                ProviderError::InvalidResponse("countTokens missing totalTokens".to_string())
            })
    }
}

#[async_trait]
impl Provider for GeminiProvider {
    async fn infer(&self, request: &InferenceRequest) -> Result<InferenceResponse> {
        tracing::debug!(model = %request.model, "Calling Gemini API");

        if let Some(limiter) = &self.rate_limiter {
            limiter.acquire().await?;
        }

        let body = build_openai_request_body(request);
        let url = format!("{}/chat/completions", self.base_url);

        let response = send_chat_request(
            &self.client,
            "gemini",
            &url,
            &[
                ("Authorization", format!("Bearer {}", self.api_key)),
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
        tracing::debug!(model = %request.model, "Calling Gemini API (streaming)");

        if let Some(limiter) = &self.rate_limiter {
            limiter.acquire().await?;
        }

        let mut body = build_openai_request_body(request);
        body["stream"] = serde_json::Value::Bool(true);
        body["stream_options"] = serde_json::json!({ "include_usage": true });
        let url = format!("{}/chat/completions", self.base_url);

        let response = send_chat_request(
            &self.client,
            "gemini",
            &url,
            &[
                ("Authorization", format!("Bearer {}", self.api_key)),
                ("Content-Type", "application/json".to_string()),
            ],
            &body,
            self.rate_limiter.as_ref(),
            request.request_timeout_secs,
        )
        .await?;

        let byte_stream = response.bytes_stream();
        let stream = OpenAiSseStream::new(byte_stream);

        Ok(Box::pin(stream))
    }

    async fn count_tokens(&self, text: &str, model: &str) -> usize {
        // Prefer Gemini's exact native `:countTokens` endpoint; fall back to the
        // local heuristic on any error (network, non-2xx, parse, non-standard base).
        match self.count_tokens_remote(text, model).await {
            Ok(n) => n,
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    "Gemini countTokens endpoint failed; using heuristic"
                );
                crate::tokenizer::count_tokens(text, model)
            }
        }
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
        // Prefer the native `/v1beta/models` listing: unlike the OpenAI-compat
        // `/models`, it returns real per-model `inputTokenLimit`/`outputTokenLimit`.
        // Fall back to the compat listing (with builtin caps) when the base URL
        // isn't the standard `.../openai` form.
        match self.native_base() {
            Some(native) => self.list_models_native(&native).await,
            None => self.list_models_compat().await,
        }
    }
}

impl GeminiProvider {
    /// Native `/v1beta/models` listing with authoritative per-model token limits.
    async fn list_models_native(&self, native_base: &str) -> Result<Vec<ModelInfo>> {
        let response = self
            .client
            .get(format!("{}/models", native_base))
            .header("x-goog-api-key", &self.api_key)
            .send()
            .await
            .map_err(|e| ProviderError::RequestFailed(e.to_string()))?;
        let response = crate::provider::check_http_response(response, None).await?;
        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| ProviderError::InvalidResponse(e.to_string()))?;

        let models = body
            .get("models")
            .and_then(|d| d.as_array())
            .ok_or_else(|| {
                ProviderError::InvalidResponse("No models field in models response".to_string())
            })?
            .iter()
            .filter_map(|item| {
                // `name` is like "models/gemini-3.5-flash"; the id drops the prefix.
                let name = item.get("name")?.as_str()?;
                let id = match name.strip_prefix("models/") {
                    Some(rest) => rest.to_string(),
                    None => name.to_string(),
                };
                // Start from the family defaults, then override with the API's
                // authoritative limits where present.
                let mut capabilities = self.capabilities(&id);
                if let Some(ctx) = item.get("inputTokenLimit").and_then(|v| v.as_u64()) {
                    capabilities.max_context_tokens = ctx as usize;
                }
                if let Some(out) = item.get("outputTokenLimit").and_then(|v| v.as_u64()) {
                    capabilities.max_output_tokens = out as usize;
                }
                Some(ModelInfo {
                    id,
                    display_name: item
                        .get("displayName")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string()),
                    provider: "google".into(),
                    capabilities,
                })
            })
            .collect();

        Ok(models)
    }

    /// OpenAI-compat `/models` listing (no per-model token limits) used only when
    /// the configured base URL isn't the standard native-derivable form.
    async fn list_models_compat(&self) -> Result<Vec<ModelInfo>> {
        let response = self
            .client
            .get(format!("{}/models", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .send()
            .await
            .map_err(|e| ProviderError::RequestFailed(e.to_string()))?;
        let response = crate::provider::check_http_response(response, None).await?;
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
    use crate::test_support::always_on_tracing_guard;
    use leviath_testkit::{spawn_mock_server, spawn_mock_server_truncated_body};

    #[test]
    fn test_provider_name() {
        let provider = GeminiProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        assert_eq!(provider.name(), "google");
    }

    #[test]
    fn test_default_base_url() {
        let provider = GeminiProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        assert_eq!(
            provider.base_url,
            "https://generativelanguage.googleapis.com/v1beta/openai"
        );
    }

    #[test]
    fn test_builtin_capabilities_gemini_35_flash() {
        let provider = GeminiProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
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
        let provider = GeminiProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let caps = provider.builtin_capabilities("gemini-3.1-pro-preview");
        assert!(caps.supports_temperature);
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 1_048_576);
        assert_eq!(caps.max_output_tokens, 65_535);
    }

    #[test]
    fn test_builtin_capabilities_gemini_3_flash() {
        let provider = GeminiProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let caps = provider.builtin_capabilities("gemini-3-flash");
        assert!(caps.supports_temperature);
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 1_048_576);
        assert_eq!(caps.max_output_tokens, 65_535);
    }

    #[test]
    fn test_builtin_capabilities_gemini_31_flash_lite() {
        let provider = GeminiProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let caps = provider.builtin_capabilities("gemini-3.1-flash-lite");
        assert!(caps.supports_temperature);
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 1_048_576);
        assert_eq!(caps.max_output_tokens, 65_535);
    }

    #[test]
    fn test_default_capabilities() {
        let provider = GeminiProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
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
        let provider = GeminiProvider::with_overrides(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
            overrides,
            None,
        );
        let caps = provider.capabilities("gemini-3.5-flash");
        assert!(!caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 1);
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
            model: "gemini-3.5-flash".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
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
        let provider = GeminiProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
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
            request_timeout_secs: None,
        };
        let provider = GeminiProvider::with_config(
            crate::provider::build_http_client(None).expect("a test client builds"),
            config,
        );
        assert!(
            provider
                .base_url
                .contains("generativelanguage.googleapis.com")
        );
    }

    #[test]
    fn test_with_config_custom_url() {
        let config = ProviderConfig {
            api_key: "key".to_string(),
            base_url: Some("https://custom.google.com".to_string()),
            rate_limit: None,
            request_timeout_secs: None,
        };
        let provider = GeminiProvider::with_config(
            crate::provider::build_http_client(None).expect("a test client builds"),
            config,
        );
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
            request_timeout_secs: None,
        };
        let provider = GeminiProvider::with_config(
            crate::provider::build_http_client(None).expect("a test client builds"),
            config,
        );
        assert!(provider.rate_limiter.is_some());
    }

    /// Provider whose native base is a non-standard URL (no `/openai` suffix),
    /// so `count_tokens` skips the endpoint and uses the local heuristic.
    fn heuristic_only_provider() -> GeminiProvider {
        GeminiProvider {
            client: reqwest::Client::new(),
            api_key: "key".to_string(),
            base_url: "http://127.0.0.1:19997".to_string(),
            rate_limiter: None,
            capability_overrides: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn test_count_tokens_heuristic_fallback() {
        let provider = heuristic_only_provider();
        // 8 chars / 4 = 2 (gemini heuristic branch)
        let tokens = provider.count_tokens("12345678", "gemini-3.5-flash").await;
        assert_eq!(tokens, 2);
    }

    #[tokio::test]
    async fn test_count_tokens_empty() {
        let provider = heuristic_only_provider();
        let tokens = provider.count_tokens("", "gemini-3.5-flash").await;
        assert_eq!(tokens, 0);
    }

    #[tokio::test]
    async fn test_count_tokens_uses_exact_endpoint() {
        let base = spawn_mock_server(200, "OK", br#"{"totalTokens": 99}"#).await;
        let provider = GeminiProvider {
            client: reqwest::Client::new(),
            api_key: "key".to_string(),
            base_url: format!("{}/openai", base),
            rate_limiter: None,
            capability_overrides: HashMap::new(),
        };
        let tokens = provider.count_tokens("anything", "gemini-3.5-flash").await;
        assert_eq!(tokens, 99);
    }

    #[tokio::test]
    async fn test_count_tokens_falls_back_on_error_status() {
        let base = spawn_mock_server(500, "Internal Server Error", b"boom").await;
        let provider = GeminiProvider {
            client: reqwest::Client::new(),
            api_key: "key".to_string(),
            base_url: format!("{}/openai", base),
            rate_limiter: None,
            capability_overrides: HashMap::new(),
        };
        // 8 chars / 4 = 2 (heuristic fallback)
        let tokens = provider.count_tokens("12345678", "gemini-3.5-flash").await;
        assert_eq!(tokens, 2);
    }

    #[tokio::test]
    async fn test_count_tokens_falls_back_on_connection_error() {
        // Base ends in `/openai` so `native_base` resolves, but the port is dead:
        // the POST fails at send() → RequestFailed → heuristic fallback.
        let provider = GeminiProvider {
            client: reqwest::Client::new(),
            api_key: "key".to_string(),
            base_url: "http://127.0.0.1:19997/openai".to_string(),
            rate_limiter: None,
            capability_overrides: HashMap::new(),
        };
        let tokens = provider.count_tokens("12345678", "gemini-3.5-flash").await;
        assert_eq!(tokens, 2);
    }

    #[tokio::test]
    async fn test_count_tokens_falls_back_on_malformed_json() {
        let base = spawn_mock_server(200, "OK", b"not json").await;
        let provider = GeminiProvider {
            client: reqwest::Client::new(),
            api_key: "key".to_string(),
            base_url: format!("{}/openai", base),
            rate_limiter: None,
            capability_overrides: HashMap::new(),
        };
        let tokens = provider.count_tokens("12345678", "gemini-3.5-flash").await;
        assert_eq!(tokens, 2);
    }

    #[tokio::test]
    async fn test_count_tokens_falls_back_on_missing_total_tokens() {
        let base = spawn_mock_server(200, "OK", br#"{"unexpected": true}"#).await;
        let provider = GeminiProvider {
            client: reqwest::Client::new(),
            api_key: "key".to_string(),
            base_url: format!("{}/openai", base),
            rate_limiter: None,
            capability_overrides: HashMap::new(),
        };
        let tokens = provider.count_tokens("12345678", "gemini-3.5-flash").await;
        assert_eq!(tokens, 2);
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
        let provider = GeminiProvider::with_overrides(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
            overrides,
            None,
        );
        let caps = provider.capabilities("gemini-custom");
        assert_eq!(caps.max_context_tokens, 42);
        assert!(!caps.supports_temperature);
    }

    #[test]
    fn test_capabilities_builtin_fallthrough() {
        let provider = GeminiProvider::with_overrides(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
            HashMap::new(),
            None,
        );
        let caps = provider.capabilities("gemini-3.5-flash");
        assert_eq!(caps.max_context_tokens, 1_048_576);
    }

    #[test]
    fn test_max_context_tokens_delegates() {
        let provider = GeminiProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
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

    fn provider_with_url(url: String) -> GeminiProvider {
        GeminiProvider::with_config(
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
            model: "gemini-3.5-flash".to_string(),
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
    async fn list_models_non_success_body_read_error_falls_back_to_status() {
        // A truncated body makes reading the error text fail; `check_http_response`
        // still reports the status (falling back to the reqwest error string).
        let url = spawn_mock_server_truncated_body(500, "Internal Server Error").await;
        let provider = provider_with_url(url);
        let err = provider.list_models().await.unwrap_err();
        assert!(err.to_string().contains("500"));
    }

    // ─── Native models listing (real per-model token limits) ──────────────

    /// Provider whose base ends in `/openai`, so `native_base()` resolves and
    /// `list_models` takes the native path.
    fn native_provider_with_base(base: &str) -> GeminiProvider {
        GeminiProvider {
            client: reqwest::Client::new(),
            api_key: "test-key".to_string(),
            base_url: format!("{}/openai", base),
            rate_limiter: None,
            capability_overrides: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn list_models_native_uses_real_token_limits() {
        // `name` carries the "models/" prefix; the API returns authoritative
        // per-model limits that override the builtin family defaults.
        let body = br#"{"models":[
            {"name":"models/gemini-3.5-flash","displayName":"Flash","inputTokenLimit":2000000,"outputTokenLimit":8192}
        ]}"#;
        let base = spawn_mock_server(200, "OK", body).await;
        let provider = native_provider_with_base(&base);
        let models = provider.list_models().await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gemini-3.5-flash");
        assert_eq!(models[0].display_name.as_deref(), Some("Flash"));
        assert_eq!(models[0].capabilities.max_context_tokens, 2_000_000);
        assert_eq!(models[0].capabilities.max_output_tokens, 8192);
    }

    #[tokio::test]
    async fn list_models_native_falls_back_to_builtin_when_limits_absent() {
        // No limit fields → builtin family defaults are kept.
        let body = br#"{"models":[{"name":"models/gemini-3.1-pro-preview"}]}"#;
        let base = spawn_mock_server(200, "OK", body).await;
        let provider = native_provider_with_base(&base);
        let models = provider.list_models().await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gemini-3.1-pro-preview");
        assert_eq!(models[0].capabilities.max_context_tokens, 1_048_576);
        assert_eq!(models[0].capabilities.max_output_tokens, 65_535);
    }

    #[tokio::test]
    async fn list_models_native_id_without_models_prefix_is_used_verbatim() {
        // A `name` lacking the "models/" prefix is used as-is (unwrap_or branch).
        let body = br#"{"models":[{"name":"gemini-bare","inputTokenLimit":500000}]}"#;
        let base = spawn_mock_server(200, "OK", body).await;
        let provider = native_provider_with_base(&base);
        let models = provider.list_models().await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gemini-bare");
        assert_eq!(models[0].capabilities.max_context_tokens, 500_000);
    }

    #[tokio::test]
    async fn list_models_native_connection_error() {
        // `/openai` base resolves native, dead port → RequestFailed.
        let provider = GeminiProvider {
            client: reqwest::Client::new(),
            api_key: "test-key".to_string(),
            base_url: "http://127.0.0.1:19997/openai".to_string(),
            rate_limiter: None,
            capability_overrides: HashMap::new(),
        };
        let err = provider.list_models().await.unwrap_err();
        assert!(err.to_string().contains("Request failed:"));
    }

    #[tokio::test]
    async fn list_models_native_malformed_json_errors() {
        let base = spawn_mock_server(200, "OK", b"not json").await;
        let provider = native_provider_with_base(&base);
        let err = provider.list_models().await.unwrap_err();
        assert!(err.to_string().contains("Invalid response:"));
    }

    #[tokio::test]
    async fn list_models_native_non_success_status_errors() {
        // A non-2xx from the native endpoint propagates via check_http_response.
        let base = spawn_mock_server(401, "Unauthorized", b"bad key").await;
        let provider = native_provider_with_base(&base);
        let err = provider.list_models().await.unwrap_err();
        assert!(err.to_string().contains("401"));
    }

    #[tokio::test]
    async fn list_models_native_missing_models_field_errors() {
        let base = spawn_mock_server(200, "OK", b"{}").await;
        let provider = native_provider_with_base(&base);
        let err = provider.list_models().await.unwrap_err();
        assert!(err.to_string().contains("Invalid response:"));
    }

    #[tokio::test]
    async fn list_models_native_skips_entries_without_name() {
        // First entry has no `name` (get None), second's `name` is a non-string
        // (as_str None) - both filtered out; only the valid third survives.
        let body =
            br#"{"models":[{"no_name":true},{"name":123},{"name":"models/gemini-3-flash"}]}"#;
        let base = spawn_mock_server(200, "OK", body).await;
        let provider = native_provider_with_base(&base);
        let models = provider.list_models().await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gemini-3-flash");
    }

    #[test]
    fn native_base_strips_openai_suffix() {
        let provider = GeminiProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "k".to_string(),
        );
        assert_eq!(
            provider.native_base().as_deref(),
            Some("https://generativelanguage.googleapis.com/v1beta")
        );
    }

    #[test]
    fn native_base_none_for_nonstandard_url() {
        let provider = provider_with_url("http://proxy.local/custom".to_string());
        assert!(provider.native_base().is_none());
    }

    #[test]
    fn gemini_family_classification() {
        assert_eq!(
            GeminiFamily::classify("gemini-3.1-flash-lite"),
            GeminiFamily::FlashLite
        );
        assert_eq!(
            GeminiFamily::classify("gemini-3.1-pro-preview"),
            GeminiFamily::Pro
        );
        assert_eq!(
            GeminiFamily::classify("gemini-3.5-flash"),
            GeminiFamily::Flash
        );
        assert_eq!(GeminiFamily::classify("gemini-future"), GeminiFamily::Other);
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
        let limited = GeminiProvider::with_overrides(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "k".to_string(),
            HashMap::new(),
            Some(&cfg),
        );
        assert!(limited.rate_limiter.is_some());
        let unlimited = GeminiProvider::with_overrides(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "k".to_string(),
            HashMap::new(),
            None,
        );
        assert!(unlimited.rate_limiter.is_none());
    }
}
