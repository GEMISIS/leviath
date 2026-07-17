//! Provider trait and common types.

use async_trait::async_trait;
use futures_core::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use thiserror::Error;

/// Result type for provider operations.
pub type Result<T> = std::result::Result<T, ProviderError>;

/// Errors that can occur during provider operations.
#[derive(Error, Debug)]
pub enum ProviderError {
    /// HTTP request failed
    #[error("Request failed: {0}")]
    RequestFailed(String),

    /// API returned an error
    #[error("API error: {0}")]
    ApiError(String),

    /// Rate limit exceeded
    #[error("Rate limit exceeded")]
    RateLimitExceeded,

    /// Invalid response from provider
    #[error("Invalid response: {0}")]
    InvalidResponse(String),

    /// Token limit exceeded
    #[error("Token limit exceeded: {used} > {max}")]
    TokenLimitExceeded { used: usize, max: usize },

    /// Other error
    #[error("{0}")]
    Other(String),
}

/// Capabilities supported by a model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCapabilities {
    /// Whether the model supports temperature sampling
    pub supports_temperature: bool,

    /// Whether the model supports streaming responses
    pub supports_streaming: bool,

    /// Whether the model supports tool/function calling
    pub supports_tools: bool,

    /// Whether the model supports a system prompt
    pub supports_system_prompt: bool,

    /// Maximum number of context (input) tokens
    pub max_context_tokens: usize,

    /// Maximum number of output tokens
    pub max_output_tokens: usize,
}

impl Default for ModelCapabilities {
    fn default() -> Self {
        Self {
            supports_temperature: true,
            supports_streaming: true,
            supports_tools: true,
            supports_system_prompt: true,
            max_context_tokens: 8192,
            max_output_tokens: 4096,
        }
    }
}

/// Information about a model offered by a provider.
#[derive(Debug, Clone)]
pub struct ModelInfo {
    /// Model identifier used in API requests
    pub id: String,

    /// Human-readable name for the model
    pub display_name: Option<String>,

    /// Name of the provider that owns this model
    pub provider: String,

    /// Capabilities of this model
    pub capabilities: ModelCapabilities,
}

/// Rich message content: either a plain text string or structured content blocks.
///
/// Provider serialization converts this to the appropriate API format
/// (e.g., Anthropic content blocks, OpenAI message + tool_calls).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    /// Plain text content (backward compatible).
    Text(String),
    /// Structured content blocks (tool_use, tool_result, text).
    Blocks(Vec<ContentBlock>),
}

impl From<String> for MessageContent {
    fn from(s: String) -> Self {
        MessageContent::Text(s)
    }
}

impl From<&str> for MessageContent {
    fn from(s: &str) -> Self {
        MessageContent::Text(s.to_string())
    }
}

impl MessageContent {
    /// Get the plain text content, concatenating text blocks if needed.
    pub fn as_text(&self) -> String {
        match self {
            MessageContent::Text(s) => s.clone(),
            MessageContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(""),
        }
    }
}

/// A content block within a rich message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    /// A text content block.
    #[serde(rename = "text")]
    Text { text: String },
    /// A tool use request from the assistant.
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// A tool result from executing a tool.
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
}

/// A system prompt block, separated from conversation messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemBlock {
    /// The text content of this system block.
    pub text: String,
    /// Cache hint for this system block.
    pub cache_hint: leviath_core::CacheHint,
}

/// Request for LLM inference.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceRequest {
    /// System prompt blocks, separate from conversation messages.
    /// Providers that support a dedicated system prompt (Anthropic, OpenAI)
    /// will serialize these appropriately. Defaults to empty.
    #[serde(default)]
    pub system: Vec<SystemBlock>,

    /// The prompt or messages to send
    pub messages: Vec<Message>,

    /// Model to use
    pub model: String,

    /// Maximum tokens to generate
    pub max_tokens: usize,

    /// Temperature for sampling
    pub temperature: f32,

    /// Available tools
    pub tools: Vec<Tool>,

    /// Additional provider-specific parameters
    pub extra: serde_json::Value,
}

/// A message in a conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// Role (system, user, assistant)
    pub role: String,

    /// Message content — plain text or structured content blocks.
    pub content: MessageContent,

    /// If true, this message is a cache breakpoint -- the provider should
    /// mark everything up to and including this message as cacheable.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cache_breakpoint: bool,
}

