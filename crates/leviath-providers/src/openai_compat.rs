//! Shared request/response handling for OpenAI-compatible APIs.
//!
//! Used by OpenAI, Gemini, and OpenRouter providers that speak the
//! OpenAI Chat Completions format.

use crate::provider::{
    ContentBlock, InferenceRequest, InferenceResponse, MessageContent, ProviderError, Result,
    StreamChunk, TokenUsage, ToolCall, ToolCallDelta, check_http_response,
    parse_openai_finish_reason,
};
use crate::rate_limit::RateLimiter;
use futures_core::Stream;
use std::pin::Pin;

/// Send an OpenAI-compatible chat request and return the checked response.
///
/// Consolidates the send-and-handle lifecycle shared by every
/// OpenAI-compatible provider: optional `debug-http` request logging,
/// the `POST` with the given headers and JSON body, transport-error
/// mapping (with `debug-http` error logging), `debug-http` response
/// logging, `check_http_response`, and rate-limiter backoff reset on
/// success. Callers remain responsible for `limiter.acquire()` up front
/// and for consuming the returned `reqwest::Response` (parsing JSON and
/// recording tokens for `infer`, or `bytes_stream()` for `infer_stream`).
///
/// `headers` are applied both to the outgoing request and (feature-gated)
/// to the logged header map, so the wire request and the debug log stay
/// in sync.
#[cfg_attr(not(feature = "debug-http"), allow(unused_variables))]
pub async fn send_chat_request(
    client: &reqwest::Client,
    provider_name: &str,
    url: &str,
    headers: &[(&str, String)],
    body: &serde_json::Value,
    limiter: Option<&RateLimiter>,
    request_timeout_secs: Option<u64>,
) -> Result<reqwest::Response> {
    #[cfg(feature = "debug-http")]
    {
        let mut header_map = reqwest::header::HeaderMap::new();
        for (name, value) in headers {
            if let (Ok(header_name), Ok(header_value)) = (
                reqwest::header::HeaderName::from_bytes(name.as_bytes()),
                value.parse::<reqwest::header::HeaderValue>(),
            ) {
                header_map.insert(header_name, header_value);
            }
        }
        let body_size = serde_json::to_vec(body).map(|b| b.len()).unwrap_or(0);
        crate::debug_http::log_request(provider_name, "POST", url, &header_map, body_size);
    }
    #[cfg(feature = "debug-http")]
    let start = std::time::Instant::now();

    let mut builder =
        crate::provider::apply_request_timeout(client.post(url), request_timeout_secs);
    for (name, value) in headers {
        builder = builder.header(*name, value);
    }

    let response = builder.json(body).send().await.map_err(|e| {
        #[cfg(feature = "debug-http")]
        crate::debug_http::log_error(provider_name, url, &e.to_string());
        ProviderError::RequestFailed(e.to_string())
    })?;

    #[cfg(feature = "debug-http")]
    crate::debug_http::log_response(
        provider_name,
        url,
        response.status().as_u16(),
        response.headers(),
        response.content_length(),
        start.elapsed(),
    );

    let response = check_http_response(response, limiter).await?;

    if let Some(limiter) = limiter {
        limiter.reset_backoff().await;
    }

    Ok(response)
}

/// Build the JSON request body for the OpenAI Chat Completions API.
/// Convert one message's content into one or more OpenAI-format messages. A
/// [`MessageContent::Blocks`] message carrying tool calls/results expands to
/// several (an assistant `tool_calls` message, or one `tool`-role message per
/// result), so tool history round-trips correctly on OpenAI-compatible APIs
/// instead of being serialized raw in Anthropic block form.
pub fn message_to_openai(role: &str, content: &MessageContent) -> Vec<serde_json::Value> {
    match content {
        MessageContent::Text(text) => {
            vec![serde_json::json!({ "role": role, "content": text })]
        }
        MessageContent::Blocks(blocks) => {
            let text_parts: Vec<&str> = blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            let tool_results: Vec<(&str, &str)> = blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } => Some((tool_use_id.as_str(), content.as_str())),
                    _ => None,
                })
                .collect();
            let tool_calls: Vec<serde_json::Value> = blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse { id, name, input } => Some(serde_json::json!({
                        "id": id,
                        "type": "function",
                        "function": { "name": name, "arguments": input.to_string() }
                    })),
                    _ => None,
                })
                .collect();

            if !tool_calls.is_empty() {
                let content = text_parts.join("");
                let mut msg_json = serde_json::json!({
                    "role": "assistant",
                    "tool_calls": tool_calls,
                });
                if !content.is_empty() {
                    msg_json["content"] = serde_json::Value::String(content);
                }
                vec![msg_json]
            } else if !tool_results.is_empty() {
                tool_results
                    .iter()
                    .map(|(tool_use_id, content)| {
                        serde_json::json!({
                            "role": "tool",
                            "tool_call_id": tool_use_id,
                            "content": content,
                        })
                    })
                    .collect()
            } else {
                vec![serde_json::json!({ "role": role, "content": text_parts.join("") })]
            }
        }
    }
}

