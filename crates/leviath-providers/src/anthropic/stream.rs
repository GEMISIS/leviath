//! Anthropic's streamed answer, event by event.
//!
//! A turn arrives as a sequence of server-sent events - a content block opens,
//! its text or its argument JSON arrives in slices, the message ends with a
//! stop reason and its usage - and this module turns each event into the
//! [`StreamChunk`] the shared collector folds back into one response.

use super::AnthropicProvider;
use crate::provider::{StreamChunk, TokenUsage, ToolCallDelta};
use futures_core::Stream;

/// Wrap a byte stream in Anthropic's server-sent-events framer.
///
/// The framer carries the open tool block between events (see
/// [`parse_sse_event`]), which is why it is a closure over that state rather
/// than a bare `fn`. `parse_sse_event` answers `None` both for "no complete
/// event yet" and for an event that carries nothing (`ping`, `message_stop`);
/// either way the stream polls for more bytes, exactly as it did before.
pub(super) fn anthropic_sse_stream<S>(inner: S) -> crate::provider::stream::FramedStream
where
    S: Stream<Item = std::result::Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
{
    let mut open_tool_block: Option<usize> = None;
    crate::provider::stream::FramedStream::new(
        inner,
        Box::new(move |buffer: &mut String| {
            parse_sse_event(buffer, &mut open_tool_block).map(|chunk| Some(Ok(chunk)))
        }),
        None,
    )
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
