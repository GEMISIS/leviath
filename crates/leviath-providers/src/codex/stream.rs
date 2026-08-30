//! The Codex Responses stream, event by event.
//!
//! A turn arrives as server-sent events: an output item opens, its text or its
//! argument JSON arrives in slices, the item closes carrying its finished form,
//! and the response ends with usage and a status.
//!
//! **`response.completed` cannot be used to read the output.** Measured against
//! the live route: under the mandatory `store: false`, the terminal event's
//! `response.output` array is *always* empty. Every tool call, every reasoning
//! item and the assistant text exist only in the stream. A parser written
//! against the terminal event compiles, connects, and silently never returns a
//! tool call.
//!
//! So `response.output_item.done` is the source of truth for items, and the
//! text deltas for prose. The terminal event contributes usage and status only.

use crate::provider::{FinishReason, ProviderError, StreamChunk, TokenUsage, ToolCallDelta};
use futures_core::Stream;

/// State carried across events within one response.
#[derive(Default)]
pub(super) struct Turn {
    /// Whether any function call was seen, which decides the finish reason:
    /// the terminal event reports `completed` either way.
    saw_tool_call: bool,
}

/// Wrap a byte stream in the Codex server-sent-events framer.
///
/// A closure over [`Turn`] rather than a bare `fn` because the finish reason
/// depends on what arrived earlier in the same response.
pub(super) fn codex_sse_stream<S>(inner: S) -> crate::provider::stream::FramedStream
where
    S: Stream<Item = std::result::Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
{
    let mut turn = Turn::default();
    crate::provider::stream::FramedStream::new(
        inner,
        Box::new(move |buffer: &mut String| parse_event(buffer, &mut turn)),
        // Every event is terminated by a blank line, so a leftover is a torn
        // frame with nothing to recover.
        None,
    )
}

