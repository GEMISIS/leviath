//! Shared request/response handling for OpenAI-compatible APIs.
//!
//! Used by OpenAI, Gemini, and OpenRouter providers that speak the
//! OpenAI Chat Completions format.

use crate::provider::{
    parse_openai_finish_reason, InferenceRequest, InferenceResponse, ProviderError, Result,
    StreamChunk, TokenUsage, ToolCall, ToolCallDelta,
};
use futures_core::Stream;
use std::pin::Pin;

/// Build the JSON request body for the OpenAI Chat Completions API.
pub fn build_openai_request_body(request: &InferenceRequest) -> serde_json::Value {
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

/// Parse a Chat Completions response body into an `InferenceResponse`.
pub fn parse_openai_response(body: &serde_json::Value) -> Result<InferenceResponse> {
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

// SSE stream parser for OpenAI-compatible streaming APIs.
pin_project_lite::pin_project! {
    /// SSE stream wrapper that parses OpenAI-compatible server-sent events.
    pub struct OpenAiSseStream<S> {
        #[pin]
        inner: S,
        buffer: String,
    }
}

impl<S> OpenAiSseStream<S> {
    /// Create a new SSE stream wrapper around a byte stream.
    pub fn new(inner: S) -> Self {
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
/// Returns `Some(Some(chunk))` for data, `Some(None)` for stream end, `None` for incomplete.
pub fn parse_openai_sse_event(buffer: &mut String) -> Option<Option<StreamChunk>> {
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
