//! OpenAI provider implementation.

#[cfg(test)]
use crate::provider::FinishReason;
use crate::provider::{
    check_http_response, parse_openai_finish_reason, InferenceRequest, InferenceResponse,
    ModelCapabilities, ModelInfo, Provider, ProviderConfig, ProviderError, Result, StreamChunk,
    TokenUsage, ToolCall, ToolCallDelta,
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
        // o1-mini — smaller context than o3-mini
        } else if model.starts_with("o1-mini") {
            ModelCapabilities {
                supports_temperature: false,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 128_000,
                max_output_tokens: 65_536,
            }
        // o-series reasoning models (o1, o3, o4) — no temperature, 200K context
        } else if model.starts_with("o1") || model.starts_with("o3") || model.starts_with("o4") {
            ModelCapabilities {
                supports_temperature: false,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 200_000,
                max_output_tokens: 100_000,
            }
        } else if model.starts_with("gpt-4o") {
            ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 128_000,
                max_output_tokens: 16_384,
            }
        } else if model.starts_with("gpt-4") {
            ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 128_000,
                max_output_tokens: 8192,
            }
        } else if model.starts_with("gpt-3.5") {
            ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 16_385,
                max_output_tokens: 4096,
            }
        } else {
            ModelCapabilities::default()
        }
    }

    /// Build the request body for the OpenAI Chat Completions API.
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

    /// Parse the API response body.
    fn parse_response(&self, body: &serde_json::Value) -> Result<InferenceResponse> {
        let choice = body
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|c| c.first())
            .ok_or_else(|| ProviderError::InvalidResponse("No choices in response".to_string()))?;

        let message = choice
            .get("message")
            .ok_or_else(|| ProviderError::InvalidResponse("No message in choice".to_string()))?;

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
                let arguments: serde_json::Value = serde_json::from_str(arguments_str)
                    .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
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

        Ok(InferenceResponse {
            content,
            tool_calls,
            tokens_used: TokenUsage {
                prompt_tokens,
                completion_tokens,
                total_tokens: prompt_tokens + completion_tokens,
            },
            finish_reason: parse_openai_finish_reason(finish_reason),
        })
    }
}

#[async_trait]
impl Provider for OpenAIProvider {
    async fn infer(&self, request: InferenceRequest) -> Result<InferenceResponse> {
        tracing::debug!(model = %request.model, "Calling OpenAI API");

        if let Some(limiter) = &self.rate_limiter {
            limiter.acquire().await?;
        }

        let body = self.build_request_body(&request);

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
        tracing::debug!(model = %request.model, "Calling OpenAI API (streaming)");

        if let Some(limiter) = &self.rate_limiter {
            limiter.acquire().await?;
        }

        let mut body = self.build_request_body(&request);
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

// SSE stream parser for OpenAI's streaming API.
pin_project_lite::pin_project! {
    pub(crate) struct OpenAiSseStream<S> {
        #[pin]
        inner: S,
        buffer: String,
    }
}

impl<S> OpenAiSseStream<S> {
    pub(crate) fn new(inner: S) -> Self {
        Self {
            inner,
            buffer: String::new(),
        }
    }
}

impl<S> Stream for OpenAiSseStream<S>
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
            // Check for complete SSE events
            if let Some(chunk) = parse_openai_sse_event(this.buffer) {
                return std::task::Poll::Ready(chunk.map(Ok));
            }

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
                    if let Some(chunk) = parse_openai_sse_event(this.buffer) {
                        return std::task::Poll::Ready(chunk.map(Ok));
                    }
                    return std::task::Poll::Ready(None);
                }
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
        }
    }
}