/// Parse one event, consuming it from the buffer.
///
/// `None` means "no complete event yet, or nothing worth emitting"; the caller
/// polls again. `Some(None)` ends the stream. `Some(Some(..))` is a chunk or an
/// error the server delivered inside a 200.
pub(super) fn parse_event(
    buffer: &mut String,
    turn: &mut Turn,
) -> Option<Option<crate::provider::Result<StreamChunk>>> {
    let (event_text, rest) = buffer.split_once("\n\n")?;
    let event_text = event_text.to_string();
    *buffer = rest.to_string();

    let mut data = String::new();
    for line in event_text.lines() {
        if let Some(d) = line.strip_prefix("data: ") {
            data = d.to_string();
        }
    }
    if data.is_empty() {
        return None;
    }
    let json: serde_json::Value = serde_json::from_str(&data).ok()?;

    // The `type` inside the payload, not the `event:` line. Both are sent, and
    // the JSON is the one that is always present and always authoritative.
    let kind = json
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or_default();

    match kind {
        "response.output_text.delta" => {
            let text = json.get("delta").and_then(|d| d.as_str()).unwrap_or("");
            Some(Some(Ok(StreamChunk {
                delta: text.to_string(),
                tool_calls: vec![],
                tokens: None,
                finish_reason: None,
                reasoning: None,
            })))
        }

        // Opens a call: the only event carrying its id and name.
        "response.output_item.added" => {
            let item = json.get("item")?;
            if item.get("type").and_then(|t| t.as_str()) != Some("function_call") {
                return None;
            }
            turn.saw_tool_call = true;
            Some(Some(Ok(StreamChunk {
                delta: String::new(),
                tool_calls: vec![ToolCallDelta {
                    // From `output_index`, never a running counter. A fresh
                    // number splits one call into an id with no arguments and
                    // arguments with an empty id.
                    index: output_index(&json),
                    // `call_id`, not the item `id`: only this one may be
                    // echoed back on the matching output.
                    id: item
                        .get("call_id")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    name: item
                        .get("name")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    arguments_delta: String::new(),
                    thought_signature: None,
                }],
                tokens: None,
                finish_reason: None,
                reasoning: None,
            })))
        }

        "response.function_call_arguments.delta" => {
            let delta = json.get("delta").and_then(|d| d.as_str()).unwrap_or("");
            Some(Some(Ok(StreamChunk {
                delta: String::new(),
                tool_calls: vec![ToolCallDelta {
                    index: output_index(&json),
                    id: None,
                    name: None,
                    arguments_delta: delta.to_string(),
                    thought_signature: None,
                }],
                tokens: None,
                finish_reason: None,
                reasoning: None,
            })))
        }

        // The finished item. Only the reasoning blob is taken from here: the
        // arguments already arrived as deltas, and re-emitting them would
        // double every call's argument text.
        "response.output_item.done" => {
            let item = json.get("item")?;
            if item.get("type").and_then(|t| t.as_str()) != Some("reasoning") {
                return None;
            }
            let blob = item.get("encrypted_content").and_then(|v| v.as_str())?;
            Some(Some(Ok(StreamChunk {
                delta: String::new(),
                tool_calls: vec![],
                tokens: None,
                finish_reason: None,
                reasoning: Some(blob.to_string()),
            })))
        }

        "response.completed" => {
            let response = json.get("response")?;
            Some(Some(Ok(StreamChunk {
                delta: String::new(),
                tool_calls: vec![],
                tokens: Some(usage_of(response)),
                finish_reason: Some(match turn.saw_tool_call {
                    true => FinishReason::ToolCall,
                    false => FinishReason::Complete,
                }),
                reasoning: None,
            })))
        }

        "response.incomplete" => {
            let response = json.get("response")?;
            Some(Some(Ok(StreamChunk {
                delta: String::new(),
                tool_calls: vec![],
                tokens: Some(usage_of(response)),
                finish_reason: Some(FinishReason::TokenLimit),
                reasoning: None,
            })))
        }

        // An error the server found after committing to a 200. Without its own
        // arm this falls through and the stream ends cleanly, leaving a
        // truncated answer with nothing anywhere saying why.
        "response.failed" | "error" => Some(Some(Err(ProviderError::ApiError(format!(
            "the model provider reported: {}",
            failure_message(&json)
        ))))),

        _ => None,
    }
}

/// Which output item an event belongs to.
fn output_index(json: &serde_json::Value) -> usize {
    json.get("output_index")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize
}

/// Read the usage block, keeping the three input counts disjoint.
///
/// `input_tokens` *includes* `cached_tokens`, so the fresh figure is the
/// difference; counting it whole would bill the cached prefix twice, once at
/// the full rate and once at the cache rate. `reasoning_tokens` is a subset of
/// `output_tokens` and is deliberately not added.
fn usage_of(response: &serde_json::Value) -> TokenUsage {
    let usage = response.get("usage");
    let number = |node: Option<&serde_json::Value>, key: &str| -> usize {
        node.and_then(|n| n.get(key))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as usize
    };
    let details = usage.and_then(|u| u.get("input_tokens_details"));

    let input = number(usage, "input_tokens");
    let cached = number(details, "cached_tokens");
    let written = number(details, "cache_write_tokens");
    TokenUsage::new(
        // Saturating because a server reporting details larger than its own
        // total is malformed, and clamping beats wrapping under
        // `overflow-checks`.
        input.saturating_sub(cached).saturating_sub(written),
        cached,
        written,
        number(usage, "output_tokens"),
    )
}

/// The most useful sentence in a failure event.
fn failure_message(json: &serde_json::Value) -> String {
    let nested = json.get("response").and_then(|r| r.get("error"));
    for node in [json.get("error"), nested] {
        if let Some(message) = node
            .and_then(|e| e.get("message"))
            .and_then(serde_json::Value::as_str)
        {
            return message.to_string();
        }
    }
    json.get("message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("the response stream failed without a reason")
        .to_string()
}

#[cfg(test)]
mod tests;
