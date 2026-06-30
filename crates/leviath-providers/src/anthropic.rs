//! Anthropic Claude provider implementation.

use crate::provider::{
    FinishReason, InferenceRequest, InferenceResponse, ModelCapabilities, ModelInfo, Provider,
    ProviderConfig, ProviderError, Result, StreamChunk, TokenUsage, ToolCall, ToolCallDelta,
};
use crate::rate_limit::RateLimiter;
use async_trait::async_trait;
use futures_core::Stream;
use reqwest::Client;
use std::collections::HashMap;
use std::pin::Pin;

/// Anthropic Claude provider.
pub struct AnthropicProvider {
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

impl AnthropicProvider {
    /// Create a new Anthropic provider.
    pub fn new(api_key: String) -> Self {
        Self {
            client: Client::new(),
            api_key,
            base_url: "https://api.anthropic.com/v1".to_string(),
            rate_limiter: None,
            capability_overrides: HashMap::new(),
        }
    }

    /// Create a new Anthropic provider with full configuration.
    pub fn with_config(config: ProviderConfig) -> Self {
        let rate_limiter = config.rate_limit.as_ref().map(RateLimiter::new);
        Self {
            client: Client::new(),
            api_key: config.api_key,
            base_url: config
                .base_url
                .unwrap_or_else(|| "https://api.anthropic.com/v1".to_string()),
            rate_limiter,
            capability_overrides: HashMap::new(),
        }
    }

    /// Create a new Anthropic provider with per-model capability overrides.
    pub fn with_overrides(api_key: String, overrides: HashMap<String, ModelCapabilities>) -> Self {
        Self {
            client: Client::new(),
            api_key,
            base_url: "https://api.anthropic.com/v1".to_string(),
            rate_limiter: None,
            capability_overrides: overrides,
        }
    }

