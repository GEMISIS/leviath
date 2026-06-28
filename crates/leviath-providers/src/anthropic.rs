//! Anthropic Claude provider implementation.

use crate::provider::{
    FinishReason, InferenceRequest, InferenceResponse, ModelCapabilities, ModelInfo, Provider,
    ProviderConfig, ProviderError, Result, StreamChunk, ToolCallDelta, TokenUsage, ToolCall,
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
        // Claude 3.5 and Claude 3
        if model.contains("claude-3-5") || model.contains("claude-3") {
            return ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 200_000,
                max_output_tokens: 8192,
            };
        }
        ModelCapabilities::default()
    }

    /// Build the request body for the Anthropic API.
    fn build_request_body(&self, request: &InferenceRequest) -> serde_json::Value {
        // Extract system messages and non-system messages
        let mut system_parts: Vec<serde_json::Value> = Vec::new();
        let mut messages: Vec<serde_json::Value> = Vec::new();

        for msg in &request.messages {
            if msg.role == "system" {
                system_parts.push(serde_json::json!({
                    "type": "text",
                    "text": msg.content,
                    "cache_control": { "type": "ephemeral" }
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

        Ok(InferenceResponse {
            content,
            tool_calls,
            tokens_used: TokenUsage {
                prompt_tokens,
                completion_tokens,
                total_tokens: prompt_tokens + completion_tokens,
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

        let data = body
            .get("data")
            .and_then(|d| d.as_array())
            .ok_or_else(|| ProviderError::RequestFailed("missing 'data' field in /models response".to_string()))?;

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
            if content_block
                .get("type")
                .and_then(|t| t.as_str())
                == Some("tool_use")
            {
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
                }),
                finish_reason: Some(AnthropicProvider::parse_stop_reason(stop_reason)),
            })
        }
        "message_start" => {
            // Extract input token count from message_start
            let usage = json
                .get("message")
                .and_then(|m| m.get("usage"));
            let input_tokens = usage
                .and_then(|u| u.get("input_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;

            if input_tokens > 0 {
                Some(StreamChunk {
                    delta: String::new(),
                    tool_calls: Vec::new(),
                    tokens: Some(TokenUsage {
                        prompt_tokens: input_tokens,
                        completion_tokens: 0,
                        total_tokens: input_tokens,
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
        assert_eq!(
            provider.max_context_tokens("claude-3-5-sonnet-20241022"),
            200_000
        );
    }

    #[test]
    fn test_build_request_body() {
        let provider = AnthropicProvider::new("test-key".to_string());
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
    fn test_builtin_capabilities_claude3() {
        let provider = AnthropicProvider::new("test-key".to_string());
        let caps = provider.builtin_capabilities("claude-3-5-sonnet-20241022");
        assert!(caps.supports_temperature);
        assert_eq!(caps.max_output_tokens, 8192);
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
}
