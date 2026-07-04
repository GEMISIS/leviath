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

    let cached_tokens = usage
        .and_then(|u| u.get("prompt_tokens_details"))
        .and_then(|d| d.get("cached_tokens"))
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
            cache_write_tokens: 0,
        },
        finish_reason: parse_openai_finish_reason(finish_reason),
    })
}

// SSE stream parser for OpenAI-compatible streaming APIs.
//
// The inner byte stream is boxed as a trait object rather than kept generic.
// In production this is always `reqwest`'s `bytes_stream()`; tests inject
// dozens of distinct mock stream types via `new`'s generic parameter, and a
// generic `impl<S> Stream` causes `cargo llvm-cov` to instrument each
// monomorphized `poll_next` separately, leaving some artificially "uncovered"
// even though the shared logic is fully exercised. Boxing collapses all of
// that into a single concrete `poll_next` implementation.
/// SSE stream wrapper that parses OpenAI-compatible server-sent events.
pub struct OpenAiSseStream {
    inner: Pin<Box<dyn Stream<Item = std::result::Result<bytes::Bytes, reqwest::Error>> + Send>>,
    buffer: String,
}

impl OpenAiSseStream {
    /// Create a new SSE stream wrapper around a byte stream.
    pub fn new<S>(inner: S) -> Self
    where
        S: Stream<Item = std::result::Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
    {
        Self {
            inner: Box::pin(inner),
            buffer: String::new(),
        }
    }
}

impl Stream for OpenAiSseStream {
    type Item = Result<StreamChunk>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.get_mut();