    /// Return built-in capabilities for a model based on its name pattern.
    fn builtin_capabilities(&self, model: &str) -> ModelCapabilities {
        // Fable 5 / Mythos 5 — top-tier, 1M context, 128K output, no temperature
        if model.contains("claude-fable-5") || model.contains("claude-mythos-5") {
            return ModelCapabilities {
                supports_temperature: false,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 1_000_000,
                max_output_tokens: 128_000,
            };
        }
        // Opus 4.8 / 4.7 — 1M context, 128K output, no temperature
        if model.contains("claude-opus-4-8") || model.contains("claude-opus-4-7") {
            return ModelCapabilities {
                supports_temperature: false,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 1_000_000,
                max_output_tokens: 128_000,
            };
        }
        // Opus 4.6 — 1M context, 128K output, temperature supported
        if model.contains("claude-opus-4-6") {
            return ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 1_000_000,
                max_output_tokens: 128_000,
            };
        }
        // Sonnet 4.6 — 1M context, 128K output, temperature supported
        if model.contains("claude-sonnet-4-6") {
            return ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 1_000_000,
                max_output_tokens: 128_000,
            };
        }
        // Haiku 4.5 — 200K context, 64K output, temperature supported
        if model.contains("claude-haiku-4-5") {
            return ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 200_000,
                max_output_tokens: 65_536,
            };
        }
        // Generic Claude 4.x fallback (e.g. older 4.5 snapshots)
        if model.contains("claude-opus-4")
            || model.contains("claude-sonnet-4")
            || model.contains("claude-haiku-4")
        {
            return ModelCapabilities {
                supports_temperature: false,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 1_000_000,
                max_output_tokens: 32_768,
            };
        }
        ModelCapabilities::default()
    }

    /// Build the request body for the Anthropic API.
    fn build_request_body(&self, request: &InferenceRequest) -> serde_json::Value {
        // Extract system messages and non-system messages
        let mut system_parts: Vec<serde_json::Value> = Vec::new();
        let mut messages: Vec<serde_json::Value> = Vec::new();

        // Track cache breakpoints — Anthropic allows max 4.
        let mut breakpoint_count = 0;
        const MAX_BREAKPOINTS: usize = 4;

        for msg in &request.messages {
            if msg.role == "system" {
                system_parts.push(serde_json::json!({
                    "type": "text",
                    "text": msg.content,
                    "cache_control": { "type": "ephemeral" }
                }));
            } else if msg.cache_breakpoint && breakpoint_count < MAX_BREAKPOINTS {
                breakpoint_count += 1;
                messages.push(serde_json::json!({
                    "role": msg.role,
                    "content": [{
                        "type": "text",
                        "text": msg.content,
                        "cache_control": { "type": "ephemeral" }
                    }],
                }));
            } else {
                messages.push(serde_json::json!({
                    "role": msg.role,
                    "content": msg.content,
                }));
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

        // Add system prompt as top-level field
        if system_parts.len() == 1 {
            // Single system message — use simple string form
            if let Some(text) = system_parts[0].get("text").and_then(|t| t.as_str()) {
                body["system"] = serde_json::Value::String(text.to_string());
            }
        } else if system_parts.len() > 1 {
            body["system"] = serde_json::Value::Array(system_parts);
        }

        // Add tools if present
        if !request.tools.is_empty() {
            let tools: Vec<serde_json::Value> = request
                .tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.parameters,
                    })
                })
                .collect();
            body["tools"] = serde_json::Value::Array(tools);
        }

        body
    }

    /// Parse a stop reason string into a FinishReason.
    fn parse_stop_reason(reason: &str) -> FinishReason {
        match reason {
            "end_turn" => FinishReason::Complete,
            "tool_use" => FinishReason::ToolCall,
            "max_tokens" => FinishReason::TokenLimit,
            "stop_sequence" => FinishReason::Stop,
            _ => FinishReason::Complete,
        }
    }

    /// Parse the API response body.
    fn parse_response(&self, body: &serde_json::Value) -> Result<InferenceResponse> {
        let mut content = String::new();
        let mut tool_calls = Vec::new();

        if let Some(content_blocks) = body.get("content").and_then(|c| c.as_array()) {
            for block in content_blocks {
                match block.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                            content.push_str(text);
                        }
                    }
                    Some("tool_use") => {
                        let id = block
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = block
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let arguments = block
                            .get("input")
                            .cloned()
                            .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
                        tool_calls.push(ToolCall {
                            id,
                            name,
                            arguments,
                        });
                    }
                    _ => {}
                }
            }
        }

        let usage = body.get("usage");
        let prompt_tokens = usage
            .and_then(|u| u.get("input_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let completion_tokens = usage
            .and_then(|u| u.get("output_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;

        let stop_reason = body
            .get("stop_reason")
            .and_then(|v| v.as_str())
            .unwrap_or("end_turn");

        let cached_tokens = usage
            .and_then(|u| u.get("cache_read_input_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let cache_write_tokens = usage
            .and_then(|u| u.get("cache_creation_input_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;

        Ok(InferenceResponse {
            content,
            tool_calls,
            tokens_used: TokenUsage {
                prompt_tokens,
                completion_tokens,
                total_tokens: prompt_tokens + completion_tokens,
                cached_tokens,
                cache_write_tokens,
            },
            finish_reason: Self::parse_stop_reason(stop_reason),
        })
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    async fn infer(&self, request: InferenceRequest) -> Result<InferenceResponse> {
        tracing::debug!(model = %request.model, "Calling Anthropic API");

        if let Some(limiter) = &self.rate_limiter {
            limiter.acquire().await?;
        }

        let body = self.build_request_body(&request);

        let response = self
            .client
            .post(format!("{}/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
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
            limiter.record_tokens(result.tokens_used.total_tokens).await;
        }

        Ok(result)
    }

    async fn infer_stream(
        &self,
        request: InferenceRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>> {
        tracing::debug!(model = %request.model, "Calling Anthropic API (streaming)");

        if let Some(limiter) = &self.rate_limiter {
            limiter.acquire().await?;
        }

        let mut body = self.build_request_body(&request);
        body["stream"] = serde_json::Value::Bool(true);

        let response = self
            .client
            .post(format!("{}/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
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

        let byte_stream = response.bytes_stream();
        let stream = AnthropicSseStream::new(byte_stream);

        Ok(Box::pin(stream))
    }

    fn count_tokens(&self, text: &str, _model: &str) -> usize {
        // Anthropic-specific: ~3.5 chars per token (more efficient than GPT)
        (text.len() as f32 / 3.5) as usize
    }

    fn max_context_tokens(&self, model: &str) -> usize {
        self.capabilities(model).max_context_tokens
    }

    fn name(&self) -> &str {
        "anthropic"
    }

    fn capabilities(&self, model: &str) -> ModelCapabilities {
        if let Some(overridden) = self.capability_overrides.get(model) {
            overridden.clone()
        } else {
            self.builtin_capabilities(model)
        }
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let response = self
            .client
            .get(format!("{}/models", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .send()
            .await
            .map_err(|e| ProviderError::RequestFailed(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let error_body = response
                .text()
                .await
                .unwrap_or_else(|_| "unknown error".to_string());
            return Err(ProviderError::RequestFailed(format!(
                "HTTP {}: {}",
                status, error_body
            )));
        }

        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| ProviderError::RequestFailed(e.to_string()))?;

        let data = body.get("data").and_then(|d| d.as_array()).ok_or_else(|| {
            ProviderError::RequestFailed("missing 'data' field in /models response".to_string())
        })?;

        let models = data
            .iter()
            .filter_map(|entry| {
                let id = entry.get("id").and_then(|v| v.as_str())?.to_string();
                let display_name = entry
                    .get("display_name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let capabilities = self.capabilities(&id);
                Some(ModelInfo {
                    id,
                    display_name,
                    provider: "anthropic".to_string(),
                    capabilities,
                })
            })
            .collect();

        Ok(models)
    }
}

// SSE stream parser for Anthropic's streaming API.
pin_project_lite::pin_project! {
    struct AnthropicSseStream<S> {
        #[pin]
        inner: S,
        buffer: String,
        current_tool_index: usize,
    }
}

impl<S> AnthropicSseStream<S> {
    fn new(inner: S) -> Self {
        Self {
            inner,
            buffer: String::new(),
            current_tool_index: 0,
        }
    }
}

impl<S> Stream for AnthropicSseStream<S>
where
    S: Stream<Item = std::result::Result<bytes::Bytes, reqwest::Error>>,
{
    type Item = Result<StreamChunk>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let mut this = self.project();

        loop {
            // Check if we have complete SSE events in the buffer
            if let Some(chunk) = parse_sse_event(this.buffer, this.current_tool_index) {
                return std::task::Poll::Ready(Some(Ok(chunk)));
            }

            // Try to get more data
            match this.inner.as_mut().poll_next(cx) {
                std::task::Poll::Ready(Some(Ok(bytes))) => {
                    if let Ok(text) = std::str::from_utf8(&bytes) {
                        this.buffer.push_str(text);
                    }
                }
                std::task::Poll::Ready(Some(Err(e))) => {
                    return std::task::Poll::Ready(Some(Err(ProviderError::RequestFailed(
                        e.to_string(),
                    ))));
                }
                std::task::Poll::Ready(None) => {
                    // Stream ended — try to parse any remaining data
                    if let Some(chunk) = parse_sse_event(this.buffer, this.current_tool_index) {
                        return std::task::Poll::Ready(Some(Ok(chunk)));
                    }
                    return std::task::Poll::Ready(None);
                }
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
        }
    }
}

/// Parse a single SSE event from the buffer, consuming it if found.
fn parse_sse_event(buffer: &mut String, tool_index: &mut usize) -> Option<StreamChunk> {
    // Look for a complete event (double newline)
    let event_end = buffer.find("\n\n")?;
    let event_text = buffer[..event_end].to_string();
    *buffer = buffer[event_end + 2..].to_string();

    // Parse event type and data
    let mut event_type = String::new();
    let mut data = String::new();

    for line in event_text.lines() {
        if let Some(et) = line.strip_prefix("event: ") {
            event_type = et.to_string();
        } else if let Some(d) = line.strip_prefix("data: ") {
            data = d.to_string();
        }
    }

    if data.is_empty() {
        return None;
    }

    let json: serde_json::Value = serde_json::from_str(&data).ok()?;

    match event_type.as_str() {
        "content_block_delta" => {
            let delta = json.get("delta")?;
            match delta.get("type").and_then(|t| t.as_str()) {
                Some("text_delta") => {
                    let text = delta.get("text").and_then(|t| t.as_str()).unwrap_or("");
                    Some(StreamChunk {
                        delta: text.to_string(),
                        tool_calls: Vec::new(),
                        tokens: None,
                        finish_reason: None,
                    })
                }
                Some("input_json_delta") => {
                    let partial = delta
                        .get("partial_json")
                        .and_then(|t| t.as_str())
                        .unwrap_or("");
                    Some(StreamChunk {
                        delta: String::new(),
                        tool_calls: vec![ToolCallDelta {
                            index: *tool_index,
                            id: None,
                            name: None,
                            arguments_delta: partial.to_string(),
                        }],
                        tokens: None,
                        finish_reason: None,
                    })
                }
                _ => None,
            }
        }
        "content_block_start" => {
            let content_block = json.get("content_block")?;
            if content_block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                let id = content_block
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = content_block
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let idx = *tool_index;
                *tool_index += 1;
                Some(StreamChunk {
                    delta: String::new(),
                    tool_calls: vec![ToolCallDelta {
                        index: idx,
                        id: Some(id),
                        name: Some(name),
                        arguments_delta: String::new(),
                    }],
                    tokens: None,
                    finish_reason: None,
                })
            } else {
                None
            }
        }
        "message_delta" => {
            let stop_reason = json
                .get("delta")
                .and_then(|d| d.get("stop_reason"))
                .and_then(|s| s.as_str())
                .unwrap_or("end_turn");

            let usage = json.get("usage");
            let output_tokens = usage
                .and_then(|u| u.get("output_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;

            Some(StreamChunk {
                delta: String::new(),
                tool_calls: Vec::new(),
                tokens: Some(TokenUsage {
                    prompt_tokens: 0,
                    completion_tokens: output_tokens,
                    total_tokens: output_tokens,
                    cached_tokens: 0,
                    cache_write_tokens: 0,
                }),
                finish_reason: Some(AnthropicProvider::parse_stop_reason(stop_reason)),
            })
        }
        "message_start" => {
            // Extract input token count from message_start
            let usage = json.get("message").and_then(|m| m.get("usage"));
            let input_tokens = usage
                .and_then(|u| u.get("input_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            let cached = usage
                .and_then(|u| u.get("cache_read_input_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            let cache_write = usage
                .and_then(|u| u.get("cache_creation_input_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;

            if input_tokens > 0 || cached > 0 || cache_write > 0 {
                Some(StreamChunk {
                    delta: String::new(),
                    tool_calls: Vec::new(),
                    tokens: Some(TokenUsage {
                        prompt_tokens: input_tokens,
                        completion_tokens: 0,
                        total_tokens: input_tokens,
                        cached_tokens: cached,
                        cache_write_tokens: cache_write,
                    }),
                    finish_reason: None,
                })
            } else {
                None
            }
        }
        "message_stop" | "ping" => None,
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_creation() {
        let provider = AnthropicProvider::new("test-key".to_string());
        assert_eq!(provider.name(), "anthropic");
    }

    #[test]
    fn test_context_limits() {
        let provider = AnthropicProvider::new("test-key".to_string());
        assert_eq!(provider.max_context_tokens("claude-sonnet-4-6"), 1_000_000);
    }

    #[test]
    fn test_build_request_body() {
        let provider = AnthropicProvider::new("test-key".to_string());
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
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
        };

        let body = provider.build_request_body(&request);
        assert_eq!(body["model"], "claude-sonnet-4-6");
        assert_eq!(body["system"], "You are helpful.");
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_parse_response() {
        let provider = AnthropicProvider::new("test-key".to_string());
        let body = serde_json::json!({
            "content": [
                { "type": "text", "text": "Hello!" }
            ],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5
            }
        });

        let response = provider.parse_response(&body).unwrap();
        assert_eq!(response.content, "Hello!");
        assert_eq!(response.tokens_used.prompt_tokens, 10);
        assert_eq!(response.tokens_used.completion_tokens, 5);
        assert!(matches!(response.finish_reason, FinishReason::Complete));
    }

    #[test]
    fn test_parse_response_with_tool_calls() {
        let provider = AnthropicProvider::new("test-key".to_string());
        let body = serde_json::json!({
            "content": [
                { "type": "text", "text": "Let me search." },
                {
                    "type": "tool_use",
                    "id": "toolu_123",
                    "name": "search",
                    "input": { "query": "rust" }
                }
            ],
            "stop_reason": "tool_use",
            "usage": { "input_tokens": 20, "output_tokens": 15 }
        });

        let response = provider.parse_response(&body).unwrap();
        assert_eq!(response.content, "Let me search.");
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].name, "search");
        assert!(matches!(response.finish_reason, FinishReason::ToolCall));
    }

    #[test]
    fn test_builtin_capabilities_opus48() {
        let provider = AnthropicProvider::new("test-key".to_string());
        let caps = provider.builtin_capabilities("claude-opus-4-8");
        assert!(!caps.supports_temperature);
        assert!(caps.supports_streaming);
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 1_000_000);
        assert_eq!(caps.max_output_tokens, 128_000);
    }

    #[test]
    fn test_builtin_capabilities_sonnet46() {
        let provider = AnthropicProvider::new("test-key".to_string());
        let caps = provider.builtin_capabilities("claude-sonnet-4-6");
        assert!(caps.supports_temperature);
        assert!(caps.supports_streaming);
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 1_000_000);
        assert_eq!(caps.max_output_tokens, 128_000);
    }

    #[test]
    fn test_builtin_capabilities_haiku45() {
        let provider = AnthropicProvider::new("test-key".to_string());
        let caps = provider.builtin_capabilities("claude-haiku-4-5-20251001");
        assert!(caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 200_000);
        assert_eq!(caps.max_output_tokens, 65_536);
    }

    #[test]
    fn test_builtin_capabilities_fable5() {
        let provider = AnthropicProvider::new("test-key".to_string());
        let caps = provider.builtin_capabilities("claude-fable-5");
        assert!(!caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 1_000_000);
        assert_eq!(caps.max_output_tokens, 128_000);
    }

    #[test]
    fn test_builtin_capabilities_sonnet_46() {
        let provider = AnthropicProvider::new("test-key".to_string());
        let caps = provider.builtin_capabilities("claude-sonnet-4-6");
        assert!(caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 1_000_000);
        assert_eq!(caps.max_output_tokens, 128_000);
    }

    #[test]
    fn test_capability_overrides() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "claude-sonnet-4-6".to_string(),
            ModelCapabilities {
                supports_temperature: false,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 1_000_000,
                max_output_tokens: 32_768,
            },
        );
        let provider = AnthropicProvider::with_overrides("test-key".to_string(), overrides);
        let caps = provider.capabilities("claude-sonnet-4-6");
        // Override should take precedence over built-in
        assert!(!caps.supports_temperature);
        assert_eq!(caps.max_output_tokens, 32_768);
    }

    #[test]
    fn test_build_request_body_with_cache_breakpoint() {
        let provider = AnthropicProvider::new("test-key".to_string());
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
                    cache_breakpoint: true,
                },
                crate::provider::Message {
                    role: "user".to_string(),
                    content: "World".to_string(),
                    cache_breakpoint: false,
                },
            ],
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
        };

        let body = provider.build_request_body(&request);
        let messages = body["messages"].as_array().unwrap();

        // First non-system message has cache_breakpoint: true
        let first_msg = &messages[0];
        assert!(
            first_msg.get("content").unwrap().is_array(),
            "Cache breakpoint message should use content blocks array"
        );
        let content_block = &first_msg["content"][0];
        assert_eq!(
            content_block["cache_control"]["type"], "ephemeral",
            "Cache breakpoint should have cache_control"
        );

        // Second non-system message has no cache_breakpoint
        let second_msg = &messages[1];
        assert!(
            second_msg.get("content").unwrap().is_string(),
            "Non-breakpoint message should use simple string content"
        );
    }

    #[test]
    fn test_build_request_body_max_4_breakpoints() {
        let provider = AnthropicProvider::new("test-key".to_string());
        let mut messages: Vec<crate::provider::Message> = (0..6)
            .map(|i| crate::provider::Message {
                role: "user".to_string(),
                content: format!("Message {}", i),
                cache_breakpoint: true,
            })
            .collect();
        // Add a system message
        messages.insert(
            0,
            crate::provider::Message {
                role: "system".to_string(),
                content: "System".to_string(),
                cache_breakpoint: false,
            },
        );

        let request = InferenceRequest {
            messages,
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
        };

        let body = provider.build_request_body(&request);
        let msgs = body["messages"].as_array().unwrap();

        // Count messages that have content blocks with cache_control
        let bp_count = msgs
            .iter()
            .filter(|m| {
                m.get("content")
                    .and_then(|c| c.as_array())
                    .and_then(|arr| arr.first())
                    .and_then(|block| block.get("cache_control"))
                    .is_some()
            })
            .count();
        assert_eq!(bp_count, 4, "Should cap at 4 cache breakpoints");
    }

    #[test]
    fn test_parse_response_with_cache_metrics() {
        let provider = AnthropicProvider::new("test-key".to_string());
        let body = serde_json::json!({
            "content": [
                { "type": "text", "text": "Hello!" }
            ],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 100,
                "output_tokens": 20,
                "cache_read_input_tokens": 80,
                "cache_creation_input_tokens": 10
            }
        });

        let response = provider.parse_response(&body).unwrap();
        assert_eq!(response.tokens_used.prompt_tokens, 100);
        assert_eq!(response.tokens_used.cached_tokens, 80);
        assert_eq!(response.tokens_used.cache_write_tokens, 10);
    }

    #[test]
    fn test_token_usage_defaults_cache_fields_to_zero() {
        let usage = TokenUsage {
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            cached_tokens: 0,
            cache_write_tokens: 0,
        };
        assert_eq!(usage.cached_tokens, 0);
        assert_eq!(usage.cache_write_tokens, 0);
    }

    // ── Additional coverage tests ──────────────────────────────────────────

    #[test]
    fn test_count_tokens_basic() {
        let provider = AnthropicProvider::new("test-key".to_string());
        let tokens = provider.count_tokens("Hello, world!", "claude-sonnet-4-6");
        assert!(tokens > 0, "Should produce at least one token");
        // ~3.5 chars per token → 13 chars ≈ 3-4 tokens
        assert!(tokens < 10);
    }

    #[test]
    fn test_count_tokens_empty() {
        let provider = AnthropicProvider::new("test-key".to_string());
        let tokens = provider.count_tokens("", "claude-sonnet-4-6");
        assert_eq!(tokens, 0);
    }

    #[test]
    fn test_count_tokens_long_string() {
        let provider = AnthropicProvider::new("test-key".to_string());
        let text = "a".repeat(3500);
        let tokens = provider.count_tokens(&text, "claude-sonnet-4-6");
        assert_eq!(tokens, 1000); // 3500 / 3.5 = 1000
    }

    #[test]
    fn test_name() {
        let provider = AnthropicProvider::new("test-key".to_string());
        assert_eq!(provider.name(), "anthropic");
    }

    #[test]
    fn test_with_config_default_base_url() {
        let config = ProviderConfig {
            api_key: "test-key".to_string(),
            base_url: None,
            rate_limit: None,
        };
        let provider = AnthropicProvider::with_config(config);
        assert_eq!(provider.base_url, "https://api.anthropic.com/v1");
    }

    #[test]
    fn test_with_config_custom_base_url() {
        let config = ProviderConfig {
            api_key: "test-key".to_string(),
            base_url: Some("https://custom.api.com".to_string()),
            rate_limit: None,
        };
        let provider = AnthropicProvider::with_config(config);
        assert_eq!(provider.base_url, "https://custom.api.com");
    }

    #[test]
    fn test_with_config_with_rate_limit() {
        let config = ProviderConfig {
            api_key: "test-key".to_string(),
            base_url: None,
            rate_limit: Some(crate::provider::RateLimitConfig {
                requests_per_minute: 10,
                tokens_per_minute: 50000,
            }),
        };
        let provider = AnthropicProvider::with_config(config);
        assert!(provider.rate_limiter.is_some());
    }

    #[test]
    fn test_builtin_capabilities_opus46() {
        let provider = AnthropicProvider::new("test-key".to_string());
        let caps = provider.builtin_capabilities("claude-opus-4-6");
        assert!(caps.supports_temperature);
        assert!(caps.supports_streaming);
        assert!(caps.supports_tools);
        assert!(caps.supports_system_prompt);
        assert_eq!(caps.max_context_tokens, 1_000_000);
        assert_eq!(caps.max_output_tokens, 128_000);
    }

    #[test]
    fn test_builtin_capabilities_opus47() {
        let provider = AnthropicProvider::new("test-key".to_string());
        let caps = provider.builtin_capabilities("claude-opus-4-7");
        assert!(!caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 1_000_000);
        assert_eq!(caps.max_output_tokens, 128_000);
    }

    #[test]
    fn test_builtin_capabilities_mythos5() {
        let provider = AnthropicProvider::new("test-key".to_string());
        let caps = provider.builtin_capabilities("claude-mythos-5");
        assert!(!caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 1_000_000);
        assert_eq!(caps.max_output_tokens, 128_000);
    }

    #[test]
    fn test_builtin_capabilities_generic_claude4_fallback() {
        let provider = AnthropicProvider::new("test-key".to_string());
        // Uses generic claude-4.x fallback (not matching specific model patterns above)
        let caps = provider.builtin_capabilities("claude-haiku-4");
        assert!(!caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 1_000_000);
        assert_eq!(caps.max_output_tokens, 32_768);
    }

    #[test]
    fn test_builtin_capabilities_unknown_model() {
        let provider = AnthropicProvider::new("test-key".to_string());
        let caps = provider.builtin_capabilities("some-unknown-model");
        // Should return ModelCapabilities::default()
        let default = ModelCapabilities::default();
        assert_eq!(caps.max_context_tokens, default.max_context_tokens);
    }

    #[test]
    fn test_capabilities_uses_override_when_present() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "custom-model".to_string(),
            ModelCapabilities {
                supports_temperature: true,
                supports_streaming: false,
                supports_tools: false,
                supports_system_prompt: false,
                max_context_tokens: 42,
                max_output_tokens: 10,
            },
        );
        let provider = AnthropicProvider::with_overrides("key".to_string(), overrides);
        let caps = provider.capabilities("custom-model");
        assert_eq!(caps.max_context_tokens, 42);
        assert!(!caps.supports_streaming);
    }

    #[test]
    fn test_capabilities_falls_through_to_builtin() {
        let provider = AnthropicProvider::with_overrides("key".to_string(), HashMap::new());
        let caps = provider.capabilities("claude-sonnet-4-6");
        assert_eq!(caps.max_context_tokens, 1_000_000);
    }

    #[test]
    fn test_max_context_tokens_delegates_to_capabilities() {
        let provider = AnthropicProvider::new("key".to_string());
        assert_eq!(provider.max_context_tokens("claude-haiku-4-5"), 200_000);
        assert_eq!(provider.max_context_tokens("claude-opus-4-8"), 1_000_000);
    }

    #[test]
    fn test_parse_stop_reason_all_variants() {
        assert!(matches!(
            AnthropicProvider::parse_stop_reason("end_turn"),
            FinishReason::Complete
        ));
        assert!(matches!(
            AnthropicProvider::parse_stop_reason("tool_use"),
            FinishReason::ToolCall
        ));
        assert!(matches!(
            AnthropicProvider::parse_stop_reason("max_tokens"),
            FinishReason::TokenLimit
        ));
        assert!(matches!(
            AnthropicProvider::parse_stop_reason("stop_sequence"),
            FinishReason::Stop
        ));
        assert!(matches!(
            AnthropicProvider::parse_stop_reason("unknown_reason"),
            FinishReason::Complete
        ));
    }

    #[test]
    fn test_parse_response_empty_content_blocks() {
        let provider = AnthropicProvider::new("key".to_string());
        let body = serde_json::json!({
            "content": [],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 5, "output_tokens": 0 }
        });
        let resp = provider.parse_response(&body).unwrap();
        assert_eq!(resp.content, "");
        assert!(resp.tool_calls.is_empty());
    }

    #[test]
    fn test_parse_response_no_content_field() {
        let provider = AnthropicProvider::new("key".to_string());
        let body = serde_json::json!({
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 5, "output_tokens": 0 }
        });
        let resp = provider.parse_response(&body).unwrap();
        assert_eq!(resp.content, "");
    }

    #[test]
    fn test_parse_response_no_usage_field() {
        let provider = AnthropicProvider::new("key".to_string());
        let body = serde_json::json!({
            "content": [{ "type": "text", "text": "Hello" }],
            "stop_reason": "end_turn"
        });
        let resp = provider.parse_response(&body).unwrap();
        assert_eq!(resp.tokens_used.prompt_tokens, 0);
        assert_eq!(resp.tokens_used.completion_tokens, 0);
    }

    #[test]
    fn test_parse_response_unknown_content_type_ignored() {
        let provider = AnthropicProvider::new("key".to_string());
        let body = serde_json::json!({
            "content": [
                { "type": "image", "data": "abc" },
                { "type": "text", "text": "Hello" }
            ],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 10, "output_tokens": 5 }
        });
        let resp = provider.parse_response(&body).unwrap();
        assert_eq!(resp.content, "Hello");
    }

    #[test]
    fn test_parse_response_tool_call_missing_fields() {
        let provider = AnthropicProvider::new("key".to_string());
        let body = serde_json::json!({
            "content": [
                { "type": "tool_use" }
            ],
            "stop_reason": "tool_use",
            "usage": { "input_tokens": 10, "output_tokens": 5 }
        });
        let resp = provider.parse_response(&body).unwrap();
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].id, "");
        assert_eq!(resp.tool_calls[0].name, "");
    }

    #[test]
    fn test_parse_response_total_tokens_computed() {
        let provider = AnthropicProvider::new("key".to_string());
        let body = serde_json::json!({
            "content": [{ "type": "text", "text": "ok" }],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 100, "output_tokens": 50 }
        });
        let resp = provider.parse_response(&body).unwrap();
        assert_eq!(resp.tokens_used.total_tokens, 150);
    }

    #[test]
    fn test_build_request_body_with_tools() {
        let provider = AnthropicProvider::new("key".to_string());
        let request = InferenceRequest {
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Use the tool".to_string(),
                cache_breakpoint: false,
            }],
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![crate::provider::Tool {
                name: "search".to_string(),
                description: "Search the web".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            }],
            extra: serde_json::Value::Null,
        };

        let body = provider.build_request_body(&request);
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "search");
        assert_eq!(tools[0]["input_schema"]["type"], "object");
    }

    #[test]
    fn test_build_request_body_no_temperature_for_opus48() {
        let provider = AnthropicProvider::new("key".to_string());
        let request = InferenceRequest {
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Hi".to_string(),
                cache_breakpoint: false,
            }],
            model: "claude-opus-4-8".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
        };

        let body = provider.build_request_body(&request);
        // Opus 4.8 doesn't support temperature, so it should NOT be in the body
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn test_build_request_body_temperature_for_sonnet46() {
        let provider = AnthropicProvider::new("key".to_string());
        let request = InferenceRequest {
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Hi".to_string(),
                cache_breakpoint: false,
            }],
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            temperature: 0.5,
            tools: vec![],
            extra: serde_json::Value::Null,
        };

        let body = provider.build_request_body(&request);
        assert_eq!(body["temperature"], 0.5);
    }

    #[test]
    fn test_build_request_body_multiple_system_messages() {
        let provider = AnthropicProvider::new("key".to_string());
        let request = InferenceRequest {
            messages: vec![
                crate::provider::Message {
                    role: "system".to_string(),
                    content: "System part 1".to_string(),
                    cache_breakpoint: false,
                },
                crate::provider::Message {
                    role: "system".to_string(),
                    content: "System part 2".to_string(),
                    cache_breakpoint: false,
                },
                crate::provider::Message {
                    role: "user".to_string(),
                    content: "Hello".to_string(),
                    cache_breakpoint: false,
                },
            ],
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
        };

        let body = provider.build_request_body(&request);
        // Multiple system messages → should be an array
        assert!(body["system"].is_array());
        assert_eq!(body["system"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_build_request_body_no_system_messages() {
        let provider = AnthropicProvider::new("key".to_string());
        let request = InferenceRequest {
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Hello".to_string(),
                cache_breakpoint: false,
            }],
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
        };

        let body = provider.build_request_body(&request);
        assert!(body.get("system").is_none());
    }

    // ── SSE parsing tests ──────────────────────────────────────────────────

    #[test]
    fn test_parse_sse_event_text_delta() {
        let mut buffer = "event: content_block_delta\ndata: {\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n".to_string();
        let mut tool_index = 0usize;
        let chunk = parse_sse_event(&mut buffer, &mut tool_index).unwrap();
        assert_eq!(chunk.delta, "Hello");
        assert!(chunk.tool_calls.is_empty());
        assert!(buffer.is_empty());
    }

    #[test]
    fn test_parse_sse_event_input_json_delta() {
        let mut buffer = "event: content_block_delta\ndata: {\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"key\\\"\"}}\n\n".to_string();
        let mut tool_index = 1usize;
        let chunk = parse_sse_event(&mut buffer, &mut tool_index).unwrap();
        assert_eq!(chunk.delta, "");
        assert_eq!(chunk.tool_calls.len(), 1);
        assert_eq!(chunk.tool_calls[0].index, 1);
        assert_eq!(chunk.tool_calls[0].arguments_delta, "{\"key\"");
    }

    #[test]
    fn test_parse_sse_event_content_block_start_tool_use() {
        let mut buffer = "event: content_block_start\ndata: {\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"search\"}}\n\n".to_string();
        let mut tool_index = 0usize;
        let chunk = parse_sse_event(&mut buffer, &mut tool_index).unwrap();
        assert_eq!(chunk.tool_calls.len(), 1);
        assert_eq!(chunk.tool_calls[0].id, Some("toolu_1".to_string()));
        assert_eq!(chunk.tool_calls[0].name, Some("search".to_string()));
        assert_eq!(chunk.tool_calls[0].index, 0);
        assert_eq!(tool_index, 1, "tool_index should increment");
    }

    #[test]
    fn test_parse_sse_event_content_block_start_text_returns_none() {
        let mut buffer = "event: content_block_start\ndata: {\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n".to_string();
        let mut tool_index = 0usize;
        let result = parse_sse_event(&mut buffer, &mut tool_index);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_sse_event_message_delta() {
        let mut buffer = "event: message_delta\ndata: {\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":42}}\n\n".to_string();
        let mut tool_index = 0usize;
        let chunk = parse_sse_event(&mut buffer, &mut tool_index).unwrap();
        assert!(matches!(chunk.finish_reason, Some(FinishReason::ToolCall)));
        let tokens = chunk.tokens.unwrap();
        assert_eq!(tokens.completion_tokens, 42);
    }

    #[test]
    fn test_parse_sse_event_message_start_with_usage() {
        let mut buffer = "event: message_start\ndata: {\"message\":{\"usage\":{\"input_tokens\":100,\"cache_read_input_tokens\":50,\"cache_creation_input_tokens\":10}}}\n\n".to_string();
        let mut tool_index = 0usize;
        let chunk = parse_sse_event(&mut buffer, &mut tool_index).unwrap();
        let tokens = chunk.tokens.unwrap();
        assert_eq!(tokens.prompt_tokens, 100);
        assert_eq!(tokens.cached_tokens, 50);
        assert_eq!(tokens.cache_write_tokens, 10);
    }

    #[test]
    fn test_parse_sse_event_message_start_zero_usage_returns_none() {
        let mut buffer =
            "event: message_start\ndata: {\"message\":{\"usage\":{\"input_tokens\":0}}}\n\n"
                .to_string();
        let mut tool_index = 0usize;
        let result = parse_sse_event(&mut buffer, &mut tool_index);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_sse_event_message_stop_returns_none() {
        let mut buffer = "event: message_stop\ndata: {}\n\n".to_string();
        let mut tool_index = 0usize;
        let result = parse_sse_event(&mut buffer, &mut tool_index);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_sse_event_ping_returns_none() {
        let mut buffer = "event: ping\ndata: {}\n\n".to_string();
        let mut tool_index = 0usize;
        let result = parse_sse_event(&mut buffer, &mut tool_index);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_sse_event_unknown_event_returns_none() {
        let mut buffer = "event: some_future_event\ndata: {\"foo\":\"bar\"}\n\n".to_string();
        let mut tool_index = 0usize;
        let result = parse_sse_event(&mut buffer, &mut tool_index);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_sse_event_incomplete_buffer() {
        let mut buffer = "event: content_block_delta\ndata: {\"delta\":".to_string();
        let mut tool_index = 0usize;
        let result = parse_sse_event(&mut buffer, &mut tool_index);
        assert!(result.is_none());
        // Buffer should be unchanged
        assert!(buffer.contains("content_block_delta"));
    }

    #[test]
    fn test_parse_sse_event_empty_data_returns_none() {
        let mut buffer = "event: content_block_delta\n\n\n".to_string();
        let mut tool_index = 0usize;
        let result = parse_sse_event(&mut buffer, &mut tool_index);
        assert!(result.is_none());
    }

    // ─── HTTP error paths (connection refused) ───────────────────────────

    #[tokio::test]
    async fn test_infer_connection_refused_returns_error() {
        let provider = AnthropicProvider {
            client: reqwest::Client::new(),
            api_key: "test-key".to_string(),
            base_url: "http://127.0.0.1:19997".to_string(),
            rate_limiter: None,
            capability_overrides: HashMap::new(),
        };
        let request = InferenceRequest {
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Hello".to_string(),
                cache_breakpoint: false,
            }],
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 100,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
        };
        let result = provider.infer(request).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ProviderError::RequestFailed(_)
        ));
    }

    #[tokio::test]
    async fn test_infer_stream_connection_refused_returns_error() {
        let provider = AnthropicProvider {
            client: reqwest::Client::new(),
            api_key: "test-key".to_string(),
            base_url: "http://127.0.0.1:19997".to_string(),
            rate_limiter: None,
            capability_overrides: HashMap::new(),
        };
        let request = InferenceRequest {
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Hello".to_string(),
                cache_breakpoint: false,
            }],
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 100,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
        };
        let result = provider.infer_stream(request).await;
        assert!(result.is_err(), "Expected connection refused error");
        // Verify it's a RequestFailed variant without using unwrap_err (Debug not impl'd for Stream)
        if let Err(e) = result {
            assert!(matches!(e, ProviderError::RequestFailed(_)));
        }
    }

    #[tokio::test]
    async fn test_list_models_connection_refused_returns_error() {
        let provider = AnthropicProvider {
            client: reqwest::Client::new(),
            api_key: "test-key".to_string(),
            base_url: "http://127.0.0.1:19997".to_string(),
            rate_limiter: None,
            capability_overrides: HashMap::new(),
        };
        let result = provider.list_models().await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ProviderError::RequestFailed(_)
        ));
    }

    // ─── parse_sse_event: message_delta without usage ─────────────────────

    #[test]
    fn test_parse_sse_event_message_delta_no_usage() {
        let mut buffer =
            "event: message_delta\ndata: {\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n"
                .to_string();
        let mut tool_index = 0usize;
        let chunk = parse_sse_event(&mut buffer, &mut tool_index).unwrap();
        assert!(matches!(chunk.finish_reason, Some(FinishReason::Complete)));
        // No usage → tokens default to 0
        let tokens = chunk.tokens.unwrap();
        assert_eq!(tokens.completion_tokens, 0);
    }

    // ─── parse_sse_event: multiple events in buffer ───────────────────────

    #[test]
    fn test_parse_sse_event_multiple_events_consumed_one_at_a_time() {
        let mut buffer = concat!(
            "event: content_block_delta\n",
            "data: {\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\n"
        )
        .to_string();
        let mut tool_index = 0usize;

        // First event
        let chunk1 = parse_sse_event(&mut buffer, &mut tool_index).unwrap();
        assert_eq!(chunk1.delta, "Hello");

        // Second event
        let chunk2 = parse_sse_event(&mut buffer, &mut tool_index).unwrap();
        assert_eq!(chunk2.delta, " world");

        // Buffer now empty
        assert!(parse_sse_event(&mut buffer, &mut tool_index).is_none());
    }

    // ─── parse_sse_event: content_block_start non-tool type ──────────────

    #[test]
    fn test_parse_sse_event_content_block_start_no_content_block() {
        // content_block_start with no "content_block" key
        let mut buffer = "event: content_block_start\ndata: {\"index\":0}\n\n".to_string();
        let mut tool_index = 0usize;
        let result = parse_sse_event(&mut buffer, &mut tool_index);
        assert!(result.is_none());
    }

    // ─── parse_sse_event: invalid JSON returns None ───────────────────────

    #[test]
    fn test_parse_sse_event_invalid_json_data_returns_none() {
        let mut buffer = "event: content_block_delta\ndata: not-valid-json\n\n".to_string();
        let mut tool_index = 0usize;
        let result = parse_sse_event(&mut buffer, &mut tool_index);
        assert!(result.is_none());
    }

    // ─── parse_response: stop_reason default end_turn ─────────────────────

    #[test]
    fn test_parse_response_no_stop_reason_defaults_to_complete() {
        let provider = AnthropicProvider::new("key".to_string());
        let body = serde_json::json!({
            "content": [{ "type": "text", "text": "hi" }],
            "usage": { "input_tokens": 5, "output_tokens": 2 }
        });
        let resp = provider.parse_response(&body).unwrap();
        assert!(matches!(resp.finish_reason, FinishReason::Complete));
    }

    // ─── build_request_body: cache breakpoints at max limit ───────────────

    #[test]
    fn test_build_request_body_exactly_4_cache_breakpoints() {
        let provider = AnthropicProvider::new("key".to_string());
        // Exactly 4 messages with cache_breakpoint = true
        let messages: Vec<crate::provider::Message> = (0..4)
            .map(|i| crate::provider::Message {
                role: "user".to_string(),
                content: format!("Message {}", i),
                cache_breakpoint: true,
            })
            .collect();

        let request = InferenceRequest {
            messages,
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 512,
            temperature: 0.5,
            tools: vec![],
            extra: serde_json::Value::Null,
        };

        let body = provider.build_request_body(&request);
        let msgs = body["messages"].as_array().unwrap();

        // All 4 should have cache_control
        let bp_count = msgs
            .iter()
            .filter(|m| {
                m.get("content")
                    .and_then(|c| c.as_array())
                    .and_then(|arr| arr.first())
                    .and_then(|block| block.get("cache_control"))
                    .is_some()
            })
            .count();
        assert_eq!(bp_count, 4);
    }
}