/// Parse a single SSE event from the buffer.
/// Returns Some(Some(chunk)) for data, Some(None) for stream end, None for incomplete.
fn parse_openai_sse_event(buffer: &mut String) -> Option<Option<StreamChunk>> {
    let event_end = buffer.find("\n\n")?;
    let event_text = buffer[..event_end].to_string();
    *buffer = buffer[event_end + 2..].to_string();

    for line in event_text.lines() {
        if let Some(data) = line.strip_prefix("data: ") {
            let data = data.trim();
            if data == "[DONE]" {
                return Some(None); // Stream finished
            }

            let json: serde_json::Value = match serde_json::from_str(data) {
                Ok(j) => j,
                Err(_) => continue,
            };

            let choice = json
                .get("choices")
                .and_then(|c| c.as_array())
                .and_then(|c| c.first());

            // Handle usage-only chunk (no choices)
            if choice.is_none() {
                if let Some(usage) = json.get("usage") {
                    let prompt_tokens = usage
                        .get("prompt_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as usize;
                    let completion_tokens = usage
                        .get("completion_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as usize;
                    return Some(Some(StreamChunk {
                        delta: String::new(),
                        tool_calls: Vec::new(),
                        tokens: Some(TokenUsage {
                            prompt_tokens,
                            completion_tokens,
                            total_tokens: prompt_tokens + completion_tokens,
                        }),
                        finish_reason: None,
                    }));
                }
                continue;
            }

            let choice = choice.unwrap();
            let delta = choice.get("delta").unwrap_or(&serde_json::Value::Null);

            let content = delta
                .get("content")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();

            let mut tool_call_deltas = Vec::new();
            if let Some(tcs) = delta.get("tool_calls").and_then(|tc| tc.as_array()) {
                for tc in tcs {
                    let index = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                    let id = tc.get("id").and_then(|v| v.as_str()).map(|s| s.to_string());
                    let function = tc.get("function");
                    let name = function
                        .and_then(|f| f.get("name"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    let args = function
                        .and_then(|f| f.get("arguments"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    tool_call_deltas.push(ToolCallDelta {
                        index,
                        id,
                        name,
                        arguments_delta: args.to_string(),
                    });
                }
            }

            let finish_reason = choice
                .get("finish_reason")
                .and_then(|v| v.as_str())
                .map(parse_openai_finish_reason);

            // Check for usage in the chunk
            let tokens = json.get("usage").map(|usage| {
                let pt = usage
                    .get("prompt_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize;
                let ct = usage
                    .get("completion_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize;
                TokenUsage {
                    prompt_tokens: pt,
                    completion_tokens: ct,
                    total_tokens: pt + ct,
                }
            });

            return Some(Some(StreamChunk {
                delta: content,
                tool_calls: tool_call_deltas,
                tokens,
                finish_reason,
            }));
        }
    }

    None
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

    #[test]
    fn test_build_request_body() {
        let provider = OpenAIProvider::new("test-key".to_string());
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
            model: "gpt-4o".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
        };

        let body = provider.build_request_body(&request);
        assert_eq!(body["model"], "gpt-4o");
        assert_eq!(body["messages"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_parse_response() {
        let provider = OpenAIProvider::new("test-key".to_string());
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

        let response = provider.parse_response(&body).unwrap();
        assert_eq!(response.content, "Hello!");
        assert_eq!(response.tokens_used.prompt_tokens, 10);
        assert!(matches!(response.finish_reason, FinishReason::Complete));
    }

    #[test]
    fn test_parse_response_with_tool_calls() {
        let provider = OpenAIProvider::new("test-key".to_string());
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

        let response = provider.parse_response(&body).unwrap();
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].name, "search");
        assert!(matches!(response.finish_reason, FinishReason::ToolCall));
    }

    #[test]
    fn test_builtin_capabilities_o1() {
        let provider = OpenAIProvider::new("test-key".to_string());
        let caps = provider.builtin_capabilities("o1-preview");
        assert!(!caps.supports_temperature);
        assert!(caps.supports_streaming);
        assert_eq!(caps.max_context_tokens, 200_000);
        assert_eq!(caps.max_output_tokens, 100_000);
    }

    #[test]
    fn test_builtin_capabilities_o1_mini() {
        let provider = OpenAIProvider::new("test-key".to_string());
        let caps = provider.builtin_capabilities("o1-mini");
        assert!(!caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 128_000);
        assert_eq!(caps.max_output_tokens, 65_536);
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
    fn test_builtin_capabilities_gpt4o() {
        let provider = OpenAIProvider::new("test-key".to_string());
        let caps = provider.builtin_capabilities("gpt-4o");
        assert!(caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 128_000);
        assert_eq!(caps.max_output_tokens, 16_384);
    }

    #[test]
    fn test_builtin_capabilities_gpt4() {
        let provider = OpenAIProvider::new("test-key".to_string());
        let caps = provider.builtin_capabilities("gpt-4-turbo");
        assert!(caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 128_000);
        assert_eq!(caps.max_output_tokens, 8192);
    }

    #[test]
    fn test_builtin_capabilities_gpt35() {
        let provider = OpenAIProvider::new("test-key".to_string());
        let caps = provider.builtin_capabilities("gpt-3.5-turbo");
        assert!(caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 16_385);
        assert_eq!(caps.max_output_tokens, 4096);
    }

    #[test]
    fn test_capabilities_override() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "gpt-4o".to_string(),
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
        let caps = provider.capabilities("gpt-4o");
        assert!(!caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 1);
    }
}
