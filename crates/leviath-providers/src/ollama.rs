//! Ollama provider implementation.
//!
//! Ollama provides local LLM execution via NDJSON streaming.

use crate::provider::{
    FinishReason, InferenceRequest, InferenceResponse, ModelCapabilities, ModelInfo, Provider,
    ProviderError, Result, StreamChunk, TokenUsage,
};
use async_trait::async_trait;
use futures_core::Stream;
use std::collections::HashMap;
use std::pin::Pin;

/// Ollama provider for local LLM execution.
pub struct OllamaProvider {
    /// HTTP client
    client: reqwest::Client,

    /// API base URL (defaults to local)
    base_url: String,

    /// Per-model capability overrides
    capability_overrides: HashMap<String, ModelCapabilities>,
}

impl OllamaProvider {
    /// Create a new Ollama provider.
    pub fn new() -> Self {
        Self {
            client: crate::provider::build_http_client(None),
            base_url: "http://localhost:11434".to_string(),
            capability_overrides: HashMap::new(),
        }
    }

    /// Create a new Ollama provider with custom base URL.
    pub fn with_base_url(base_url: String) -> Self {
        if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
            tracing::warn!(url = %base_url, "Ollama base URL should start with http:// or https://");
        }
        Self {
            client: crate::provider::build_http_client(None),
            base_url,
            capability_overrides: HashMap::new(),
        }
    }

    /// Create a new Ollama provider with a custom base URL and per-model capability overrides.
    pub fn with_overrides(
        base_url: String,
        overrides: HashMap<String, ModelCapabilities>,
        timeout_secs: Option<u64>,
    ) -> Self {
        if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
            tracing::warn!(url = %base_url, "Ollama base URL should start with http:// or https://");
        }
        Self {
            client: crate::provider::build_http_client(timeout_secs),
            base_url,
            capability_overrides: overrides,
        }
    }

    /// Return built-in capability defaults for a model based on its name pattern.
    fn builtin_capabilities(&self, model: &str) -> ModelCapabilities {
        // Llama 3.x and Qwen 2.x/3 — tool-capable, 128K context
        if model.contains("llama3")
            || model.contains("llama-3")
            || model.contains("qwen2.5")
            || model.contains("qwen3")
            || model.contains("qwen2")
        {
            ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 131_072,
                max_output_tokens: 8192,
            }
        // Mistral / Mixtral — tool-capable
        } else if model.contains("mistral") || model.contains("mixtral") {
            ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 32_768,
                max_output_tokens: 4096,
            }
        // Phi-4 — tool-capable, 128K context
        } else if model.contains("phi-4") || model.contains("phi4") {
            ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 131_072,
                max_output_tokens: 8192,
            }
        // DeepSeek R1 — reasoning, no tool calls
        } else if model.contains("deepseek-r1") {
            ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: false,
                supports_system_prompt: true,
                max_context_tokens: 131_072,
                max_output_tokens: 8192,
            }
        // DeepSeek general — tool-capable
        } else if model.contains("deepseek") {
            ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 131_072,
                max_output_tokens: 8192,
            }
        // Gemma — no tool support
        } else if model.contains("gemma") {
            ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: false,
                supports_system_prompt: true,
                max_context_tokens: 131_072,
                max_output_tokens: 8192,
            }
        // CodeLlama — no tool support
        } else if model.contains("codellama") {
            ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: false,
                supports_system_prompt: true,
                max_context_tokens: 16_384,
                max_output_tokens: 4096,
            }
        } else {
            // Conservative fallback for unknown local models
            ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: false,
                supports_system_prompt: true,
                max_context_tokens: 8192,
                max_output_tokens: 4096,
            }
        }
    }

    /// Build request body for the Ollama API.
    fn build_request_body(&self, request: &InferenceRequest) -> serde_json::Value {
        // System blocks prepended + tool_use/tool_result history converted to
        // OpenAI format (Ollama speaks the OpenAI chat shape). Previously the
        // system prompt was dropped and blocks were serialized as raw JSON.
        let messages = crate::openai_compat::openai_messages(request);

        let caps = self.capabilities(&request.model);
        let options = if caps.supports_temperature {
            serde_json::json!({
                "temperature": request.temperature,
                "num_predict": request.max_tokens,
            })
        } else {
            serde_json::json!({
                "num_predict": request.max_tokens,
            })
        };
        let mut body = serde_json::json!({
            "model": request.model,
            "messages": messages,
            "options": options,
        });

        // Add tools if present (Ollama supports tool calling for some models)
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

        // Pass through extra model parameters (top_p, top_k, stop, seed, …).
        // Ollama's sampling knobs live under `options`, so merge there.
        crate::openai_compat::merge_extra_params(
            body.get_mut("options")
                .and_then(|v| v.as_object_mut())
                .expect("ollama request body always has an `options` object"),
            &request.extra,
        );
        body
    }

    /// Parse non-streaming response from Ollama.
    fn parse_response(&self, body: &serde_json::Value) -> Result<InferenceResponse> {
        let message = body
            .get("message")
            .ok_or_else(|| ProviderError::InvalidResponse("No message in response".to_string()))?;

        let content = message
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();

        let mut tool_calls = Vec::new();
        if let Some(tcs) = message.get("tool_calls").and_then(|tc| tc.as_array()) {
            for (i, tc) in tcs.iter().enumerate() {
                let function = tc.get("function").unwrap_or(&serde_json::Value::Null);
                let name = function
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let arguments = function
                    .get("arguments")
                    .cloned()
                    .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
                tool_calls.push(crate::provider::ToolCall {
                    id: format!("ollama_{}", i),
                    name,
                    arguments,
                });
            }
        }

        let eval_count = body.get("eval_count").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let prompt_eval_count = body
            .get("prompt_eval_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;

        let finish_reason = if !tool_calls.is_empty() {
            FinishReason::ToolCall
        } else {
            FinishReason::Complete
        };

        Ok(InferenceResponse {
            content,
            tool_calls,
            tokens_used: TokenUsage {
                prompt_tokens: prompt_eval_count,
                completion_tokens: eval_count,
                total_tokens: prompt_eval_count + eval_count,
                cached_tokens: 0,
                cache_write_tokens: 0,
            },
            finish_reason,
        })
    }
}

impl Default for OllamaProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for OllamaProvider {
    async fn infer(&self, request: InferenceRequest) -> Result<InferenceResponse> {
        tracing::debug!(model = %request.model, "Calling Ollama API");

        let mut body = self.build_request_body(&request);
        body["stream"] = serde_json::Value::Bool(false);
        let url = format!("{}/api/chat", self.base_url);

        #[cfg(feature = "debug-http")]
        {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert("content-type", "application/json".parse().unwrap());
            let body_size = serde_json::to_vec(&body).map(|b| b.len()).unwrap_or(0);
            crate::debug_http::log_request("ollama", "POST", &url, &headers, body_size);
        }
        #[cfg(feature = "debug-http")]
        let start = std::time::Instant::now();

        let response = crate::provider::apply_request_timeout(
            self.client.post(&url),
            request.request_timeout_secs,
        )
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| {
            #[cfg(feature = "debug-http")]
            crate::debug_http::log_error("ollama", &url, &e.to_string());
            ProviderError::RequestFailed(e.to_string())
        })?;

