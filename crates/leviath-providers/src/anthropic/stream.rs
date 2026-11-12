//! Anthropic's streamed answer, event by event.
//!
//! A turn arrives as a sequence of server-sent events - a content block opens,
//! its text or its argument JSON arrives in slices, the message ends with a
//! stop reason and its usage - and this module turns each event into the
//! [`StreamChunk`] the shared collector folds back into one response.

use super::AnthropicProvider;
use crate::failure::FailureKind;
use crate::provider::{ProviderError, Result, StreamChunk, TokenUsage, ToolCallDelta};
use futures_core::Stream;

/// Wrap a byte stream in Anthropic's server-sent-events framer.
///
/// The framer carries the open tool block between events (see
/// [`parse_sse_event`]), which is why it is a closure over that state rather
/// than a bare `fn`. `parse_sse_event` answers `None` both for "no complete
/// event yet" and for an event that carries nothing (`ping`, `message_stop`);
/// either way the stream polls for more bytes. An `error` event is the
/// stream's error item.
pub(super) fn anthropic_sse_stream<S>(inner: S) -> crate::provider::stream::FramedStream
where
    S: Stream<Item = std::result::Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
{
    let mut turn = Turn::default();
    crate::provider::stream::FramedStream::new(
        inner,
        Box::new(move |buffer: &mut String| parse_turn_event(buffer, &mut turn).map(Some)),
        None,
    )
}

/// What a stream's parser carries from one event to the next.
#[derive(Debug, Default)]
pub(super) struct Turn {
    /// The call being streamed now.
    pub(super) open_tool_block: Option<usize>,
    /// The `usage` object `message_start` carried: the input and cache
    /// counts, which the closing event repeats only in part.
    pub(super) started: Option<serde_json::Value>,
}

/// [`parse_turn_event`] for a caller that carries only the open tool block
/// between events.
#[cfg(test)]
pub(super) fn parse_sse_event(
    buffer: &mut String,
    open_tool_block: &mut Option<usize>,
) -> Option<Result<StreamChunk>> {
    let mut turn = Turn {
        open_tool_block: *open_tool_block,
        started: None,
    };
    let parsed = parse_turn_event(buffer, &mut turn);
    *open_tool_block = turn.open_tool_block;
    parsed
}

