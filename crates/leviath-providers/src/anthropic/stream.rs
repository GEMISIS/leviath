//! Anthropic's streamed answer, event by event.
//!
//! A turn arrives as a sequence of server-sent events - a content block opens,
//! its text or its argument JSON arrives in slices, the message ends with a
//! stop reason and its usage - and this module turns each event into the
//! [`StreamChunk`] the shared collector folds back into one response.

use super::AnthropicProvider;
use crate::provider::{ProviderError, Result, StreamChunk, TokenUsage, ToolCallDelta};
use futures_core::Stream;
use std::pin::Pin;

// The inner byte stream is boxed as a trait object rather than kept generic.
// In production this is always `reqwest`'s `bytes_stream()`; tests inject
// dozens of distinct mock stream types via `new`'s generic parameter, and a
// generic `impl<S> Stream` causes `cargo llvm-cov` to instrument each
// monomorphized `poll_next` separately, leaving some artificially "uncovered"
// even though the shared logic is fully exercised. Boxing collapses all of
// that into a single concrete `poll_next` implementation.
pub(super) struct AnthropicSseStream {
    inner: Pin<Box<dyn Stream<Item = std::result::Result<bytes::Bytes, reqwest::Error>> + Send>>,
    buffer: String,
    /// The tool call the argument deltas arriving now belong to, `None` until
    /// the first one opens. `content_block_start` carries a call's id and its
    /// name and each `input_json_delta` after it a slice of its arguments, and
    /// the collector puts them back together by index, so which block is open
    /// has to survive from one event to the next.
    open_tool_block: Option<usize>,
}

impl AnthropicSseStream {
    pub(super) fn new<S>(inner: S) -> Self
    where
        S: Stream<Item = std::result::Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
    {
        Self {
            inner: Box::pin(inner),
            buffer: String::new(),
            open_tool_block: None,
        }
    }
}

impl Stream for AnthropicSseStream {
    type Item = Result<StreamChunk>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.get_mut();

        loop {
            // Check if we have complete SSE events in the buffer
            if let Some(chunk) = parse_sse_event(&mut this.buffer, &mut this.open_tool_block) {
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
                    return std::task::Poll::Ready(Some(Err(ProviderError::transport(
                        "reading the response stream",
                        &e,
                    ))));
                }
                std::task::Poll::Ready(None) => {
                    // Stream ended - try to parse any remaining data
                    if let Some(chunk) =
                        parse_sse_event(&mut this.buffer, &mut this.open_tool_block)
                    {
                        return std::task::Poll::Ready(Some(Ok(chunk)));
                    }
                    return std::task::Poll::Ready(None);
                }
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
        }
    }
}

/// The content-block index an event names, when it names one. Anthropic
/// numbers every block and repeats that number on each event belonging to it,
/// which is the authoritative answer to "which call is this delta for"; the
/// callers fall back to the open block when it is missing.
fn event_block_index(json: &serde_json::Value) -> Option<usize> {
    json.get("index")
        .and_then(|v| v.as_u64())
        .map(|i| i as usize)
}

/// Parse a single SSE event from the buffer, consuming it if found.
/// `open_tool_block` is the call being streamed now; see the field it names.
pub(super) fn parse_sse_event(
    buffer: &mut String,
    open_tool_block: &mut Option<usize>,
) -> Option<StreamChunk> {
    // `None` until the double newline that terminates an event has arrived;
    // the caller polls again with more bytes.
    let (event_text, rest) = buffer.split_once("\n\n")?;
    let event_text = event_text.to_string();
    *buffer = rest.to_string();

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
                            // The block this delta names, or the one still
                            // open, and never a fresh number: an index of its
                            // own splits one call into an id with no arguments
                            // and arguments with no id.
                            index: event_block_index(&json).or(*open_tool_block).unwrap_or(0),
                            id: None,
                            name: None,
                            arguments_delta: partial.to_string(),
                            // Anthropic signs nothing: the signature is a
                            // Gemini 3.x requirement carried on the
                            // OpenAI-shaped wire.
                            thought_signature: None,
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
                // The number the event carries, else the one after the last
                // tool block: a second call must not overwrite the first.
                let idx = event_block_index(&json)
                    .unwrap_or_else(|| open_tool_block.map_or(0, |open| open + 1));
                *open_tool_block = Some(idx);
                Some(StreamChunk {
                    delta: String::new(),
                    tool_calls: vec![ToolCallDelta {
                        index: idx,
                        id: Some(id),
                        name: Some(name),
                        arguments_delta: String::new(),
                        thought_signature: None,
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
                tokens: Some(TokenUsage::new(0, 0, 0, output_tokens)),
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
                    tokens: Some(TokenUsage::new(input_tokens, cached, cache_write, 0)),
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