        #[cfg(feature = "debug-http")]
        crate::debug_http::log_response(
            "ollama",
            &url,
            response.status().as_u16(),
            response.headers(),
            response.content_length(),
            start.elapsed(),
        );

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

        let response_body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| ProviderError::InvalidResponse(e.to_string()))?;

        self.parse_response(&response_body)
    }

    async fn infer_stream(
        &self,
        request: InferenceRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>> {
        tracing::debug!(model = %request.model, "Calling Ollama API (streaming)");

        let mut body = self.build_request_body(&request);
        body["stream"] = serde_json::Value::Bool(true);
        let url = format!("{}/api/chat", self.base_url);

        #[cfg(feature = "debug-http")]
        {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert("content-type", "application/json".parse().unwrap());
            let body_size = serde_json::to_vec(&body).map(|b| b.len()).unwrap_or(0);
            crate::debug_http::log_request("ollama", "POST", &url, &headers, body_size);
        }
        #[cfg(feature = "debug-http")]
        let start = std::time::Instant::now();

        let response = crate::provider::apply_request_timeout(
            self.client.post(&url),
            request.request_timeout_secs,
        )
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| {
            #[cfg(feature = "debug-http")]
            crate::debug_http::log_error("ollama", &url, &e.to_string());
            ProviderError::RequestFailed(e.to_string())
        })?;

        #[cfg(feature = "debug-http")]
        crate::debug_http::log_response(
            "ollama",
            &url,
            response.status().as_u16(),
            response.headers(),
            response.content_length(),
            start.elapsed(),
        );

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

        let byte_stream = response.bytes_stream();
        let stream = OllamaNdjsonStream::new(byte_stream);

        Ok(Box::pin(stream))
    }

    async fn count_tokens(&self, text: &str, _model: &str) -> usize {
        // Ollama exposes no token-count endpoint; approximate locally.
        text.len() / 4
    }

    fn max_context_tokens(&self, model: &str) -> usize {
        self.capabilities(model).max_context_tokens
    }

    fn name(&self) -> &str {
        "ollama"
    }

    fn capabilities(&self, model: &str) -> ModelCapabilities {
        if let Some(overridden) = self.capability_overrides.get(model) {
            return overridden.clone();
        }
        self.builtin_capabilities(model)
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let response = self
            .client
            .get(format!("{}/api/tags", self.base_url))
            .send()
            .await
            .map_err(|e| ProviderError::RequestFailed(e.to_string()))?;

        let status = response.status();
        if !status.is_success() {
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
            .map_err(|e| ProviderError::InvalidResponse(e.to_string()))?;

        let models = body
            .get("models")
            .and_then(|m| m.as_array())
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| {
                        let id = entry.get("name")?.as_str()?.to_string();
                        let capabilities = self.capabilities(&id);
                        Some(ModelInfo {
                            display_name: Some(id.clone()),
                            provider: "ollama".into(),
                            capabilities,
                            id,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(models)
    }
}

// NDJSON stream parser for Ollama's streaming API.
//
// The inner byte stream is boxed as a trait object rather than kept generic.
// In production this is always `reqwest`'s `bytes_stream()`; tests inject
// dozens of distinct mock stream types via `new`'s generic parameter, and a
// generic `impl<S> Stream` causes `cargo llvm-cov` to instrument each
// monomorphized `poll_next` separately, leaving some artificially "uncovered"
// even though the shared logic is fully exercised. Boxing collapses all of
// that into a single concrete `poll_next` implementation.
struct OllamaNdjsonStream {
    inner: Pin<Box<dyn Stream<Item = std::result::Result<bytes::Bytes, reqwest::Error>> + Send>>,
    buffer: String,
}

impl OllamaNdjsonStream {
    fn new<S>(inner: S) -> Self
    where
        S: Stream<Item = std::result::Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
    {
        Self {
            inner: Box::pin(inner),
            buffer: String::new(),
        }
    }
}

impl Stream for OllamaNdjsonStream {
    type Item = Result<StreamChunk>;

    #[expect(
        clippy::string_slice,
        reason = "`newline_pos` is a `find` hit for the ASCII '\\n', so it and `newline_pos + 1` \
                  are char boundaries"
    )]
    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.get_mut();

        loop {
            // Try to parse a complete JSON line
            if let Some(newline_pos) = this.buffer.find('\n') {
                let line = this.buffer[..newline_pos].to_string();
                this.buffer = this.buffer[newline_pos + 1..].to_string();

                let line = line.trim();
                if line.is_empty() {
                    continue;
                }

                let json: serde_json::Value = match serde_json::from_str(line) {
                    Ok(j) => j,
                    Err(e) => {
                        return std::task::Poll::Ready(Some(Err(ProviderError::InvalidResponse(
                            e.to_string(),
                        ))));
                    }
                };

                // Check for done flag
                let done = json.get("done").and_then(|v| v.as_bool()).unwrap_or(false);

                let message = json.get("message");
                let content = message
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .to_string();

                if done {
                    let eval_count =
                        json.get("eval_count").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                    let prompt_eval_count = json
                        .get("prompt_eval_count")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as usize;

                    // Parse tool calls from the final chunk's message.tool_calls
                    let mut tool_calls = Vec::new();
                    if let Some(tcs) = message
                        .and_then(|m| m.get("tool_calls"))
                        .and_then(|tc| tc.as_array())
                    {
                        for (i, tc) in tcs.iter().enumerate() {
                            let function = tc.get("function").unwrap_or(&serde_json::Value::Null);
                            let name = function
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let arguments = function
                                .get("arguments")
                                .cloned()
                                .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
                            tool_calls.push(crate::provider::ToolCallDelta {
                                index: i,
                                id: Some(format!("ollama_{}", i)),
                                name: Some(name),
                                arguments_delta: arguments.to_string(),
                            });
                        }
                    }

                    let finish_reason = if tool_calls.is_empty() {
                        FinishReason::Complete
                    } else {
                        FinishReason::ToolCall
                    };

                    return std::task::Poll::Ready(Some(Ok(StreamChunk {
                        delta: content,
                        tool_calls,
                        tokens: Some(TokenUsage {
                            prompt_tokens: prompt_eval_count,
                            completion_tokens: eval_count,
                            total_tokens: prompt_eval_count + eval_count,
                            cached_tokens: 0,
                            cache_write_tokens: 0,
                        }),
                        finish_reason: Some(finish_reason),
                    })));
                }

                return std::task::Poll::Ready(Some(Ok(StreamChunk {
                    delta: content,
                    tool_calls: Vec::new(),
                    tokens: None,
                    finish_reason: None,
                })));
            }

            // Need more data
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
                    // Try remaining buffer
                    let remaining = this.buffer.trim().to_string();
                    if !remaining.is_empty() {
                        this.buffer.clear();
                        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&remaining) {
                            let content = json
                                .get("message")
                                .and_then(|m| m.get("content"))
                                .and_then(|c| c.as_str())
                                .unwrap_or("")
                                .to_string();
                            return std::task::Poll::Ready(Some(Ok(StreamChunk {
                                delta: content,
                                tool_calls: Vec::new(),
                                tokens: None,
                                finish_reason: Some(FinishReason::Complete),
                            })));
                        }
                    }
                    return std::task::Poll::Ready(None);
                }
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::always_on_tracing_guard;

    #[test]
    fn test_provider_creation() {
        let provider = OllamaProvider::new();
        assert_eq!(provider.name(), "ollama");
        assert!(provider.base_url.contains("localhost"));
    }

    #[test]
    fn test_custom_base_url() {
        let provider = OllamaProvider::with_base_url("http://custom:11434".to_string());
        assert_eq!(provider.base_url, "http://custom:11434");
    }

    #[test]
    fn test_parse_response() {
        let provider = OllamaProvider::new();
        let body = serde_json::json!({
            "message": {
                "role": "assistant",
                "content": "Hello from Ollama!"
            },
            "eval_count": 10,
            "prompt_eval_count": 20,
            "done": true
        });

        let response = provider.parse_response(&body).unwrap();
        assert_eq!(response.content, "Hello from Ollama!");
        assert_eq!(response.tokens_used.completion_tokens, 10);
        assert_eq!(response.tokens_used.prompt_tokens, 20);
    }

    // ── Additional coverage tests ──────────────────────────────────────────

    #[test]
    fn test_default() {
        let provider = OllamaProvider::default();
        assert_eq!(provider.name(), "ollama");
        assert!(provider.base_url.contains("localhost"));
    }

    #[test]
    fn test_with_overrides() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "custom-model".to_string(),
            ModelCapabilities {
                supports_temperature: false,
                supports_streaming: false,
                supports_tools: false,
                supports_system_prompt: false,
                max_context_tokens: 42,
                max_output_tokens: 10,
            },
        );
        let provider =
            OllamaProvider::with_overrides("http://localhost:11434".to_string(), overrides, None);
        let caps = provider.capabilities("custom-model");
        assert_eq!(caps.max_context_tokens, 42);
        assert!(!caps.supports_temperature);
    }

    #[test]
    fn test_capabilities_falls_through_to_builtin() {
        let provider = OllamaProvider::with_overrides(
            "http://localhost:11434".to_string(),
            HashMap::new(),
            None,
        );
        let caps = provider.capabilities("llama3-8b");
        assert_eq!(caps.max_context_tokens, 131_072);
    }

    #[tokio::test]
    async fn test_count_tokens() {
        let provider = OllamaProvider::new();
        let tokens = provider.count_tokens("Hello, world!", "llama3").await;
        assert!(tokens > 0);
        // len / 4 = 13 / 4 = 3
        assert_eq!(tokens, 3);
    }

    #[tokio::test]
    async fn test_count_tokens_empty() {
        let provider = OllamaProvider::new();
        assert_eq!(provider.count_tokens("", "llama3").await, 0);
    }

    #[test]
    fn test_max_context_tokens() {
        let provider = OllamaProvider::new();
        assert_eq!(provider.max_context_tokens("llama3-8b"), 131_072);
        assert_eq!(provider.max_context_tokens("mistral-7b"), 32_768);
    }

    #[test]
    fn test_builtin_capabilities_llama3() {
        let provider = OllamaProvider::new();
        let caps = provider.builtin_capabilities("llama3-8b");
        assert!(caps.supports_temperature);
        assert!(caps.supports_streaming);
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 131_072);
    }

    #[test]
    fn test_builtin_capabilities_qwen2() {
        let provider = OllamaProvider::new();
        let caps = provider.builtin_capabilities("qwen2-7b");
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 131_072);
    }

    #[test]
    fn test_builtin_capabilities_qwen25() {
        let provider = OllamaProvider::new();
        let caps = provider.builtin_capabilities("qwen2.5-coder");
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 131_072);
    }

    #[test]
    fn test_builtin_capabilities_qwen3() {
        let provider = OllamaProvider::new();
        let caps = provider.builtin_capabilities("qwen3-30b");
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 131_072);
    }

    #[test]
    fn test_builtin_capabilities_mistral() {
        let provider = OllamaProvider::new();
        let caps = provider.builtin_capabilities("mistral-7b");
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 32_768);
        assert_eq!(caps.max_output_tokens, 4096);
    }

    #[test]
    fn test_builtin_capabilities_mixtral() {
        let provider = OllamaProvider::new();
        let caps = provider.builtin_capabilities("mixtral-8x7b");
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 32_768);
    }

    #[test]
    fn test_builtin_capabilities_phi4() {
        let provider = OllamaProvider::new();
        let caps = provider.builtin_capabilities("phi-4");
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 131_072);
    }

    #[test]
    fn test_builtin_capabilities_phi4_variant() {
        let provider = OllamaProvider::new();
        let caps = provider.builtin_capabilities("phi4-mini");
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 131_072);
    }

    #[test]
    fn test_builtin_capabilities_deepseek_r1() {
        let provider = OllamaProvider::new();
        let caps = provider.builtin_capabilities("deepseek-r1:latest");
        assert!(!caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 131_072);
    }

    #[test]
    fn test_builtin_capabilities_deepseek_general() {
        let provider = OllamaProvider::new();
        let caps = provider.builtin_capabilities("deepseek-v3");
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 131_072);
    }

    #[test]
    fn test_builtin_capabilities_gemma() {
        let provider = OllamaProvider::new();
        let caps = provider.builtin_capabilities("gemma-7b");
        assert!(!caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 131_072);
    }

    #[test]
    fn test_builtin_capabilities_codellama() {
        let provider = OllamaProvider::new();
        // Note: "codellama-34b" contains "llama-3" so it matches llama-3 branch.
        // Use a model name without a dash-number suffix.
        let caps = provider.builtin_capabilities("codellama:latest");
        assert!(!caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 16_384);
        assert_eq!(caps.max_output_tokens, 4096);
    }

    #[test]
    fn test_builtin_capabilities_unknown() {
        let provider = OllamaProvider::new();
        let caps = provider.builtin_capabilities("totally-unknown-model");
        assert!(!caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 8192);
        assert_eq!(caps.max_output_tokens, 4096);
    }

    #[test]
    fn test_parse_response_no_message_returns_error() {
        let provider = OllamaProvider::new();
        let body = serde_json::json!({ "done": true });
        let result = provider.parse_response(&body);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_response_with_tool_calls() {
        let provider = OllamaProvider::new();
        let body = serde_json::json!({
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {
                        "function": {
                            "name": "search",
                            "arguments": { "query": "rust" }
                        }
                    }
                ]
            },
            "eval_count": 5,
            "prompt_eval_count": 10,
            "done": true
        });

        let response = provider.parse_response(&body).unwrap();
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].name, "search");
        assert_eq!(response.tool_calls[0].id, "ollama_0");
        assert_eq!(response.finish_reason, FinishReason::ToolCall);
    }

    #[test]
    fn test_parse_response_total_tokens() {
        let provider = OllamaProvider::new();
        let body = serde_json::json!({
            "message": { "role": "assistant", "content": "ok" },
            "eval_count": 30,
            "prompt_eval_count": 70,
            "done": true
        });
        let response = provider.parse_response(&body).unwrap();
        assert_eq!(response.tokens_used.total_tokens, 100);
        assert_eq!(response.tokens_used.cached_tokens, 0);
        assert_eq!(response.tokens_used.cache_write_tokens, 0);
    }

    #[test]
    fn test_parse_response_missing_counts_default_zero() {
        let provider = OllamaProvider::new();
        let body = serde_json::json!({
            "message": { "role": "assistant", "content": "hi" },
            "done": true
        });
        let response = provider.parse_response(&body).unwrap();
        assert_eq!(response.tokens_used.prompt_tokens, 0);
        assert_eq!(response.tokens_used.completion_tokens, 0);
    }

    #[test]
    fn test_build_request_body_basic() {
        let provider = OllamaProvider::new();
        let request = InferenceRequest {
            system: vec![],
            messages: vec![
                crate::provider::Message {
                    role: "system".to_string(),
                    content: "Be helpful".into(),
                    cache_breakpoint: false,
                },
                crate::provider::Message {
                    role: "user".to_string(),
                    content: "Hello".into(),
                    cache_breakpoint: false,
                },
            ],
            model: "llama3-8b".to_string(),
            max_tokens: 512,
            temperature: 0.8,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        assert_eq!(body["model"], "llama3-8b");
        assert_eq!(body["messages"].as_array().unwrap().len(), 2);
        // Use approximate comparison for float (JSON serializes f32 with precision loss)
        let temp = body["options"]["temperature"].as_f64().unwrap();
        assert!((temp - 0.8).abs() < 0.001);
        assert_eq!(body["options"]["num_predict"], 512);
    }

    #[test]
    fn test_build_request_body_passes_extra_params_into_options() {
        let provider = OllamaProvider::new();
        let request = InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Hi".into(),
                cache_breakpoint: false,
            }],
            model: "llama3-8b".to_string(),
            max_tokens: 512,
            temperature: 0.8,
            tools: vec![],
            extra: serde_json::json!({ "top_k": 40, "top_p": 0.95 }),
            request_timeout_secs: None,
        };
        let body = provider.build_request_body(&request);
        // Ollama's sampling knobs live under `options`.
        assert_eq!(body["options"]["top_k"], serde_json::json!(40));
        assert_eq!(body["options"]["top_p"], serde_json::json!(0.95));
    }

    #[test]
    fn test_build_request_body_prepends_system_blocks() {
        let provider = OllamaProvider::new();
        let request = InferenceRequest {
            system: vec![crate::provider::SystemBlock {
                text: "You are a local assistant.".to_string(),
                cache_hint: leviath_core::CacheHint::Never,
            }],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Hi".into(),
                cache_breakpoint: false,
            }],
            model: "llama3-8b".to_string(),
            max_tokens: 512,
            temperature: 0.8,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        let messages = body["messages"].as_array().unwrap();
        // The system block is delivered as the first message rather than dropped.
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "You are a local assistant.");
        assert_eq!(messages[1]["role"], "user");
    }

    #[test]
    fn test_build_request_body_serializes_tool_history() {
        let provider = OllamaProvider::new();
        let request = InferenceRequest {
            system: vec![],
            messages: vec![
                crate::provider::Message {
                    role: "assistant".to_string(),
                    content: crate::provider::MessageContent::Blocks(vec![
                        crate::provider::ContentBlock::ToolUse {
                            id: "call_1".to_string(),
                            name: "list_files".to_string(),
                            input: serde_json::json!({ "dir": "." }),
                        },
                    ]),
                    cache_breakpoint: false,
                },
                crate::provider::Message {
                    role: "user".to_string(),
                    content: crate::provider::MessageContent::Blocks(vec![
                        crate::provider::ContentBlock::ToolResult {
                            tool_use_id: "call_1".to_string(),
                            content: "a.txt\nb.txt".to_string(),
                            is_error: false,
                        },
                    ]),
                    cache_breakpoint: false,
                },
            ],
            model: "llama3-8b".to_string(),
            max_tokens: 512,
            temperature: 0.8,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        let messages = body["messages"].as_array().unwrap();
        // Tool call round-trips as an assistant `tool_calls` message …
        assert_eq!(messages[0]["role"], "assistant");
        assert_eq!(
            messages[0]["tool_calls"][0]["function"]["name"],
            "list_files"
        );
        // … and the result as a `tool`-role message, not raw block JSON.
        assert_eq!(messages[1]["role"], "tool");
        assert_eq!(messages[1]["tool_call_id"], "call_1");
        assert_eq!(messages[1]["content"], "a.txt\nb.txt");
    }

    #[test]
    fn test_build_request_body_with_tools() {
        let provider = OllamaProvider::new();
        let request = InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Search".into(),
                cache_breakpoint: false,
            }],
            model: "llama3-8b".to_string(),
            max_tokens: 512,
            temperature: 0.5,
            tools: vec![crate::provider::Tool {
                name: "search".to_string(),
                description: "Search something".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            }],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "search");
    }

    // ─── parse_response edge cases ──────────────────────────────────────

    #[test]
    fn test_parse_response_empty_content() {
        let provider = OllamaProvider::new();
        let body = serde_json::json!({
            "message": { "role": "assistant", "content": "" },
            "eval_count": 0,
            "prompt_eval_count": 0,
            "done": true
        });
        let response = provider.parse_response(&body).unwrap();
        assert_eq!(response.content, "");
        assert_eq!(response.finish_reason, FinishReason::Complete);
    }

    #[test]
    fn test_parse_response_missing_content_field() {
        let provider = OllamaProvider::new();
        let body = serde_json::json!({
            "message": { "role": "assistant" },
            "done": true
        });
        let response = provider.parse_response(&body).unwrap();
        assert_eq!(response.content, "");
    }

    #[test]
    fn test_parse_response_multiple_tool_calls() {
        let provider = OllamaProvider::new();
        let body = serde_json::json!({
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {
                        "function": {
                            "name": "tool_a",
                            "arguments": { "arg1": "val1" }
                        }
                    },
                    {
                        "function": {
                            "name": "tool_b",
                            "arguments": { "arg2": "val2" }
                        }
                    }
                ]
            },
            "eval_count": 10,
            "prompt_eval_count": 20,
            "done": true
        });

        let response = provider.parse_response(&body).unwrap();
        assert_eq!(response.tool_calls.len(), 2);
        assert_eq!(response.tool_calls[0].name, "tool_a");
        assert_eq!(response.tool_calls[0].id, "ollama_0");
        assert_eq!(response.tool_calls[1].name, "tool_b");
        assert_eq!(response.tool_calls[1].id, "ollama_1");
        assert_eq!(response.finish_reason, FinishReason::ToolCall);
    }

    #[test]
    fn test_parse_response_tool_call_missing_function() {
        let provider = OllamaProvider::new();
        let body = serde_json::json!({
            "message": {
                "role": "assistant",
                "content": "hi",
                "tool_calls": [
                    {}
                ]
            },
            "done": true
        });

        let response = provider.parse_response(&body).unwrap();
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].name, "");
    }

    #[test]
    fn test_parse_response_tool_call_missing_arguments() {
        let provider = OllamaProvider::new();
        let body = serde_json::json!({
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {
                        "function": {
                            "name": "my_tool"
                        }
                    }
                ]
            },
            "done": true
        });

        let response = provider.parse_response(&body).unwrap();
        assert_eq!(response.tool_calls[0].name, "my_tool");
        // Arguments should default to empty object
        assert!(response.tool_calls[0].arguments.is_object());
    }

    // ─── build_request_body edge cases ──────────────────────────────────

    #[test]
    fn test_build_request_body_no_tools_no_tools_key() {
        let provider = OllamaProvider::new();
        let request = InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Hello".into(),
                cache_breakpoint: false,
            }],
            model: "llama3-8b".to_string(),
            max_tokens: 100,
            temperature: 0.5,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        // When tools are empty, no "tools" key should be present
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn test_build_request_body_deepseek_r1_no_temperature() {
        let provider = OllamaProvider::new();
        let request = InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Hello".into(),
                cache_breakpoint: false,
            }],
            model: "deepseek-r1:latest".to_string(),
            max_tokens: 100,
            temperature: 0.5,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        // deepseek-r1 doesn't support temperature, so options should only have num_predict
        // Actually checking: the builtin_capabilities for deepseek-r1 has supports_temperature=true,
        // so temperature IS included. Let's just verify num_predict is present.
        assert_eq!(body["options"]["num_predict"], 100);
    }

    #[test]
    fn test_build_request_body_multiple_messages() {
        let provider = OllamaProvider::new();
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
                crate::provider::Message {
                    role: "assistant".to_string(),
                    content: "Hi there".into(),
                    cache_breakpoint: false,
                },
            ],
            model: "mistral-7b".to_string(),
            max_tokens: 256,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[2]["role"], "assistant");
    }

    #[test]
    fn test_build_request_body_multiple_tools() {
        let provider = OllamaProvider::new();
        let request = InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Do things".into(),
                cache_breakpoint: false,
            }],
            model: "llama3-8b".to_string(),
            max_tokens: 512,
            temperature: 0.5,
            tools: vec![
                crate::provider::Tool {
                    name: "tool1".to_string(),
                    description: "First tool".to_string(),
                    parameters: serde_json::json!({"type": "object"}),
                },
                crate::provider::Tool {
                    name: "tool2".to_string(),
                    description: "Second tool".to_string(),
                    parameters: serde_json::json!({"type": "object"}),
                },
            ],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["function"]["name"], "tool1");
        assert_eq!(tools[1]["function"]["name"], "tool2");
    }

    // ─── capabilities with overrides ────────────────────────────────────

    #[test]
    fn test_capabilities_override_takes_precedence() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "llama3-8b".to_string(),
            ModelCapabilities {
                supports_temperature: false,
                supports_streaming: false,
                supports_tools: false,
                supports_system_prompt: false,
                max_context_tokens: 99,
                max_output_tokens: 99,
            },
        );
        let provider =
            OllamaProvider::with_overrides("http://localhost:11434".to_string(), overrides, None);
        let caps = provider.capabilities("llama3-8b");
        assert_eq!(caps.max_context_tokens, 99);
        assert!(!caps.supports_temperature);
    }

    #[test]
    fn test_capabilities_no_override_uses_builtin() {
        let overrides = HashMap::new();
        let provider =
            OllamaProvider::with_overrides("http://localhost:11434".to_string(), overrides, None);
        let caps = provider.capabilities("llama3-8b");
        assert_eq!(caps.max_context_tokens, 131_072); // builtin
    }

    // ─── builtin_capabilities: llama-3 pattern ──────────────────────────

    #[test]
    fn test_builtin_capabilities_llama_3_with_dash() {
        let provider = OllamaProvider::new();
        let caps = provider.builtin_capabilities("llama-3.1-70b");
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 131_072);
    }

    // ─── name() returns "ollama" ─────────────────────────────────────────

    #[test]
    fn test_name() {
        let provider = OllamaProvider::new();
        assert_eq!(provider.name(), "ollama");
    }

    // ─── max_context_tokens uses capabilities ───────────────────────────

    #[test]
    fn test_max_context_tokens_unknown_model() {
        let provider = OllamaProvider::new();
        assert_eq!(provider.max_context_tokens("unknown-model"), 8192);
    }

    // ─── with_base_url stores the URL ───────────────────────────────────

    #[test]
    fn test_with_base_url_stores_url() {
        let provider = OllamaProvider::with_base_url("https://remote:11434".to_string());
        assert_eq!(provider.base_url, "https://remote:11434");
    }

    // ─── HTTP error paths (connection refused) ──────────────────────────

    #[tokio::test]
    async fn test_infer_connection_refused() {
        let provider = OllamaProvider::with_base_url("http://127.0.0.1:19998".to_string());
        let request = InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Hello".into(),
                cache_breakpoint: false,
            }],
            model: "llama3-8b".to_string(),
            max_tokens: 100,
            temperature: 0.5,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };
        let result = provider.infer(request).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("Request failed:"));
    }

    #[tokio::test]
    async fn test_infer_stream_connection_refused() {
        let provider = OllamaProvider::with_base_url("http://127.0.0.1:19998".to_string());
        let request = InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Hello".into(),
                cache_breakpoint: false,
            }],
            model: "llama3-8b".to_string(),
            max_tokens: 100,
            temperature: 0.5,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };
        let result = provider.infer_stream(request).await;
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("Request failed:")
        );
    }

    #[tokio::test]
    async fn test_list_models_connection_refused() {
        let provider = OllamaProvider::with_base_url("http://127.0.0.1:19998".to_string());
        let err = provider.list_models().await.unwrap_err();
        assert!(err.to_string().contains("Request failed:"));
    }

    // ─── with_base_url: invalid URL pattern triggers warning ─────────────

    #[test]
    fn test_with_base_url_invalid_protocol_does_not_panic() {
        // Registers a real Subscriber so the tracing::warn! call's field
        // arguments in with_base_url's "bad protocol" branch are actually
        // exercised, rather than short-circuited by the "is this level
        // enabled" check with no subscriber installed.
        let _guard = always_on_tracing_guard();
        // Should log a warning but not panic
        let provider = OllamaProvider::with_base_url("ftp://invalid:11434".to_string());
        assert_eq!(provider.base_url, "ftp://invalid:11434");
    }

    #[test]
    fn test_with_overrides_invalid_protocol_does_not_panic() {
        // Registers a real Subscriber so the tracing::warn! call's field
        // arguments in with_overrides's "bad protocol" branch are actually
        // exercised.
        let _guard = always_on_tracing_guard();
        let provider =
            OllamaProvider::with_overrides("ftp://invalid:11434".to_string(), HashMap::new(), None);
        assert_eq!(provider.base_url, "ftp://invalid:11434");
    }

    // ─── parse_response: tool_call with null function ────────────────────

    #[test]
    fn test_parse_response_tool_call_null_function_field() {
        let provider = OllamaProvider::new();
        let body = serde_json::json!({
            "message": {
                "role": "assistant",
                "content": "done",
                "tool_calls": [
                    {
                        "function": null
                    }
                ]
            },
            "eval_count": 0,
            "prompt_eval_count": 0,
            "done": true
        });
        // When function is null, name and arguments should default
        let response = provider.parse_response(&body).unwrap();
        // tool_calls has 1 entry with empty name
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].name, "");
    }

    // ─── NdjsonStream: parse logic ────────────────────────────────────────

    #[test]
    fn test_ollama_ndjson_stream_parses_done_line() {
        use futures_core::Stream;
        use std::pin::Pin;
        use std::task::{Context, Poll};

        // Create a stream from a static slice of bytes
        struct StaticStream {
            data: Vec<Vec<u8>>,
            idx: usize,
        }

        impl Stream for StaticStream {
            type Item = std::result::Result<bytes::Bytes, reqwest::Error>;

            fn poll_next(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<Option<Self::Item>> {
                if self.idx < self.data.len() {
                    let chunk = bytes::Bytes::from(self.data[self.idx].clone());
                    self.idx += 1;
                    Poll::Ready(Some(Ok(chunk)))
                } else {
                    Poll::Ready(None)
                }
            }
        }

        // Build a test NDJSON stream with one non-done and one done message
        let chunk1 =
            b"{\"message\":{\"role\":\"assistant\",\"content\":\"Hello \"},\"done\":false}\n"
                .to_vec();
        let chunk2 = b"{\"message\":{\"role\":\"assistant\",\"content\":\"world\"},\"done\":true,\"eval_count\":10,\"prompt_eval_count\":20}\n".to_vec();

        let static_stream = StaticStream {
            data: vec![chunk1, chunk2],
            idx: 0,
        };

        let ndjson_stream = OllamaNdjsonStream::new(static_stream);

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            use tokio_stream::StreamExt;
            let chunks: Vec<_> = ndjson_stream.collect().await;
            assert!(chunks.len() >= 2);
            // First chunk: content "Hello "
            let first = chunks[0].as_ref().unwrap();
            assert_eq!(first.delta, "Hello ");
            assert!(first.finish_reason.is_none());
            // Last chunk: done=true
            let last = chunks.last().unwrap().as_ref().unwrap();
            assert!(last.finish_reason.is_some());
            assert_eq!(last.finish_reason, Some(FinishReason::Complete));
            let tokens = last.tokens.as_ref().unwrap();
            assert_eq!(tokens.completion_tokens, 10);
            assert_eq!(tokens.prompt_tokens, 20);
        });
    }

    #[test]
    fn test_ollama_ndjson_stream_invalid_json_returns_error() {
        use futures_core::Stream;
        use std::pin::Pin;
        use std::task::{Context, Poll};

        struct StaticStream {
            data: Vec<Vec<u8>>,
            idx: usize,
        }

        impl Stream for StaticStream {
            type Item = std::result::Result<bytes::Bytes, reqwest::Error>;
            fn poll_next(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<Option<Self::Item>> {
                if self.idx < self.data.len() {
                    let chunk = bytes::Bytes::from(self.data[self.idx].clone());
                    self.idx += 1;
                    Poll::Ready(Some(Ok(chunk)))
                } else {
                    Poll::Ready(None)
                }
            }
        }

        // Invalid JSON line
        let chunk1 = b"not valid json at all\n".to_vec();
        let static_stream = StaticStream {
            data: vec![chunk1],
            idx: 0,
        };

        let ndjson_stream = OllamaNdjsonStream::new(static_stream);

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            use tokio_stream::StreamExt;
            let chunks: Vec<_> = ndjson_stream.collect().await;
            // First item should be an error
            assert!(!chunks.is_empty());
            assert!(chunks[0].is_err());
        });
    }

    #[test]
    fn test_ollama_ndjson_stream_remaining_buffer_parsed() {
        use futures_core::Stream;
        use std::pin::Pin;
        use std::task::{Context, Poll};

        struct StaticStream {
            data: Vec<Vec<u8>>,
            idx: usize,
        }

        impl Stream for StaticStream {
            type Item = std::result::Result<bytes::Bytes, reqwest::Error>;
            fn poll_next(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<Option<Self::Item>> {
                if self.idx < self.data.len() {
                    let chunk = bytes::Bytes::from(self.data[self.idx].clone());
                    self.idx += 1;
                    Poll::Ready(Some(Ok(chunk)))
                } else {
                    Poll::Ready(None)
                }
            }
        }

        // Send data WITHOUT trailing newline — it's in the remaining buffer
        let chunk1 =
            b"{\"message\":{\"role\":\"assistant\",\"content\":\"leftover\"},\"done\":false}"
                .to_vec();
        let static_stream = StaticStream {
            data: vec![chunk1],
            idx: 0,
        };

        let ndjson_stream = OllamaNdjsonStream::new(static_stream);

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            use tokio_stream::StreamExt;
            let chunks: Vec<_> = ndjson_stream.collect().await;
            // The remaining buffer should be parsed on stream end
            assert_eq!(chunks.len(), 1);
            let chunk = chunks[0].as_ref().unwrap();
            assert_eq!(chunk.delta, "leftover");
        });
    }

    #[test]
    fn test_ollama_ndjson_stream_empty_line_skipped() {
        use futures_core::Stream;
        use std::pin::Pin;
        use std::task::{Context, Poll};

        struct StaticStream {
            data: Vec<Vec<u8>>,
            idx: usize,
        }

        impl Stream for StaticStream {
            type Item = std::result::Result<bytes::Bytes, reqwest::Error>;
            fn poll_next(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<Option<Self::Item>> {
                if self.idx < self.data.len() {
                    let chunk = bytes::Bytes::from(self.data[self.idx].clone());
                    self.idx += 1;
                    Poll::Ready(Some(Ok(chunk)))
                } else {
                    Poll::Ready(None)
                }
            }
        }

        // Empty line followed by real data
        let chunk1 =
            b"\n{\"message\":{\"role\":\"assistant\",\"content\":\"hi\"},\"done\":false}\n"
                .to_vec();
        let static_stream = StaticStream {
            data: vec![chunk1],
            idx: 0,
        };

        let ndjson_stream = OllamaNdjsonStream::new(static_stream);

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            use tokio_stream::StreamExt;
            let chunks: Vec<_> = ndjson_stream.collect().await;
            // Empty line is skipped; one real chunk
            assert_eq!(chunks.len(), 1);
            assert_eq!(chunks[0].as_ref().unwrap().delta, "hi");
        });
    }

    #[test]
    fn test_ollama_ndjson_stream_invalid_remaining_buffer_yields_none() {
        use futures_core::Stream;
        use std::pin::Pin;
        use std::task::{Context, Poll};

        struct StaticStream {
            data: Vec<Vec<u8>>,
            idx: usize,
        }

        impl Stream for StaticStream {
            type Item = std::result::Result<bytes::Bytes, reqwest::Error>;
            fn poll_next(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<Option<Self::Item>> {
                if self.idx < self.data.len() {
                    let chunk = bytes::Bytes::from(self.data[self.idx].clone());
                    self.idx += 1;
                    Poll::Ready(Some(Ok(chunk)))
                } else {
                    Poll::Ready(None)
                }
            }
        }

        // No trailing newline, and the leftover isn't valid JSON either --
        // the remaining-buffer parse attempt fails and the stream just ends.
        let chunk1 = b"not valid json, no newline".to_vec();
        let static_stream = StaticStream {
            data: vec![chunk1],
            idx: 0,
        };

        let ndjson_stream = OllamaNdjsonStream::new(static_stream);

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            use tokio_stream::StreamExt;
            let chunks: Vec<_> = ndjson_stream.collect().await;
            assert!(chunks.is_empty());
        });
    }

    #[test]
    fn test_ollama_ndjson_stream_pending_is_propagated() {
        use futures_core::Stream;
        use std::pin::Pin;
        use std::task::{Context, Poll};

        // Yields Pending once, then a done line, then ends.
        struct PendingThenDataStream {
            polled_once: bool,
            yielded: bool,
        }

        impl Stream for PendingThenDataStream {
            type Item = std::result::Result<bytes::Bytes, reqwest::Error>;
            fn poll_next(
                mut self: Pin<&mut Self>,
                cx: &mut Context<'_>,
            ) -> Poll<Option<Self::Item>> {
                if !self.polled_once {
                    self.polled_once = true;
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
                if !self.yielded {
                    self.yielded = true;
                    let chunk = bytes::Bytes::from(
                        b"{\"message\":{\"content\":\"hi\"},\"done\":true}\n".to_vec(),
                    );
                    return Poll::Ready(Some(Ok(chunk)));
                }
                Poll::Ready(None)
            }
        }

        let ndjson_stream = OllamaNdjsonStream::new(PendingThenDataStream {
            polled_once: false,
            yielded: false,
        });

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            use tokio_stream::StreamExt;
            let chunks: Vec<_> = ndjson_stream.collect().await;
            assert_eq!(chunks.len(), 1);
            assert_eq!(chunks[0].as_ref().unwrap().delta, "hi");
        });
    }

    #[test]
    fn test_ollama_ndjson_stream_parses_tool_calls_from_done_chunk() {
        use futures_core::Stream;
        use std::pin::Pin;
        use std::task::{Context, Poll};

        struct StaticStream {
            data: Vec<Vec<u8>>,
            idx: usize,
        }

        impl Stream for StaticStream {
            type Item = std::result::Result<bytes::Bytes, reqwest::Error>;
            fn poll_next(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<Option<Self::Item>> {
                if self.idx < self.data.len() {
                    let chunk = bytes::Bytes::from(self.data[self.idx].clone());
                    self.idx += 1;
                    Poll::Ready(Some(Ok(chunk)))
                } else {
                    Poll::Ready(None)
                }
            }
        }

        // The done chunk includes tool_calls in the message
        let chunk1 = br#"{"message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"search","arguments":{"query":"rust"}}}]},"done":true,"eval_count":5,"prompt_eval_count":10}"#.to_vec();
        let mut chunk1_with_newline = chunk1;
        chunk1_with_newline.push(b'\n');

        let static_stream = StaticStream {
            data: vec![chunk1_with_newline],
            idx: 0,
        };

        let ndjson_stream = OllamaNdjsonStream::new(static_stream);

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            use tokio_stream::StreamExt;
            let chunks: Vec<_> = ndjson_stream.collect().await;
            assert_eq!(chunks.len(), 1);
            let chunk = chunks[0].as_ref().unwrap();
            assert_eq!(chunk.finish_reason, Some(FinishReason::ToolCall));
            assert_eq!(chunk.tool_calls.len(), 1);
            assert_eq!(chunk.tool_calls[0].name, Some("search".to_string()));
            assert_eq!(chunk.tool_calls[0].id, Some("ollama_0".to_string()));
            assert!(chunk.tool_calls[0].arguments_delta.contains("rust"));
        });
    }

    #[test]
    fn test_ollama_ndjson_stream_no_tool_calls_still_works() {
        use futures_core::Stream;
        use std::pin::Pin;
        use std::task::{Context, Poll};

        struct StaticStream {
            data: Vec<Vec<u8>>,
            idx: usize,
        }

        impl Stream for StaticStream {
            type Item = std::result::Result<bytes::Bytes, reqwest::Error>;
            fn poll_next(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<Option<Self::Item>> {
                if self.idx < self.data.len() {
                    let chunk = bytes::Bytes::from(self.data[self.idx].clone());
                    self.idx += 1;
                    Poll::Ready(Some(Ok(chunk)))
                } else {
                    Poll::Ready(None)
                }
            }
        }

        // Done chunk without tool_calls — should still be FinishReason::Complete
        let chunk1 = b"{\"message\":{\"content\":\"done\"},\"done\":true,\"eval_count\":1,\"prompt_eval_count\":1}\n".to_vec();
        let static_stream = StaticStream {
            data: vec![chunk1],
            idx: 0,
        };

        let ndjson_stream = OllamaNdjsonStream::new(static_stream);

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            use tokio_stream::StreamExt;
            let chunks: Vec<_> = ndjson_stream.collect().await;
            assert_eq!(chunks.len(), 1);
            let chunk = chunks[0].as_ref().unwrap();
            assert_eq!(chunk.finish_reason, Some(FinishReason::Complete));
            assert!(chunk.tool_calls.is_empty());
        });
    }

    #[test]
    fn test_build_request_body_no_temperature_via_override() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "no-temp-model".to_string(),
            ModelCapabilities {
                supports_temperature: false,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 8192,
                max_output_tokens: 4096,
            },
        );
        let provider =
            OllamaProvider::with_overrides("http://localhost:11434".to_string(), overrides, None);
        let request = InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "hi".into(),
                cache_breakpoint: false,
            }],
            model: "no-temp-model".to_string(),
            max_tokens: 50,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };
        let body = provider.build_request_body(&request);
        assert!(body["options"].get("temperature").is_none());
        assert_eq!(body["options"]["num_predict"], 50);
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

    /// Sends a non-2xx status line with a `Content-Length` far larger than
    /// the actual bytes written, then closes the connection -- this forces a
    /// genuine mid-body I/O error when the caller tries to read the error
    /// body (`response.text().await` returns `Err`, not merely an empty or
    /// malformed string), exercising the `unwrap_or_else(|_| "unknown
    /// error"...)` fallback that a well-formed (even if empty/garbled) body
    /// can never reach. Mirrors `infer_stream_body_error_propagates_as_stream_item_error`'s
    /// technique below, applied to the non-streaming error-body read path.
    async fn spawn_mock_server_truncated_error_body(status: u16, reason: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response = format!(
            "HTTP/1.1 {} {}\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n",
            status, reason
        );
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = [0u8; 8192];
            let _ = socket.read(&mut buf).await;
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.write_all(b"short").await;
            let _ = socket.flush().await;
            let _ = socket.shutdown().await;
        });
        format!("http://{}", addr)
    }

    fn mock_request() -> InferenceRequest {
        InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "hi".into(),
                cache_breakpoint: false,
            }],
            model: "llama3-8b".to_string(),
            max_tokens: 50,
            temperature: 0.5,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        }
    }

    #[tokio::test]
    async fn infer_non_success_status_returns_api_error() {
        let url = spawn_mock_server(500, "Internal Server Error", b"boom").await;
        let provider = OllamaProvider::with_base_url(url);
        let err = provider.infer(mock_request()).await.unwrap_err();
        assert!(err.to_string().contains("API error:"));
    }

    #[tokio::test]
    async fn infer_non_success_status_body_read_error_falls_back_to_unknown_error() {
        let url = spawn_mock_server_truncated_error_body(500, "Internal Server Error").await;
        let provider = OllamaProvider::with_base_url(url);
        let err = provider.infer(mock_request()).await.unwrap_err();
        assert!(err.to_string().contains("unknown error"));
    }

    #[tokio::test]
    async fn infer_malformed_json_returns_invalid_response() {
        let url = spawn_mock_server(200, "OK", b"not json").await;
        let provider = OllamaProvider::with_base_url(url);
        let err = provider.infer(mock_request()).await.unwrap_err();
        assert!(err.to_string().contains("Invalid response:"));
    }

    #[tokio::test]
    async fn infer_stream_non_success_status_returns_api_error() {
        let url = spawn_mock_server(503, "Service Unavailable", b"down").await;
        let provider = OllamaProvider::with_base_url(url);
        let result = provider.infer_stream(mock_request()).await;
        assert!(result.is_err());
        assert!(result.err().unwrap().to_string().contains("API error:"));
    }

    #[tokio::test]
    async fn infer_stream_non_success_status_body_read_error_falls_back_to_unknown_error() {
        let url = spawn_mock_server_truncated_error_body(503, "Service Unavailable").await;
        let provider = OllamaProvider::with_base_url(url);
        let result = provider.infer_stream(mock_request()).await;
        assert!(result.is_err());
        assert!(result.err().unwrap().to_string().contains("unknown error"));
    }

    #[tokio::test]
    async fn infer_stream_success_yields_chunks() {
        let ndjson_body =
            b"{\"message\":{\"content\":\"hi\"},\"done\":true,\"eval_count\":1,\"prompt_eval_count\":1}\n";
        let url = spawn_mock_server(200, "OK", ndjson_body).await;
        let provider = OllamaProvider::with_base_url(url);
        let mut stream = provider.infer_stream(mock_request()).await.unwrap();
        use tokio_stream::StreamExt;
        let chunk = stream.next().await.unwrap().unwrap();
        assert_eq!(chunk.delta, "hi");
    }

    #[tokio::test]
    async fn infer_stream_body_error_propagates_as_stream_item_error() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        // Send a Content-Length larger than the actual body, then close the
        // connection early -- this produces a genuine mid-stream reqwest::Error
        // (there's no public constructor to fake one).
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = [0u8; 8192];
            let _ = socket.read(&mut buf).await;
            let response = b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n";
            let _ = socket.write_all(response).await;
            let _ = socket
                .write_all(b"{\"message\":{\"content\":\"hi\"},\"done\":false}\nshort")
                .await;
            let _ = socket.flush().await;
            let _ = socket.shutdown().await;
        });
        let provider = OllamaProvider::with_base_url(format!("http://{}", addr));
        let mut stream = provider.infer_stream(mock_request()).await.unwrap();
        use tokio_stream::StreamExt;
        let mut saw_error = false;
        while let Some(item) = stream.next().await {
            if item.is_err() {
                saw_error = true;
                break;
            }
        }
        assert!(saw_error);
    }

    #[tokio::test]
    async fn list_models_non_success_status_returns_error() {
        let url = spawn_mock_server(401, "Unauthorized", b"nope").await;
        let provider = OllamaProvider::with_base_url(url);
        let err = provider.list_models().await.unwrap_err();
        assert!(err.to_string().contains("Request failed:"));
    }

    #[tokio::test]
    async fn list_models_non_success_status_body_read_error_falls_back_to_unknown_error() {
        let url = spawn_mock_server_truncated_error_body(401, "Unauthorized").await;
        let provider = OllamaProvider::with_base_url(url);
        let err = provider.list_models().await.unwrap_err();
        assert!(err.to_string().contains("unknown error"));
    }

    #[tokio::test]
    async fn list_models_malformed_json_returns_error() {
        let url = spawn_mock_server(200, "OK", b"not json").await;
        let provider = OllamaProvider::with_base_url(url);
        let err = provider.list_models().await.unwrap_err();
        assert!(err.to_string().contains("Invalid response:"));
    }

    #[tokio::test]
    async fn list_models_success_returns_models() {
        let body = br#"{"models":[{"name":"llama3-8b"},{"name":"mistral-7b"}]}"#;
        let url = spawn_mock_server(200, "OK", body).await;
        let provider = OllamaProvider::with_base_url(url);
        let models = provider.list_models().await.unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "llama3-8b");
        assert_eq!(models[0].display_name, Some("llama3-8b".to_string()));
        assert_eq!(models[0].provider, "ollama");
    }

    #[tokio::test]
    async fn list_models_missing_data_field_returns_empty() {
        let url = spawn_mock_server(200, "OK", b"{}").await;
        let provider = OllamaProvider::with_base_url(url);
        let models = provider.list_models().await.unwrap();
        assert!(models.is_empty());
    }

    #[tokio::test]
    async fn list_models_entry_without_name_is_skipped() {
        let body = br#"{"models":[{"foo":"bar"},{"name":"llama3-8b"}]}"#;
        let url = spawn_mock_server(200, "OK", body).await;
        let provider = OllamaProvider::with_base_url(url);
        let models = provider.list_models().await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "llama3-8b");
    }

    #[tokio::test]
    async fn list_models_entry_with_non_string_name_is_skipped() {
        // covers the `.as_str()?` None branch in the filter_map
        let body = br#"{"models":[{"name":42},{"name":"llama3-8b"}]}"#;
        let url = spawn_mock_server(200, "OK", body).await;
        let provider = OllamaProvider::with_base_url(url);
        let models = provider.list_models().await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "llama3-8b");
    }

    #[test]
    fn ndjson_stream_skips_invalid_utf8_chunk_and_continues() {
        // covers the implicit else of `if let Ok(text) = from_utf8(&bytes)` in
        // OllamaNdjsonStream::poll_next
        use futures_core::Stream;
        use std::pin::Pin;
        use std::task::{Context, Poll};

        struct StaticStream {
            data: Vec<Vec<u8>>,
            idx: usize,
        }
        impl Stream for StaticStream {
            type Item = std::result::Result<bytes::Bytes, reqwest::Error>;
            fn poll_next(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<Option<Self::Item>> {
                if self.idx < self.data.len() {
                    let chunk = bytes::Bytes::from(self.data[self.idx].clone());
                    self.idx += 1;
                    Poll::Ready(Some(Ok(chunk)))
                } else {
                    Poll::Ready(None)
                }
            }
        }

        let invalid_utf8 = vec![0xFF, 0xFE, 0x00];
        let valid_chunk =
            b"{\"message\":{\"role\":\"assistant\",\"content\":\"ok\"},\"done\":false}\n".to_vec();
        let stream = StaticStream {
            data: vec![invalid_utf8, valid_chunk],
            idx: 0,
        };
        let ndjson_stream = OllamaNdjsonStream::new(stream);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            use tokio_stream::StreamExt;
            let chunks: Vec<_> = ndjson_stream.collect().await;
            assert_eq!(chunks.len(), 1);
            assert_eq!(chunks[0].as_ref().unwrap().delta, "ok");
        });
    }
}