/// `closing`, the `usage` of a `message_delta`, laid over `started`, the
/// `usage` of the `message_start` before it: each field the closing event
/// carries with a value replaces the opening one's, and each it leaves out
/// or sends as `null` stands as it was.
fn overlaid(started: Option<&serde_json::Value>, closing: &serde_json::Value) -> serde_json::Value {
    let mut whole = started
        .and_then(serde_json::Value::as_object)
        .cloned()
        .unwrap_or_default();
    let said = closing.as_object().into_iter().flatten();
    for (field, value) in said.filter(|(_, value)| !value.is_null()) {
        whole.insert(field.clone(), value.clone());
    }
    serde_json::Value::Object(whole)
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

/// Parse a single SSE event from the buffer, consuming it if found. `turn`
/// is what the events before it left: the call being streamed now, and the
/// usage the message opened with.
///
/// `None` until a whole event has arrived, and for an event that carries
/// nothing the collector needs. `Some(Err(..))` for Anthropic's own `error`
/// event, which is the stream failing with the API's words: dropped, it left
/// the stream to end with no stop reason, and that read as a connection that
/// had died rather than as the overload or refusal it was.
pub(super) fn parse_turn_event(
    buffer: &mut String,
    turn: &mut Turn,
) -> Option<Result<StreamChunk>> {
    // `None` until a whole event has arrived; the caller polls again with
    // more bytes.
    let event = leviath_net::sse::next_event(buffer)?;
    if event.data.is_empty() {
        return None;
    }
    // The `event:` name, not a field of the payload, is what says an event
    // is the stream's error.
    let event_type = event.event.unwrap_or_default();

    let json: serde_json::Value = match serde_json::from_str(&event.data) {
        Ok(json) => json,
        Err(e) => {
            tracing::warn!(
                event = %event_type,
                error = %e,
                "an event from Anthropic's stream is not JSON; skipped"
            );
            return None;
        }
    };

    if event_type == "error" {
        let field = |name: &str| {
            json.pointer(&format!("/error/{name}"))
                .and_then(|v| v.as_str())
        };
        return Some(Err(stream_error(
            field("type").unwrap_or("error"),
            field("message").unwrap_or("(no message)"),
        )));
    }

    parse_event(&event_type, &json, turn).map(Ok)
}

/// The error an in-stream `error` event stands for.
///
/// Anthropic names the kind the way its HTTP statuses do, and the message is
/// written to read as that status: the retry loop classifies an error by its
/// text, so a streamed overload gets the same capacity-sized wait a buffered
/// 529 does, a server fault the blip-sized one, and anything else is the
/// request's own and permanent.
pub(super) fn stream_error(kind: &str, message: &str) -> ProviderError {
    match kind {
        "rate_limit_error" => ProviderError::RateLimitExceeded {
            retry_after_secs: None,
        },
        "overloaded_error" => status_error(529, FailureKind::ServerError, kind, message),
        "api_error" => status_error(500, FailureKind::ServerError, kind, message),
        other => status_error(400, FailureKind::BadRequest, other, message),
    }
}

/// An API error in the shape the HTTP path produces for `status`.
fn status_error(status: u16, failure: FailureKind, kind: &str, message: &str) -> ProviderError {
    ProviderError::ApiError(format!(
        "[{}] HTTP {status}: {kind}: {message} - {}",
        failure.label(),
        failure.remedy()
    ))
}

/// One parsed event as a chunk, or `None` for one that carries nothing the
/// collector needs. An event or a block this build does not read is said out
/// loud before it is skipped, so a missing answer has a line in the log.
fn parse_event(event_type: &str, json: &serde_json::Value, turn: &mut Turn) -> Option<StreamChunk> {
    match event_type {
        "content_block_delta" => {
            let delta = json.get("delta")?;
            match delta.get("type").and_then(|t| t.as_str()) {
                Some("text_delta") => {
                    let text = delta.get("text").and_then(|t| t.as_str()).unwrap_or("");
                    Some(StreamChunk {
                        reasoning_delta: None,
                        delta: text.to_string(),
                        tool_calls: Vec::new(),
                        tokens: None,
                        finish_reason: None,
                        reasoning: None,
                        parts: Vec::new(),
                    })
                }
                // The model's thinking, as it is written. The signature that
                // seals a block, and a redacted block's opaque data, are not
                // text and are skipped below with the other deltas.
                Some("thinking_delta") => {
                    let thinking = delta.get("thinking").and_then(|t| t.as_str()).unwrap_or("");
                    (!thinking.is_empty()).then(|| StreamChunk {
                        reasoning_delta: Some(thinking.to_string()),
                        delta: String::new(),
                        tool_calls: Vec::new(),
                        tokens: None,
                        finish_reason: None,
                        reasoning: None,
                        parts: Vec::new(),
                    })
                }
                Some("signature_delta") => None,
                Some("input_json_delta") => {
                    let partial = delta
                        .get("partial_json")
                        .and_then(|t| t.as_str())
                        .unwrap_or("");
                    Some(StreamChunk {
                        reasoning_delta: None,
                        delta: String::new(),
                        tool_calls: vec![ToolCallDelta {
                            // The block this delta names, or the one still
                            // open, and never a fresh number: an index of its
                            // own splits one call into an id with no arguments
                            // and arguments with no id.
                            index: event_block_index(json)
                                .or(turn.open_tool_block)
                                .unwrap_or(0),
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
                        reasoning: None,
                        parts: Vec::new(),
                    })
                }
                other => {
                    tracing::warn!(
                        delta_type = other.unwrap_or("(none)"),
                        "unrecognised content block delta from Anthropic; skipped"
                    );
                    None
                }
            }
        }
        "content_block_start" => {
            let content_block = json.get("content_block")?;
            match content_block.get("type").and_then(|t| t.as_str()) {
                Some("tool_use") => {
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
                    let idx = event_block_index(json)
                        .unwrap_or_else(|| turn.open_tool_block.map_or(0, |open| open + 1));
                    turn.open_tool_block = Some(idx);
                    Some(StreamChunk {
                        reasoning_delta: None,
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
                        reasoning: None,
                        parts: Vec::new(),
                    })
                }
                // A text block's words arrive in its deltas; the start says
                // nothing the collector needs.
                // Thinking arrives in its deltas the same way, and a redacted
                // block has no text to show.
                Some("text" | "thinking" | "redacted_thinking") => None,
                other => {
                    tracing::warn!(
                        block_type = other.unwrap_or("(none)"),
                        "unrecognised content block from Anthropic's stream; skipped"
                    );
                    None
                }
            }
        }
        "message_delta" => {
            let stop_reason = json
                .get("delta")
                .and_then(|d| d.get("stop_reason"))
                .and_then(|s| s.as_str())
                .unwrap_or("end_turn");

            // The last word on what the call used. Anthropic repeats the
            // input counts here once anything moved them after the message
            // began (a server tool reads more input), and then this event is
            // the running total: what it says laid over what the message
            // opened with, since it may repeat the input count and leave the
            // cache counts out. With the output count alone it is the slice
            // `message_start` left out.
            let usage = json.get("usage").unwrap_or(&serde_json::Value::Null);
            let repeats_input = usage.get("input_tokens").is_some_and(|n| !n.is_null());
            let tokens = match repeats_input {
                true => usage_of(&overlaid(turn.started.as_ref(), usage)).whole(),
                false => usage_of(usage),
            };

            Some(StreamChunk {
                reasoning_delta: None,
                delta: String::new(),
                tool_calls: Vec::new(),
                tokens: Some(tokens),
                finish_reason: Some(AnthropicProvider::parse_stop_reason(stop_reason)),
                reasoning: None,
                parts: Vec::new(),
            })
        }
        "message_start" => {
            // The input counts, and Anthropic's own id for the call, which
            // nothing can be looked up by but which says which call it was.
            let message = json.get("message").unwrap_or(&serde_json::Value::Null);
            if let Some(id) = message.get("id").and_then(|v| v.as_str()) {
                crate::call_trace::note_call_id(id);
            }
            let opened = message.get("usage").unwrap_or(&serde_json::Value::Null);
            turn.started = Some(opened.clone());
            let mut tokens = usage_of(opened);
            // The output count here is the few tokens written so far; the
            // call's own comes with `message_delta`.
            tokens = TokenUsage {
                terms: tokens.terms.clone(),
                ..TokenUsage::new(
                    tokens.prompt_tokens,
                    tokens.cached_tokens,
                    tokens.cache_write_tokens,
                    0,
                )
            };
            tokens.terms.model = message
                .get("model")
                .and_then(|v| v.as_str())
                .map(str::to_string);

            if tokens.total_tokens > 0 {
                Some(StreamChunk {
                    reasoning_delta: None,
                    delta: String::new(),
                    tool_calls: Vec::new(),
                    tokens: Some(tokens),
                    finish_reason: None,
                    reasoning: None,
                    parts: Vec::new(),
                })
            } else {
                None
            }
        }
        // The end of a block and of the message, and a keep-alive, carry
        // nothing the collector does not already have.
        "message_stop" | "content_block_stop" | "ping" => None,
        other => {
            tracing::warn!(
                event = other,
                "unrecognised event from Anthropic's stream; skipped"
            );
            None
        }
    }
}

/// A `usage` object as Anthropic writes it, on a reply or on a stream event.
///
/// `input_tokens` is already exclusive of both cache counts, so it is the
/// fresh figure [`TokenUsage`] wants. Beside the counts: how many of the
/// cache writes were made to last an hour (billed above the five-minute
/// write), the service tier the call was served on, and what the call was
/// billed on that token rates do not cover: a server tool it ran, a region
/// or a speed with a price of its own.
pub(crate) fn usage_of(usage: &serde_json::Value) -> TokenUsage {
    use crate::usage::count;
    let mut parsed = TokenUsage::new(
        count(usage, "/input_tokens"),
        count(usage, "/cache_read_input_tokens"),
        count(usage, "/cache_creation_input_tokens"),
        count(usage, "/output_tokens"),
    );
    parsed.terms.cache_write_1h_tokens = count(usage, "/cache_creation/ephemeral_1h_input_tokens");
    parsed.terms.service_tier = usage
        .get("service_tier")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    for (path, term) in [
        (
            "/server_tool_use/web_search_requests",
            "web_search_requests",
        ),
        ("/server_tool_use/web_fetch_requests", "web_fetch_requests"),
    ] {
        if count(usage, path) > 0 {
            parsed.terms.extra_terms.push(term.to_string());
        }
    }
    // A region or a speed other than the ordinary one multiplies every rate.
    for (key, ordinary) in [
        ("inference_geo", ["global", "not_available"]),
        ("speed", ["standard", "standard"]),
    ] {
        if let Some(value) = usage.get(key).and_then(|v| v.as_str())
            && !ordinary.contains(&value)
        {
            parsed.terms.extra_terms.push(format!("{key}:{value}"));
        }
    }
    parsed
}