/// The full OpenAI-format message array for a request: `request.system` blocks
/// prepended as `system`-role messages, then each conversation message
/// converted via [`message_to_openai`]. Reused by every OpenAI-compatible
/// provider so system prompts and tool history are handled uniformly.
pub fn openai_messages(request: &InferenceRequest) -> Vec<serde_json::Value> {
    let mut messages: Vec<serde_json::Value> = Vec::new();
    for block in &request.system {
        messages.push(serde_json::json!({ "role": "system", "content": block.text }));
    }
    for msg in &request.messages {
        messages.extend(message_to_openai(&msg.role, &msg.content));
    }
    messages
}

pub fn build_openai_request_body(request: &InferenceRequest) -> serde_json::Value {
    let messages = openai_messages(request);

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

    merge_extra_params(
        body.as_object_mut()
            .expect("an OpenAI request body is always a JSON object"),
        &request.extra,
    );
    body
}

/// Merge a request's pass-through `extra` params (the manifest's
/// `[model.parameters]` beyond temperature/max_output_tokens - `top_p`, `stop`,
/// `seed`, `frequency_penalty`, …) into an OpenAI-shaped request `target`.
/// A non-object `extra` (e.g. `Null` when none are set) is a no-op, and keys the
/// builder already set are not overwritten (explicit request fields win).
pub fn merge_extra_params(
    target: &mut serde_json::Map<String, serde_json::Value>,
    extra: &serde_json::Value,
) {
    if let serde_json::Value::Object(params) = extra {
        for (key, value) in params {
            target.entry(key.clone()).or_insert_with(|| value.clone());
        }
    }
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
#[expect(
    clippy::string_slice,
    reason = "`event_end` is a `find` hit for the ASCII \"\\n\\n\" terminator, so it and \
              `event_end + 2` are char boundaries"
)]
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
            system: vec![],
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
            request_timeout_secs: None,
        }
    }

    // ─── merge_extra_params ─────────────────────────────────────────────────

    #[test]
    fn merge_extra_params_adds_object_keys_without_overwriting() {
        let mut target = serde_json::Map::new();
        target.insert("temperature".to_string(), serde_json::json!(0.5));
        merge_extra_params(
            &mut target,
            &serde_json::json!({ "top_p": 0.9, "temperature": 0.1, "seed": 7 }),
        );
        // New keys added …
        assert_eq!(target["top_p"], serde_json::json!(0.9));
        assert_eq!(target["seed"], serde_json::json!(7));
        // … but an existing key (an explicit request field) is not overwritten.
        assert_eq!(target["temperature"], serde_json::json!(0.5));
    }

    #[test]
    fn merge_extra_params_ignores_non_object_extra() {
        let mut target = serde_json::Map::new();
        target.insert("a".to_string(), serde_json::json!(1));
        merge_extra_params(&mut target, &serde_json::Value::Null);
        merge_extra_params(&mut target, &serde_json::json!("string"));
        assert_eq!(target.len(), 1);
    }

    #[test]
    fn build_openai_request_body_passes_through_extra_params() {
        let mut req = sample_request();
        req.extra = serde_json::json!({ "top_p": 0.8, "stop": ["END"] });
        let body = build_openai_request_body(&req);
        assert_eq!(body["top_p"], serde_json::json!(0.8));
        assert_eq!(body["stop"], serde_json::json!(["END"]));
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
            system: vec![],
            messages: vec![],
            model: "gpt-4".into(),
            max_tokens: 100,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::json!({}),
            request_timeout_secs: None,
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
        // No trailing "\n\n" - the event never completes.
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
        // for-loop finds no `data:` line to act on) - indistinguishable to
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

    // ─── MessageContent::Blocks code paths in build_openai_request_body ────

    #[test]
    fn build_request_body_blocks_tool_use_with_text() {
        let req = InferenceRequest {
            system: vec![],
            messages: vec![Message {
                role: "assistant".into(),
                content: MessageContent::Blocks(vec![
                    ContentBlock::Text {
                        text: "Let me call a tool.".into(),
                    },
                    ContentBlock::ToolUse {
                        id: "call_1".into(),
                        name: "search".into(),
                        input: serde_json::json!({"query": "rust"}),
                    },
                ]),
                cache_breakpoint: false,
            }],
            model: "gpt-4".into(),
            max_tokens: 1024,
            temperature: 0.5,
            tools: vec![],
            extra: serde_json::json!({}),
            request_timeout_secs: None,
        };
        let body = build_openai_request_body(&req);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        let msg = &messages[0];
        assert_eq!(msg["role"], "assistant");
        assert_eq!(msg["content"], "Let me call a tool.");
        let tool_calls = msg["tool_calls"].as_array().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0]["id"], "call_1");
        assert_eq!(tool_calls[0]["type"], "function");
        assert_eq!(tool_calls[0]["function"]["name"], "search");
        // arguments is serialized as a string
        let args: serde_json::Value =
            serde_json::from_str(tool_calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["query"], "rust");
    }

    #[test]
    fn build_request_body_blocks_tool_use_without_text() {
        let req = InferenceRequest {
            system: vec![],
            messages: vec![Message {
                role: "assistant".into(),
                content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                    id: "call_2".into(),
                    name: "write_file".into(),
                    input: serde_json::json!({"path": "/tmp/a.txt"}),
                }]),
                cache_breakpoint: false,
            }],
            model: "gpt-4".into(),
            max_tokens: 512,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::json!({}),
            request_timeout_secs: None,
        };
        let body = build_openai_request_body(&req);
        let msg = &body["messages"].as_array().unwrap()[0];
        assert_eq!(msg["role"], "assistant");
        // No text content → content key should be absent
        assert!(msg.get("content").is_none());
        assert_eq!(msg["tool_calls"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn build_request_body_blocks_tool_result() {
        let req = InferenceRequest {
            system: vec![],
            messages: vec![Message {
                role: "user".into(),
                content: MessageContent::Blocks(vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "call_1".into(),
                        content: "result A".into(),
                        is_error: false,
                    },
                    ContentBlock::ToolResult {
                        tool_use_id: "call_2".into(),
                        content: "result B".into(),
                        is_error: true,
                    },
                ]),
                cache_breakpoint: false,
            }],
            model: "gpt-4".into(),
            max_tokens: 1024,
            temperature: 0.5,
            tools: vec![],
            extra: serde_json::json!({}),
            request_timeout_secs: None,
        };
        let body = build_openai_request_body(&req);
        let messages = body["messages"].as_array().unwrap();
        // Each tool result becomes a separate "tool" role message
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "tool");
        assert_eq!(messages[0]["tool_call_id"], "call_1");
        assert_eq!(messages[0]["content"], "result A");
        assert_eq!(messages[1]["role"], "tool");
        assert_eq!(messages[1]["tool_call_id"], "call_2");
        assert_eq!(messages[1]["content"], "result B");
    }

    #[test]
    fn build_request_body_blocks_text_only() {
        let req = InferenceRequest {
            system: vec![],
            messages: vec![Message {
                role: "user".into(),
                content: MessageContent::Blocks(vec![
                    ContentBlock::Text {
                        text: "Hello ".into(),
                    },
                    ContentBlock::Text {
                        text: "world".into(),
                    },
                ]),
                cache_breakpoint: false,
            }],
            model: "gpt-4".into(),
            max_tokens: 256,
            temperature: 0.5,
            tools: vec![],
            extra: serde_json::json!({}),
            request_timeout_secs: None,
        };
        let body = build_openai_request_body(&req);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"], "Hello world");
    }

    #[test]
    fn build_request_body_system_blocks_prepended() {
        let req = InferenceRequest {
            system: vec![
                crate::provider::SystemBlock {
                    text: "You are helpful.".into(),
                    cache_hint: leviath_core::CacheHint::Always,
                },
                crate::provider::SystemBlock {
                    text: "Be concise.".into(),
                    cache_hint: leviath_core::CacheHint::Always,
                },
            ],
            messages: vec![Message {
                role: "user".into(),
                content: "Hi".into(),
                cache_breakpoint: false,
            }],
            model: "gpt-4".into(),
            max_tokens: 100,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::json!({}),
            request_timeout_secs: None,
        };
        let body = build_openai_request_body(&req);
        let messages = body["messages"].as_array().unwrap();
        // 2 system blocks + 1 user message
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "You are helpful.");
        assert_eq!(messages[1]["role"], "system");
        assert_eq!(messages[1]["content"], "Be concise.");
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[2]["content"], "Hi");
    }

    #[test]
    fn parse_response_tool_call_no_function_key() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "",
                    "tool_calls": [{
                        "id": "call_x"
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        });
        let resp = parse_openai_response(&body).unwrap();
        assert_eq!(resp.tool_calls.len(), 1);
        // function defaults to null, so name and arguments use defaults
        assert_eq!(resp.tool_calls[0].id, "call_x");
        assert_eq!(resp.tool_calls[0].name, "");
        assert!(resp.tool_calls[0].arguments.is_object());
    }

    #[test]
    fn parse_response_tool_call_no_id_field() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "",
                    "tool_calls": [{
                        "function": {
                            "name": "do_thing",
                            "arguments": "{\"a\":1}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        });
        let resp = parse_openai_response(&body).unwrap();
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].id, "");
        assert_eq!(resp.tool_calls[0].name, "do_thing");
        assert_eq!(resp.tool_calls[0].arguments["a"], 1);
    }

    #[tokio::test]
    async fn openai_sse_stream_propagates_real_reqwest_error() {
        use tokio_stream::StreamExt;

        let url = leviath_testkit::spawn_mock_server_truncated_body(200, "OK").await;
        let client = reqwest::Client::new();
        let resp = client.get(&url).send().await.unwrap();
        let byte_stream = resp.bytes_stream();
        let mut sse = OpenAiSseStream::new(byte_stream);

        let item = sse.next().await.expect("stream should yield an item");
        let err = item.unwrap_err();
        assert!(err.to_string().contains("Request failed:"));
    }

    async fn spawn_ok_server() -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = [0u8; 8192];
            let _ = socket.read(&mut buf).await;
            let resp =
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
            let _ = socket.write_all(resp).await;
            let _ = socket.flush().await;
            let _ = socket.shutdown().await;
        });
        format!("http://{}", addr)
    }

    #[tokio::test]
    async fn send_chat_request_success_with_limiter_resets_backoff() {
        // A 200 response on the limiter-carrying path drives the success
        // branch, including the `if let Some(limiter)` reset_backoff call.
        let url = spawn_ok_server().await;
        let client = reqwest::Client::new();
        let limiter = crate::rate_limit::RateLimiter::with_defaults();
        let body = serde_json::json!({ "model": "x" });
        let resp = send_chat_request(
            &client,
            "test",
            &url,
            &[("content-type", "application/json".to_string())],
            &body,
            Some(&limiter),
            None,
        )
        .await;
        assert!(resp.is_ok());
    }

    #[tokio::test]
    async fn send_chat_request_tolerates_unparseable_debug_header() {
        // Under the `debug-http` feature (which the coverage build enables),
        // building the debug header map skips any header whose name/value
        // won't parse (the `if let (Ok, Ok)` false arm) rather than panicking.
        // The request send itself fails (unroutable address, and reqwest also
        // rejects the bad header), which is fine - the debug-http block runs
        // first, before the send.
        let client = reqwest::Client::new();
        let body = serde_json::json!({ "model": "x" });
        let _ = send_chat_request(
            &client,
            "test",
            "http://127.0.0.1:1/v1/chat/completions",
            &[
                ("bad header name", "v".to_string()),
                ("content-type", "application/json".to_string()),
            ],
            &body,
            None,
            None,
        )
        .await;
    }
}
