//! Ollama provider implementation.
//!
//! Ollama provides local LLM execution via NDJSON streaming.

use crate::provider::{
    FinishReason, InferenceRequest, InferenceResponse, ModelCapabilities, ModelInfo, Provider,
    ProviderError, Result, StreamChunk, TokenUsage,
};
use async_trait::async_trait;
use futures_core::Stream;
use reqwest::Client;
use std::collections::HashMap;
use std::pin::Pin;

/// Ollama provider for local LLM execution.
pub struct OllamaProvider {
    /// HTTP client
    client: Client,

    /// API base URL (defaults to local)
    base_url: String,

    /// Per-model capability overrides
    capability_overrides: HashMap<String, ModelCapabilities>,
}

impl OllamaProvider {
    /// Create a new Ollama provider.
    pub fn new() -> Self {
        Self {
            client: Client::new(),
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
            client: Client::new(),
            base_url,
            capability_overrides: HashMap::new(),
        }
    }

    /// Create a new Ollama provider with a custom base URL and per-model capability overrides.
    pub fn with_overrides(base_url: String, overrides: HashMap<String, ModelCapabilities>) -> Self {
        if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
            tracing::warn!(url = %base_url, "Ollama base URL should start with http:// or https://");
        }
        Self {
            client: Client::new(),
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

        let response = self
            .client
            .post(format!("{}/api/chat", self.base_url))
            .header("Content-Type", "application/json")
            .json(&body)
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

        let response = self
            .client
            .post(format!("{}/api/chat", self.base_url))
            .header("Content-Type", "application/json")
            .json(&body)
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

        let byte_stream = response.bytes_stream();
        let stream = OllamaNdjsonStream::new(byte_stream);

        Ok(Box::pin(stream))
    }

    fn count_tokens(&self, text: &str, _model: &str) -> usize {
        // Approximate counting (model-dependent)
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
pin_project_lite::pin_project! {
    struct OllamaNdjsonStream<S> {
        #[pin]
        inner: S,
        buffer: String,
    }
}

impl<S> OllamaNdjsonStream<S> {
    fn new(inner: S) -> Self {
        Self {
            inner,
            buffer: String::new(),
        }
    }
}

impl<S> Stream for OllamaNdjsonStream<S>
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
            // Try to parse a complete JSON line
            if let Some(newline_pos) = this.buffer.find('\n') {
                let line = this.buffer[..newline_pos].to_string();
                *this.buffer = this.buffer[newline_pos + 1..].to_string();

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

                    return std::task::Poll::Ready(Some(Ok(StreamChunk {
                        delta: content,
                        tool_calls: Vec::new(),
                        tokens: Some(TokenUsage {
                            prompt_tokens: prompt_eval_count,
                            completion_tokens: eval_count,
                            total_tokens: prompt_eval_count + eval_count,
                        }),
                        finish_reason: Some(FinishReason::Complete),
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
}