/// A tool that can be called by the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    /// Tool name
    pub name: String,

    /// Tool description
    pub description: String,

    /// JSON schema for tool parameters
    pub parameters: serde_json::Value,
}

/// Response from LLM inference.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceResponse {
    /// The model's response text
    pub content: String,

    /// Tool calls requested by the model
    pub tool_calls: Vec<ToolCall>,

    /// Tokens used (prompt + completion)
    pub tokens_used: TokenUsage,

    /// Whether the response was complete or truncated
    pub finish_reason: FinishReason,
}

/// Token usage breakdown.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Tokens in the prompt
    pub prompt_tokens: usize,

    /// Tokens in the completion
    pub completion_tokens: usize,

    /// Total tokens
    pub total_tokens: usize,

    /// Tokens read from cache (Anthropic: cache_read_input_tokens)
    #[serde(default)]
    pub cached_tokens: usize,

    /// Tokens written to cache this request (Anthropic: cache_creation_input_tokens)
    #[serde(default)]
    pub cache_write_tokens: usize,
}

/// Reason inference completed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FinishReason {
    /// Normal completion
    Complete,

    /// Hit token limit
    TokenLimit,

    /// Model requested tool call
    ToolCall,

    /// Model requested stop
    Stop,
}

impl PartialEq for FinishReason {
    #[inline(never)]
    fn eq(&self, other: &Self) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
    }
}

/// A tool call from the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Tool call ID
    pub id: String,

    /// Tool name
    pub name: String,

    /// Tool arguments
    pub arguments: serde_json::Value,
}

/// A chunk from a streaming inference response.
#[derive(Debug, Clone)]
pub struct StreamChunk {
    /// Text delta
    pub delta: String,

    /// Partial tool call updates
    pub tool_calls: Vec<ToolCallDelta>,

    /// Token usage (usually only on final chunk)
    pub tokens: Option<TokenUsage>,

    /// Finish reason (only on final chunk)
    pub finish_reason: Option<FinishReason>,
}

/// A partial tool call update from streaming.
#[derive(Debug, Clone)]
pub struct ToolCallDelta {
    /// Index of the tool call being built
    pub index: usize,

    /// Tool call ID (sent on first delta for this index)
    pub id: Option<String>,

    /// Tool name (sent on first delta for this index)
    pub name: Option<String>,

    /// Partial arguments JSON string
    pub arguments_delta: String,
}

/// Configuration for a provider instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// API key for authentication
    pub api_key: String,

    /// Optional custom base URL
    pub base_url: Option<String>,

    /// Optional rate limit configuration
    pub rate_limit: Option<RateLimitConfig>,

    /// Optional request timeout in seconds.
    /// When set, the reqwest client will abort requests that exceed this duration.
    /// Default is None (no timeout).
    #[serde(default)]
    pub request_timeout_secs: Option<u64>,
}

/// Rate limit configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitConfig {
    /// Maximum requests per minute
    pub requests_per_minute: u32,

    /// Maximum tokens per minute
    pub tokens_per_minute: u32,
}

/// If no byte is received on a connection for this long, the request is
/// aborted. This is a *stall* timeout, not a total-duration cap: every
/// successful read resets it, so a legitimately slow streaming response (which
/// keeps sending bytes) is never cut off, while a connection where the server
/// accepted the request but produces no response bytes fails instead of hanging
/// forever. It is the backstop behind the connection-reuse fix in
/// [`build_http_client`]: if a connection ever goes silent mid-request, this
/// bounds the wait instead of hanging indefinitely.
const READ_STALL_TIMEOUT_SECS: u64 = 900;