        loop {
            // Check for complete SSE events
            if let Some(chunk) = parse_openai_sse_event(&mut this.buffer) {
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
                    if let Some(chunk) = parse_openai_sse_event(&mut this.buffer) {
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
                    let cached_tokens = usage
                        .get("prompt_tokens_details")
                        .and_then(|d| d.get("cached_tokens"))
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as usize;
                    return Some(Some(StreamChunk {
                        delta: String::new(),
                        tool_calls: Vec::new(),
                        tokens: Some(TokenUsage {
                            prompt_tokens,
                            completion_tokens,
                            total_tokens: prompt_tokens + completion_tokens,
                            cached_tokens,
                            cache_write_tokens: 0,
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
                let cached = usage
                    .get("prompt_tokens_details")
                    .and_then(|d| d.get("cached_tokens"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize;
                TokenUsage {
                    prompt_tokens: pt,
                    completion_tokens: ct,
                    total_tokens: pt + ct,
                    cached_tokens: cached,
                    cache_write_tokens: 0,
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
    use crate::provider::{InferenceRequest, Message, Tool};

    fn sample_request() -> InferenceRequest {
        InferenceRequest {
            messages: vec![
                Message {
                    role: "system".into(),
                    content: "You are helpful".into(),
                    cache_breakpoint: false,
                },
                Message {
                    role: "user".into(),
                    content: "Hello".into(),
                    cache_breakpoint: false,
                },
            ],
            model: "gpt-4".into(),
            max_tokens: 1024,
            temperature: 0.5,
            tools: vec![],
            extra: serde_json::json!({}),
        }
    }

    // ─── build_openai_request_body ──────────────────────────────────────────

    #[test]
    fn build_request_body_basic() {
        let req = sample_request();
        let body = build_openai_request_body(&req);
        assert_eq!(body["model"], "gpt-4");
        assert_eq!(body["max_tokens"], 1024);
        assert_eq!(body["temperature"], 0.5);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["content"], "Hello");
    }

    #[test]
    fn build_request_body_no_tools_omits_tools_key() {
        let req = sample_request();
        let body = build_openai_request_body(&req);
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn build_request_body_with_tools() {
        let mut req = sample_request();
        req.tools = vec![Tool {
            name: "search".into(),
            description: "Search the web".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }];
        let body = build_openai_request_body(&req);
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "search");
        assert_eq!(tools[0]["function"]["description"], "Search the web");
    }

    #[test]
    fn build_request_body_multiple_tools() {
        let mut req = sample_request();
        req.tools = vec![
            Tool {
                name: "tool_a".into(),
                description: "A".into(),
                parameters: serde_json::json!({}),
            },
            Tool {
                name: "tool_b".into(),
                description: "B".into(),
                parameters: serde_json::json!({}),
            },
        ];
        let body = build_openai_request_body(&req);
        assert_eq!(body["tools"].as_array().unwrap().len(), 2);
    }

    // ─── parse_openai_response ──────────────────────────────────────────────

    #[test]
    fn parse_response_basic() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "Hello there!"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5
            }
        });
        let resp = parse_openai_response(&body).unwrap();
        assert_eq!(resp.content, "Hello there!");
        assert!(resp.tool_calls.is_empty());
        assert_eq!(resp.tokens_used.prompt_tokens, 10);
        assert_eq!(resp.tokens_used.completion_tokens, 5);
        assert_eq!(resp.tokens_used.total_tokens, 15);
        assert_eq!(resp.finish_reason, crate::provider::FinishReason::Complete);
    }

    #[test]
    fn parse_response_no_choices_returns_error() {
        let body = serde_json::json!({});
        let err = parse_openai_response(&body).unwrap_err();
        assert!(err.to_string().contains("No choices"));
    }

    #[test]
    fn parse_response_no_message_returns_error() {
        let body = serde_json::json!({
            "choices": [{}]
        });
        let err = parse_openai_response(&body).unwrap_err();
        assert!(err.to_string().contains("No message"));
    }

    #[test]
    fn parse_response_with_tool_calls() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "function": {
                            "name": "get_weather",
                            "arguments": "{\"city\":\"NYC\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 20, "completion_tokens": 10}
        });
        let resp = parse_openai_response(&body).unwrap();
        assert_eq!(resp.content, "");
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].id, "call_1");
        assert_eq!(resp.tool_calls[0].name, "get_weather");
        assert_eq!(resp.tool_calls[0].arguments["city"], "NYC");
        assert_eq!(resp.finish_reason, crate::provider::FinishReason::ToolCall);
    }

    #[test]
    fn parse_response_cached_tokens() {
        let body = serde_json::json!({
            "choices": [{
                "message": {"content": "ok"},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 10,
                "prompt_tokens_details": {
                    "cached_tokens": 80
                }
            }
        });
        let resp = parse_openai_response(&body).unwrap();
        assert_eq!(resp.tokens_used.cached_tokens, 80);
    }

    #[test]
    fn parse_response_missing_usage_defaults_to_zero() {
        let body = serde_json::json!({
            "choices": [{
                "message": {"content": "hi"},
                "finish_reason": "stop"
            }]
        });
        let resp = parse_openai_response(&body).unwrap();
        assert_eq!(resp.tokens_used.prompt_tokens, 0);
        assert_eq!(resp.tokens_used.completion_tokens, 0);
    }

    #[test]
    fn parse_response_finish_reason_length() {
        let body = serde_json::json!({
            "choices": [{
                "message": {"content": "truncated"},
                "finish_reason": "length"
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 5}
        });
        let resp = parse_openai_response(&body).unwrap();
        assert_eq!(
            resp.finish_reason,
            crate::provider::FinishReason::TokenLimit
        );
    }

    // ─── parse_openai_sse_event ─────────────────────────────────────────────

    #[test]
    fn sse_event_incomplete_returns_none() {
        let mut buf = "data: {\"choices\":[".to_string();
        assert!(parse_openai_sse_event(&mut buf).is_none());
    }

    #[test]
    fn sse_event_done_returns_stream_end() {
        let mut buf = "data: [DONE]\n\n".to_string();
        let result = parse_openai_sse_event(&mut buf);
        assert!(result.is_some() && result.unwrap().is_none());
        assert!(buf.is_empty());
    }

    #[test]
    fn sse_event_content_delta() {
        let mut buf = format!(
            "data: {}\n\n",
            serde_json::json!({
                "choices": [{
                    "delta": {"content": "Hello"},
                    "finish_reason": null
                }]
            })
        );
        let result = parse_openai_sse_event(&mut buf);
        let chunk = result.unwrap().unwrap();
        assert_eq!(chunk.delta, "Hello");
        assert!(chunk.finish_reason.is_none());
        assert!(chunk.tool_calls.is_empty());
    }

    #[test]
    fn sse_event_with_finish_reason() {
        let mut buf = format!(
            "data: {}\n\n",
            serde_json::json!({
                "choices": [{
                    "delta": {"content": ""},
                    "finish_reason": "stop"
                }]
            })
        );
        let chunk = parse_openai_sse_event(&mut buf).unwrap().unwrap();
        assert_eq!(
            chunk.finish_reason,
            Some(crate::provider::FinishReason::Complete)
        );
    }

    #[test]
    fn sse_event_tool_call_delta() {
        let mut buf = format!(
            "data: {}\n\n",
            serde_json::json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_abc",
                            "function": {
                                "name": "search",
                                "arguments": "{\"q\":"
                            }
                        }]
                    }
                }]
            })
        );
        let chunk = parse_openai_sse_event(&mut buf).unwrap().unwrap();
        assert_eq!(chunk.tool_calls.len(), 1);
        assert_eq!(chunk.tool_calls[0].index, 0);
        assert_eq!(chunk.tool_calls[0].id.as_deref(), Some("call_abc"));
        assert_eq!(chunk.tool_calls[0].name.as_deref(), Some("search"));
        assert_eq!(chunk.tool_calls[0].arguments_delta, "{\"q\":");
    }

    #[test]
    fn sse_event_usage_only_chunk() {
        let mut buf = format!(
            "data: {}\n\n",
            serde_json::json!({
                "usage": {
                    "prompt_tokens": 50,
                    "completion_tokens": 25,
                    "prompt_tokens_details": {"cached_tokens": 10}
                }
            })
        );
        let chunk = parse_openai_sse_event(&mut buf).unwrap().unwrap();
        assert_eq!(chunk.delta, "");
        let tokens = chunk.tokens.unwrap();
        assert_eq!(tokens.prompt_tokens, 50);
        assert_eq!(tokens.completion_tokens, 25);
        assert_eq!(tokens.cached_tokens, 10);
    }

    #[test]
    fn sse_event_multiple_events_in_buffer() {
        let mut buf = format!(
            "data: {}\n\ndata: {}\n\n",
            serde_json::json!({"choices": [{"delta": {"content": "A"}}]}),
            serde_json::json!({"choices": [{"delta": {"content": "B"}}]})
        );
        let chunk1 = parse_openai_sse_event(&mut buf).unwrap().unwrap();
        assert_eq!(chunk1.delta, "A");

        let chunk2 = parse_openai_sse_event(&mut buf).unwrap().unwrap();
        assert_eq!(chunk2.delta, "B");
    }

    #[test]
    fn sse_event_with_usage_in_choice_chunk() {
        let mut buf = format!(
            "data: {}\n\n",
            serde_json::json!({
                "choices": [{"delta": {"content": "X"}, "finish_reason": "stop"}],
                "usage": {
                    "prompt_tokens": 100,
                    "completion_tokens": 50,
                    "prompt_tokens_details": {"cached_tokens": 30}
                }
            })
        );
        let chunk = parse_openai_sse_event(&mut buf).unwrap().unwrap();
        assert_eq!(chunk.delta, "X");
        let tokens = chunk.tokens.unwrap();
        assert_eq!(tokens.prompt_tokens, 100);
        assert_eq!(tokens.cached_tokens, 30);
    }

    #[test]
    fn sse_event_invalid_json_skipped() {
        let mut buf = "data: not-json\n\n".to_string();
        // Invalid JSON line is skipped; no valid data line follows → None
        assert!(parse_openai_sse_event(&mut buf).is_none());
    }

    // ── Additional coverage tests ──────────────────────────────────────────

    #[test]
    fn build_request_body_empty_messages() {
        let req = InferenceRequest {
            messages: vec![],
            model: "gpt-4".into(),
            max_tokens: 100,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::json!({}),
        };
        let body = build_openai_request_body(&req);
        assert_eq!(body["messages"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn parse_response_multiple_tool_calls() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "",
                    "tool_calls": [
                        {
                            "id": "call_1",
                            "function": { "name": "search", "arguments": "{}" }
                        },
                        {
                            "id": "call_2",
                            "function": { "name": "write", "arguments": "{\"file\":\"a.txt\"}" }
                        }
                    ]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        });
        let resp = parse_openai_response(&body).unwrap();
        assert_eq!(resp.tool_calls.len(), 2);
        assert_eq!(resp.tool_calls[0].name, "search");
        assert_eq!(resp.tool_calls[1].name, "write");
        assert_eq!(resp.tool_calls[1].arguments["file"], "a.txt");
    }

    #[test]
    fn parse_response_malformed_tool_arguments_defaults_to_empty_object() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "",
                    "tool_calls": [{
                        "id": "call_1",
                        "function": { "name": "test", "arguments": "not-valid-json" }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 5}
        });
        let resp = parse_openai_response(&body).unwrap();
        assert_eq!(resp.tool_calls.len(), 1);
        assert!(resp.tool_calls[0].arguments.is_object());
    }

    #[test]
    fn sse_event_empty_buffer() {
        let mut buf = String::new();
        assert!(parse_openai_sse_event(&mut buf).is_none());
    }

    #[test]
    fn sse_event_consumes_only_first_event() {
        let mut buf = format!(
            "data: {}\n\ndata: [DONE]\n\n",
            serde_json::json!({"choices": [{"delta": {"content": "X"}}]})
        );
        let chunk = parse_openai_sse_event(&mut buf).unwrap().unwrap();
        assert_eq!(chunk.delta, "X");
        // Buffer should still have the [DONE] event
        assert!(buf.contains("[DONE]"));
    }

    #[test]
    fn sse_event_no_data_prefix_skipped() {
        // Event with no "data: " prefix line
        let mut buf = "event: something\n\n".to_string();
        assert!(parse_openai_sse_event(&mut buf).is_none());
    }

    #[test]
    fn sse_event_tool_call_delta_no_id() {
        let mut buf = format!(
            "data: {}\n\n",
            serde_json::json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 1,
                            "function": {
                                "arguments": "\"val\"}"
                            }
                        }]
                    }
                }]
            })
        );
        let chunk = parse_openai_sse_event(&mut buf).unwrap().unwrap();
        assert_eq!(chunk.tool_calls.len(), 1);
        assert_eq!(chunk.tool_calls[0].index, 1);
        assert!(chunk.tool_calls[0].id.is_none());
        assert!(chunk.tool_calls[0].name.is_none());
    }

    #[test]
    fn sse_event_no_choices_no_usage_continues() {
        let mut buf = format!("data: {}\n\n", serde_json::json!({"id": "chatcmpl-123"}));
        // No choices and no usage → should continue, effectively None since
        // no data line produces a result
        let result = parse_openai_sse_event(&mut buf);
        assert!(result.is_none());
    }

    // ─── OpenAiSseStream (Stream-level, not just parse_openai_sse_event) ───

    struct StaticByteStream {
        data: Vec<Vec<u8>>,
        idx: usize,
    }

    impl futures_core::Stream for StaticByteStream {
        type Item = std::result::Result<bytes::Bytes, reqwest::Error>;
        fn poll_next(
            mut self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            if self.idx < self.data.len() {
                let chunk = bytes::Bytes::from(self.data[self.idx].clone());
                self.idx += 1;
                std::task::Poll::Ready(Some(Ok(chunk)))
            } else {
                std::task::Poll::Ready(None)
            }
        }
    }

    #[tokio::test]
    async fn openai_sse_stream_yields_content_delta() {
        use tokio_stream::StreamExt;
        let data = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n".to_vec();
        let stream = StaticByteStream {
            data: vec![data],
            idx: 0,
        };
        let mut sse = OpenAiSseStream::new(stream);
        let chunk = sse.next().await.unwrap().unwrap();
        assert_eq!(chunk.delta, "hi");
    }

    #[tokio::test]
    async fn openai_sse_stream_done_marker_ends_stream() {
        use tokio_stream::StreamExt;
        let data = b"data: [DONE]\n\n".to_vec();
        let stream = StaticByteStream {
            data: vec![data],
            idx: 0,
        };
        let mut sse = OpenAiSseStream::new(stream);
        assert!(sse.next().await.is_none());
    }

    #[tokio::test]
    async fn openai_sse_stream_ends_with_incomplete_buffer_returns_none() {
        use tokio_stream::StreamExt;
        // No trailing "\n\n" — the event never completes.
        let data = b"data: {\"choices\":[{\"delta\":{}}]}".to_vec();
        let stream = StaticByteStream {
            data: vec![data],
            idx: 0,
        };
        let mut sse = OpenAiSseStream::new(stream);
        assert!(sse.next().await.is_none());
    }

    #[tokio::test]
    async fn openai_sse_stream_multiple_chunks_across_reads() {
        use tokio_stream::StreamExt;
        // First read has no complete event; second read completes it.
        let stream = StaticByteStream {
            data: vec![
                b"data: {\"choices\":".to_vec(),
                b"[{\"delta\":{\"content\":\"ok\"}}]}\n\n".to_vec(),
            ],
            idx: 0,
        };
        let mut sse = OpenAiSseStream::new(stream);
        let chunk = sse.next().await.unwrap().unwrap();
        assert_eq!(chunk.delta, "ok");
    }

    #[tokio::test]
    async fn openai_sse_stream_flushes_trailing_buffered_event_when_stream_ends() {
        use tokio_stream::StreamExt;
        // A comment-only SSE event (no `data:` line) glued directly to a real
        // data event in the SAME chunk. `parse_openai_sse_event` consumes the
        // comment event from the buffer but returns plain `None` (its
        // for-loop finds no `data:` line to act on) -- indistinguishable to
        // the caller from "incomplete". So `poll_next`'s top-of-loop check
        // falls through and polls the inner stream again, which then reports
        // end-of-stream; only *there* does poll_next's own end-of-stream
        // re-check find the still-buffered, still-unconsumed data event.
        let data = b": ping\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n".to_vec();
        let stream = StaticByteStream {
            data: vec![data],
            idx: 0,
        };
        let mut sse = OpenAiSseStream::new(stream);
        let chunk = sse.next().await.unwrap().unwrap();
        assert_eq!(chunk.delta, "hi");
        assert!(sse.next().await.is_none());
    }

    #[tokio::test]
    async fn openai_sse_stream_skips_invalid_utf8_chunk_and_continues() {
        // covers the implicit else of `if let Ok(text) = from_utf8(&bytes)`
        use tokio_stream::StreamExt;
        let stream = StaticByteStream {
            data: vec![
                vec![0xFF, 0xFE, 0x00],
                b"data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n".to_vec(),
            ],
            idx: 0,
        };
        let mut sse = OpenAiSseStream::new(stream);
        let chunk = sse.next().await.unwrap().unwrap();
        assert_eq!(chunk.delta, "ok");
    }

    /// Declares a `Content-Length` far larger than the bytes actually sent,
    /// then closes the connection -- forcing a genuine `reqwest::Error` when
    /// the byte stream itself is polled (not just `.text()`), so
    /// `OpenAiSseStream`'s `Poll::Ready(Some(Err(e)))` arm is reachable.
    /// `reqwest::Error` has no public constructor, so a real (truncated) HTTP
    /// response is the only way to produce one.
    async fn spawn_mock_server_truncated_body() -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 10000\r\nConnection: close\r\n\r\nshort".to_vec();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = [0u8; 8192];
            let _ = socket.read(&mut buf).await;
            let _ = socket.write_all(&response).await;
            let _ = socket.flush().await;
            let _ = socket.shutdown().await;
        });

        format!("http://{}", addr)
    }

    #[tokio::test]
    async fn openai_sse_stream_propagates_real_reqwest_error() {
        use tokio_stream::StreamExt;

        let url = spawn_mock_server_truncated_body().await;
        let client = reqwest::Client::new();
        let resp = client.get(&url).send().await.unwrap();
        let byte_stream = resp.bytes_stream();
        let mut sse = OpenAiSseStream::new(byte_stream);

        let item = sse.next().await.expect("stream should yield an item");
        let err = item.unwrap_err();
        assert!(err.to_string().contains("Request failed:"));
    }
}