/// Build a `reqwest::Client` for talking to an LLM HTTP API.
///
/// All providers should use this instead of `Client::new()`. It applies:
/// - **`pool_max_idle_per_host(0)`** — never reuse an idle connection. A large
///   request sent over a *reused* pooled connection to `api.anthropic.com`
///   stalls indefinitely (the server never responds), 100% reproducibly on some
///   setups, while the *same* large request over a *fresh* connection succeeds
///   (confirmed via `curl`: a 40KB POST on a fresh connection returns HTTP 200,
///   and small requests, which don't trigger the stall, share the pool fine).
///   It is transport-independent — it reproduces over both HTTP/2 and HTTP/1.1 —
///   so forcing a fresh connection per request, not the protocol, is the fix.
///   The cost is a TLS handshake per request, negligible for the sequential
///   request/response calls these providers make.
/// - a `read_timeout` (idle/stall timeout — see [`READ_STALL_TIMEOUT_SECS`]) as
///   a backstop so a stalled connection can never hang the process forever;
/// - TCP keep-alive.
///
/// `timeout_secs` (`ProviderConfig::request_timeout_secs`) adds an optional
/// hard cap on total request duration; when `None`, no total cap is applied and
/// the stall timeout above is the backstop (so it does not cap legitimately
/// long streaming responses).
pub fn build_http_client(timeout_secs: Option<u64>) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .connect_timeout(std::time::Duration::from_secs(30))
        .read_timeout(std::time::Duration::from_secs(READ_STALL_TIMEOUT_SECS))
        .tcp_keepalive(std::time::Duration::from_secs(30));

    if let Some(secs) = timeout_secs {
        builder = builder.timeout(std::time::Duration::from_secs(secs));
    }

    builder.build().expect("failed to build reqwest client")
}

/// Trait for LLM providers.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Execute inference with the given request.
    async fn infer(&self, request: InferenceRequest) -> Result<InferenceResponse>;

    /// Execute streaming inference with the given request.
    ///
    /// Returns a stream of chunks that can be consumed incrementally.
    /// Default implementation collects the full response from `infer()`.
    async fn infer_stream(
        &self,
        request: InferenceRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>> {
        let response = self.infer(request).await?;
        let chunk = StreamChunk {
            delta: response.content,
            tool_calls: response
                .tool_calls
                .iter()
                .enumerate()
                .map(|(i, tc)| ToolCallDelta {
                    index: i,
                    id: Some(tc.id.clone()),
                    name: Some(tc.name.clone()),
                    arguments_delta: tc.arguments.to_string(),
                })
                .collect(),
            tokens: Some(response.tokens_used),
            finish_reason: Some(response.finish_reason),
        };
        Ok(Box::pin(stream_once::once(Ok(chunk))))
    }

    /// Count tokens in the given text for this provider's models.
    fn count_tokens(&self, text: &str, model: &str) -> usize;

    /// Get the maximum context tokens for a model.
    fn max_context_tokens(&self, model: &str) -> usize;

    /// Get the provider name.
    fn name(&self) -> &str;

    /// Get the capabilities of the given model.
    fn capabilities(&self, model: &str) -> ModelCapabilities;

    /// List models available from this provider.
    ///
    /// Returns an empty list by default; providers may override to enumerate
    /// their available models.
    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        Ok(Vec::new())
    }
}

// ─── Shared provider helpers ─────────────────────────────────────────────────

/// Map an OpenAI-style `finish_reason` string to a `FinishReason`.
///
/// Used by both the OpenAI and OpenRouter providers which share the same
/// Chat Completions API response schema.
pub fn parse_openai_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "stop" => FinishReason::Complete,
        "tool_calls" => FinishReason::ToolCall,
        "length" => FinishReason::TokenLimit,
        _ => FinishReason::Complete,
    }
}

/// Check an HTTP response for errors and return it on success.
///
/// - On 429 (rate limit): notifies the optional rate limiter and returns `RateLimitExceeded`.
/// - On any other non-2xx: reads the body and returns `ApiError`.
/// - On 2xx: returns `Ok(response)` so the caller can read the body.
///
/// Pass the full `reqwest::Response`; it is returned back on success.
pub async fn check_http_response(
    response: reqwest::Response,
    limiter: Option<&crate::rate_limit::RateLimiter>,
) -> Result<reqwest::Response> {
    let status = response.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        // Extract retry-after *before* consuming the response body.
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        if let Some(l) = limiter {
            l.handle_rate_limit(retry_after).await;
        }
        return Err(ProviderError::RateLimitExceeded);
    }
    if !status.is_success() {
        let error_body = response.text().await.unwrap_or_else(|e| e.to_string());
        return Err(ProviderError::ApiError(format!(
            "HTTP {}: {}",
            status, error_body
        )));
    }
    Ok(response)
}

// Helper module for single-item streams
mod stream_once {
    use futures_core::Stream;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    pub struct Once<T> {
        item: Option<T>,
    }

    impl<T: Unpin> Stream for Once<T> {
        type Item = T;

        fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Ready(self.item.take())
        }
    }

    pub fn once<T>(item: T) -> Once<T> {
        Once { item: Some(item) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── ProviderError Display ──────────────────────────────────────────────

    #[test]
    fn provider_error_request_failed_display() {
        let err = ProviderError::RequestFailed("timeout".into());
        assert_eq!(err.to_string(), "Request failed: timeout");
    }

    #[test]
    fn provider_error_api_error_display() {
        let err = ProviderError::ApiError("bad request".into());
        assert_eq!(err.to_string(), "API error: bad request");
    }

    #[test]
    fn provider_error_rate_limit_display() {
        let err = ProviderError::RateLimitExceeded;
        assert_eq!(err.to_string(), "Rate limit exceeded");
    }

    #[test]
    fn provider_error_invalid_response_display() {
        let err = ProviderError::InvalidResponse("missing field".into());
        assert_eq!(err.to_string(), "Invalid response: missing field");
    }

    #[test]
    fn provider_error_token_limit_display() {
        let err = ProviderError::TokenLimitExceeded {
            used: 500,
            max: 100,
        };
        assert_eq!(err.to_string(), "Token limit exceeded: 500 > 100");
    }

    #[test]
    fn provider_error_other_display() {
        let err = ProviderError::Other("something went wrong".into());
        assert_eq!(err.to_string(), "something went wrong");
    }

    // ─── ModelCapabilities default ──────────────────────────────────────────

    #[test]
    fn model_capabilities_default() {
        let caps = ModelCapabilities::default();
        assert!(caps.supports_temperature);
        assert!(caps.supports_streaming);
        assert!(caps.supports_tools);
        assert!(caps.supports_system_prompt);
        assert_eq!(caps.max_context_tokens, 8192);
        assert_eq!(caps.max_output_tokens, 4096);
    }

    // ─── parse_openai_finish_reason ─────────────────────────────────────────

    #[test]
    fn parse_finish_reason_stop() {
        assert_eq!(parse_openai_finish_reason("stop"), FinishReason::Complete);
    }

    #[test]
    fn parse_finish_reason_tool_calls() {
        assert_eq!(
            parse_openai_finish_reason("tool_calls"),
            FinishReason::ToolCall
        );
    }

    #[test]
    fn parse_finish_reason_length() {
        assert_eq!(
            parse_openai_finish_reason("length"),
            FinishReason::TokenLimit
        );
    }

    #[test]
    fn parse_finish_reason_unknown_defaults_to_complete() {
        assert_eq!(
            parse_openai_finish_reason("unknown"),
            FinishReason::Complete
        );
    }

    // ─── Serialization round-trips ──────────────────────────────────────────

    #[test]
    fn token_usage_serde_roundtrip() {
        let usage = TokenUsage {
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            cached_tokens: 20,
            cache_write_tokens: 10,
        };
        let json = serde_json::to_string(&usage).unwrap();
        let back: TokenUsage = serde_json::from_str(&json).unwrap();
        assert_eq!(back.prompt_tokens, 100);
        assert_eq!(back.completion_tokens, 50);
        assert_eq!(back.total_tokens, 150);
        assert_eq!(back.cached_tokens, 20);
        assert_eq!(back.cache_write_tokens, 10);
    }

    #[test]
    fn token_usage_cached_defaults_to_zero() {
        let json = r#"{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}"#;
        let usage: TokenUsage = serde_json::from_str(json).unwrap();
        assert_eq!(usage.cached_tokens, 0);
        assert_eq!(usage.cache_write_tokens, 0);
    }

    #[test]
    fn message_cache_breakpoint_skipped_when_false() {
        let msg = Message {
            role: "user".into(),
            content: "hello".into(),
            cache_breakpoint: false,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert!(json.get("cache_breakpoint").is_none());
    }

    #[test]
    fn message_cache_breakpoint_included_when_true() {
        let msg = Message {
            role: "system".into(),
            content: "you are helpful".into(),
            cache_breakpoint: true,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["cache_breakpoint"], true);
    }

    #[test]
    fn inference_request_serde_roundtrip() {
        let req = InferenceRequest {
            system: vec![],
            messages: vec![Message {
                role: "user".into(),
                content: "hi".into(),
                cache_breakpoint: false,
            }],
            model: "gpt-4".into(),
            max_tokens: 100,
            temperature: 0.7,
            tools: vec![Tool {
                name: "search".into(),
                description: "Search the web".into(),
                parameters: serde_json::json!({"type": "object"}),
            }],
            extra: serde_json::json!({}),
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: InferenceRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.model, "gpt-4");
        assert_eq!(back.messages.len(), 1);
        assert_eq!(back.tools.len(), 1);
        assert_eq!(back.tools[0].name, "search");
    }

    #[test]
    fn tool_call_serde_roundtrip() {
        let tc = ToolCall {
            id: "call_123".into(),
            name: "get_weather".into(),
            arguments: serde_json::json!({"city": "NYC"}),
        };
        let json = serde_json::to_string(&tc).unwrap();
        let back: ToolCall = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "call_123");
        assert_eq!(back.name, "get_weather");
        assert_eq!(back.arguments["city"], "NYC");
    }

    #[test]
    fn finish_reason_serde_roundtrip() {
        for reason in [
            FinishReason::Complete,
            FinishReason::TokenLimit,
            FinishReason::ToolCall,
            FinishReason::Stop,
        ] {
            let json = serde_json::to_string(&reason).unwrap();
            let back: FinishReason = serde_json::from_str(&json).unwrap();
            assert_eq!(format!("{:?}", reason), format!("{:?}", back));
        }
    }

    #[test]
    fn rate_limit_config_serde() {
        let cfg = RateLimitConfig {
            requests_per_minute: 60,
            tokens_per_minute: 100_000,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: RateLimitConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.requests_per_minute, 60);
        assert_eq!(back.tokens_per_minute, 100_000);
    }

    #[test]
    fn provider_config_serde_roundtrip() {
        let cfg = ProviderConfig {
            api_key: "sk-test".into(),
            base_url: Some("https://api.example.com".into()),
            rate_limit: Some(RateLimitConfig {
                requests_per_minute: 30,
                tokens_per_minute: 50_000,
            }),
            request_timeout_secs: None,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: ProviderConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.api_key, "sk-test");
        assert_eq!(back.base_url.as_deref(), Some("https://api.example.com"));
        assert!(back.rate_limit.is_some());
    }

    #[test]
    fn provider_config_optional_fields_default_to_none() {
        let json = r#"{"api_key":"sk-test"}"#;
        let cfg: ProviderConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.api_key, "sk-test");
        assert!(cfg.base_url.is_none());
        assert!(cfg.rate_limit.is_none());
    }

    // ─── stream_once ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn stream_once_yields_single_item() {
        use futures_core::Stream;
        use std::pin::Pin;
        use std::task::{Context, Poll, Waker};

        let mut stream = stream_once::once(42);
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);

        assert_eq!(
            Pin::new(&mut stream).poll_next(&mut cx),
            Poll::Ready(Some(42))
        );
        assert_eq!(Pin::new(&mut stream).poll_next(&mut cx), Poll::Ready(None));
    }

    // ─── Default trait method impls (infer_stream, list_models) ────────────

    struct MinimalProvider;

    #[async_trait]
    impl Provider for MinimalProvider {
        async fn infer(&self, _request: InferenceRequest) -> Result<InferenceResponse> {
            Ok(InferenceResponse {
                content: "hello".to_string(),
                tool_calls: vec![ToolCall {
                    id: "call_1".to_string(),
                    name: "search".to_string(),
                    arguments: serde_json::json!({"q": "rust"}),
                }],
                tokens_used: TokenUsage {
                    prompt_tokens: 1,
                    completion_tokens: 1,
                    total_tokens: 2,
                    cached_tokens: 0,
                    cache_write_tokens: 0,
                },
                finish_reason: FinishReason::Complete,
            })
        }

        fn count_tokens(&self, text: &str, _model: &str) -> usize {
            text.len()
        }

        fn max_context_tokens(&self, _model: &str) -> usize {
            1000
        }

        fn name(&self) -> &str {
            "minimal"
        }

        fn capabilities(&self, _model: &str) -> ModelCapabilities {
            ModelCapabilities::default()
        }
    }

    #[tokio::test]
    async fn default_infer_stream_yields_single_chunk_from_infer() {
        use tokio_stream::StreamExt;

        let provider = MinimalProvider;
        let request = InferenceRequest {
            system: vec![],
            messages: vec![],
            model: "any".to_string(),
            max_tokens: 10,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::Value::Null,
        };
        let mut stream = provider.infer_stream(request).await.unwrap();
        let chunk = stream.next().await.unwrap().unwrap();
        assert_eq!(chunk.delta, "hello");
        assert_eq!(chunk.tool_calls.len(), 1);
        assert_eq!(chunk.tool_calls[0].index, 0);
        assert_eq!(chunk.tool_calls[0].id.as_deref(), Some("call_1"));
        assert_eq!(chunk.tool_calls[0].name.as_deref(), Some("search"));
        assert_eq!(chunk.tokens.as_ref().unwrap().total_tokens, 2);
        assert_eq!(chunk.finish_reason, Some(FinishReason::Complete));
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn default_list_models_returns_empty() {
        let provider = MinimalProvider;
        let models = provider.list_models().await.unwrap();
        assert!(models.is_empty());
    }

    #[test]
    fn minimal_provider_trait_accessors() {
        let provider = MinimalProvider;
        assert_eq!(provider.count_tokens("hello", "any"), 5);
        assert_eq!(provider.max_context_tokens("any"), 1000);
        assert_eq!(provider.name(), "minimal");
        assert_eq!(
            provider.capabilities("any").max_context_tokens,
            ModelCapabilities::default().max_context_tokens
        );
    }

    // ─── check_http_response ────────────────────────────────────────────────

    async fn spawn_mock_response(
        status: u16,
        reason: &str,
        headers: &[(&str, &str)],
        body: &'static [u8],
    ) -> reqwest::Response {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut header_lines = format!(
            "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
            status,
            reason,
            body.len()
        );
        for (k, v) in headers {
            header_lines.push_str(&format!("{}: {}\r\n", k, v));
        }
        header_lines.push_str("\r\n");
        let response_bytes = header_lines.into_bytes();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = [0u8; 8192];
            let _ = socket.read(&mut buf).await;
            let _ = socket.write_all(&response_bytes).await;
            let _ = socket.write_all(body).await;
            let _ = socket.flush().await;
            let _ = socket.shutdown().await;
        });
        reqwest::get(format!("http://{}", addr)).await.unwrap()
    }

    async fn spawn_truncated_error_response(status: u16, reason: &str) -> reqwest::Response {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let header = format!(
            "HTTP/1.1 {} {}\r\nContent-Length: 9999\r\nConnection: close\r\n\r\nshort",
            status, reason
        )
        .into_bytes();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = [0u8; 8192];
            let _ = socket.read(&mut buf).await;
            let _ = socket.write_all(&header).await;
            let _ = socket.flush().await;
            let _ = socket.shutdown().await;
        });
        reqwest::get(format!("http://{}", addr)).await.unwrap()
    }

    #[tokio::test]
    async fn check_http_response_success_returns_response() {
        let response = spawn_mock_response(200, "OK", &[], b"ok").await;
        let result = check_http_response(response, None).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn check_http_response_non_success_returns_api_error() {
        let response = spawn_mock_response(500, "Internal Server Error", &[], b"boom").await;
        let err = check_http_response(response, None).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("500"));
        assert!(msg.contains("boom"));
    }

    fn assert_contains_500(msg: &str) {
        assert!(msg.contains("500"), "expected 500 in: {msg}");
    }

    #[test]
    #[should_panic(expected = "expected 500 in: not the status you're looking for")]
    fn assert_contains_500_panics_when_missing() {
        assert_contains_500("not the status you're looking for");
    }

    #[tokio::test]
    async fn check_http_response_non_success_body_read_error_falls_back_to_error_string() {
        let response = spawn_truncated_error_response(500, "Internal Server Error").await;
        let err = check_http_response(response, None).await.unwrap_err();
        let msg = err.to_string();
        assert_contains_500(&msg);
    }

    #[tokio::test]
    async fn check_http_response_rate_limited_without_limiter_returns_rate_limit_exceeded() {
        let response = spawn_mock_response(429, "Too Many Requests", &[], b"slow down").await;
        let err = check_http_response(response, None).await.unwrap_err();
        assert_eq!(
            std::mem::discriminant(&err),
            std::mem::discriminant(&ProviderError::RateLimitExceeded)
        );
    }

    #[tokio::test]
    async fn check_http_response_rate_limited_with_retry_after_notifies_limiter() {
        use crate::rate_limit::RateLimiter;
        let limiter = RateLimiter::new(&RateLimitConfig {
            requests_per_minute: 60,
            tokens_per_minute: 100_000,
        });
        let response = spawn_mock_response(
            429,
            "Too Many Requests",
            &[("retry-after", "2")],
            b"slow down",
        )
        .await;
        let err = check_http_response(response, Some(&limiter))
            .await
            .unwrap_err();
        assert_eq!(
            std::mem::discriminant(&err),
            std::mem::discriminant(&ProviderError::RateLimitExceeded)
        );
    }

    #[tokio::test]
    async fn check_http_response_rate_limited_with_non_numeric_retry_after_is_ignored() {
        use crate::rate_limit::RateLimiter;
        let limiter = RateLimiter::new(&RateLimitConfig {
            requests_per_minute: 60,
            tokens_per_minute: 100_000,
        });
        let response = spawn_mock_response(
            429,
            "Too Many Requests",
            &[("retry-after", "not-a-number")],
            b"slow down",
        )
        .await;
        let err = check_http_response(response, Some(&limiter))
            .await
            .unwrap_err();
        assert_eq!(
            std::mem::discriminant(&err),
            std::mem::discriminant(&ProviderError::RateLimitExceeded)
        );
    }

    // ─── build_http_client ─────────────────────────────────────────────────

    #[test]
    fn build_http_client_with_timeout() {
        let client = build_http_client(Some(30));
        // Should successfully build a client; we cannot inspect the timeout
        // directly, but confirming it doesn't panic is the coverage goal.
        drop(client);
    }

    #[test]
    fn build_http_client_without_timeout() {
        let client = build_http_client(None);
        drop(client);
    }

    /// Regression for the read_files hang: a connection where the server
    /// accepts the request but never sends a response must ERROR (via the
    /// stall/read timeout), not block forever. This is the exact shape of the
    /// hang the user hit — a large request accepted by Anthropic (h2
    /// WindowUpdate seen) with no response ever returned. Uses a 2s read
    /// timeout so the test is fast; the production default is 300s.
    #[tokio::test]
    async fn read_timeout_aborts_a_connection_that_never_responds() {
        use std::time::{Duration, Instant};
        use tokio::io::AsyncReadExt;

        // Server: accept one connection, drain the request, then hold the
        // socket open forever without writing any response.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                // Read until the peer goes away; never respond.
                while let Ok(n) = sock.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                }
            }
        });

        // Mirror build_http_client's stall protection but with a short read
        // timeout. If read_timeout were defeated (e.g. by keep-alive) this
        // send would hang and the test would time out instead of asserting.
        let client = reqwest::Client::builder()
            .read_timeout(Duration::from_secs(2))
            .http2_keep_alive_interval(Duration::from_secs(30))
            .http2_keep_alive_timeout(Duration::from_secs(20))
            .http2_keep_alive_while_idle(true)
            .tcp_keepalive(Duration::from_secs(60))
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .unwrap();

        let start = Instant::now();
        let result = client
            .post(format!("http://{addr}/"))
            .body("request body that never gets a response")
            .send()
            .await;
        let elapsed = start.elapsed();

        assert!(
            result.is_err(),
            "a silent server must yield an error, not a response"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "read_timeout should abort at ~2s; took {elapsed:?} (it did not fire)"
        );
    }

    // ─── MessageContent::as_text for Blocks variant ────────────────────────

    #[test]
    fn message_content_as_text_blocks_mixed() {
        let content = MessageContent::Blocks(vec![
            ContentBlock::Text {
                text: "hello ".to_string(),
            },
            ContentBlock::ToolUse {
                id: "call_1".to_string(),
                name: "search".to_string(),
                input: serde_json::json!({}),
            },
            ContentBlock::Text {
                text: "world".to_string(),
            },
            ContentBlock::ToolResult {
                tool_use_id: "call_1".to_string(),
                content: "result".to_string(),
                is_error: false,
            },
        ]);
        // Only Text blocks are concatenated; ToolUse and ToolResult are skipped.
        assert_eq!(content.as_text(), "hello world");
    }

    // ─── MessageContent::from &str ─────────────────────────────────────────

    #[test]
    fn message_content_from_str_ref() {
        let content: MessageContent = "hi there".into();
        assert!(matches!(&content, MessageContent::Text(s) if s == "hi there"));
    }

    // ─── FinishReason equality ─────────────────────────────────────────────

    #[test]
    fn finish_reason_stop_eq_stop() {
        assert_eq!(FinishReason::Stop, FinishReason::Stop);
    }

    #[test]
    fn finish_reason_different_variants_not_eq() {
        assert_ne!(FinishReason::Stop, FinishReason::Complete);
        assert_ne!(FinishReason::TokenLimit, FinishReason::ToolCall);
    }
}
