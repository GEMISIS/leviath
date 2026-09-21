//! A Gemini Interactions stream, event by event.
//!
//! An interaction arrives as server-sent events whose JSON names its own
//! `event_type`: `step.start` opens a step (model text, a thought, a function
//! call) at an `index`, `step.delta` carries its pieces, `step.stop` closes it
//! with the usage so far, and `interaction.completed` ends the turn with its
//! status. An `error` event is a failure delivered inside a 200.
//!
//! Text is passed on as it arrives. A function call's arguments may arrive in
//! pieces, as objects to merge rather than text to append, so a call is
//! assembled here and handed on whole when its step stops. A thought's
//! signature rides on the calls that follow it, which is where the request
//! builder hands it back.

use base64::Engine as _;
use serde_json::{Map, Value};

use crate::provider::{FinishReason, ProviderError, StreamChunk, TokenUsage, ToolCallDelta};
use futures_core::Stream;

/// A call being assembled.
#[derive(Default)]
struct Call {
    id: String,
    name: String,
    arguments: Map<String, Value>,
}

/// State carried across the events of one interaction.
#[derive(Default)]
pub(crate) struct Turn {
    /// Calls open by step index.
    calls: std::collections::HashMap<u64, Call>,
    /// How many calls were handed on, which numbers the next.
    finished_calls: usize,
    /// The last thought signature seen, for the calls after it.
    signature: Option<String>,
    /// The latest usage the stream reported.
    usage: Option<TokenUsage>,
    /// Speech a TTS model has streamed so far, handed on as one WAV when the
    /// interaction completes.
    pcm: Option<Pcm>,
    /// How many files the stream has handed on, which numbers their names.
    parts_made: usize,
}

/// Wrap a byte stream in the Interactions framer.
pub(crate) fn sse_stream<S>(inner: S) -> crate::provider::stream::FramedStream
where
    S: Stream<Item = std::result::Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
{
    let mut turn = Turn::default();
    crate::provider::stream::FramedStream::new(
        inner,
        Box::new(move |buffer: &mut String| parse_event(buffer, &mut turn)),
        None,
    )
}

/// A chunk carrying only `delta`.
fn text_chunk(delta: String) -> StreamChunk {
    StreamChunk {
        delta,
        tool_calls: Vec::new(),
        tokens: None,
        finish_reason: None,
        reasoning: None,
        parts: Vec::new(),
    }
}

/// Parse one event, consuming it from the buffer. `None` is "nothing to hand
/// on yet"; `Some(None)` would end the stream, which only the transport does.
pub(crate) fn parse_event(
    buffer: &mut String,
    turn: &mut Turn,
) -> Option<Option<crate::provider::Result<StreamChunk>>> {
    let (event_text, rest) = buffer.split_once("\n\n")?;
    let event_text = event_text.to_string();
    *buffer = rest.to_string();
    let data: String = event_text
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim_start)
        .collect();
    let json: Value = serde_json::from_str(&data).ok()?;
    let index = json.get("index").and_then(Value::as_u64).unwrap_or(0);

    match json.get("event_type").and_then(Value::as_str)? {
        "step.start" => {
            let step = json.get("step")?;
            match step.get("type").and_then(Value::as_str)? {
                "function_call" => {
                    let call = turn.calls.entry(index).or_default();
                    call.id = str_of(step, "id");
                    call.name = str_of(step, "name");
                    merge_arguments(call, step.get("arguments"));
                    None
                }
                "thought" => {
                    remember_signature(turn, step);
                    None
                }
                _ => content_chunk(step.get("content"), turn),
            }
        }
        "step.delta" => {
            let delta = json.get("delta")?;
            record_usage(turn, json.pointer("/metadata/total_usage"));
            match delta.get("type").and_then(Value::as_str)? {
                "text" => Some(Some(Ok(text_chunk(str_of(delta, "text"))))),
                "thought" | "thought_signature" => {
                    remember_signature(turn, delta);
                    None
                }
                "function_call" => {
                    let call = turn.calls.entry(index).or_default();
                    if let Some(id) = delta.get("id").and_then(Value::as_str) {
                        call.id = id.to_string();
                    }
                    if let Some(name) = delta.get("name").and_then(Value::as_str) {
                        call.name = name.to_string();
                    }
                    merge_arguments(call, delta.get("arguments"));
                    None
                }
                _ => content_chunk(Some(&Value::Array(vec![delta.clone()])), turn),
            }
        }
        "step.stop" => {
            record_usage(turn, json.get("usage"));
            let call = turn.calls.remove(&index)?;
            // The API names its calls; a reply that arrives without one still
            // has to be answerable, and an empty id is the one value that
            // cannot be.
            let id = match call.id.is_empty() {
                true => crate::call_ids::mint("gemini_call"),
                false => call.id,
            };
            let delta = ToolCallDelta {
                index: turn.finished_calls,
                id: Some(id),
                name: Some(call.name),
                arguments_delta: Value::Object(call.arguments).to_string(),
                thought_signature: turn.signature.clone(),
            };
            turn.finished_calls += 1;
            Some(Some(Ok(StreamChunk {
                tool_calls: vec![delta],
                ..text_chunk(String::new())
            })))
        }
        "interaction.completed" => {
            let interaction = json.get("interaction");
            record_usage(turn, interaction.and_then(|i| i.get("usage")));
            let status = interaction
                .and_then(|i| i.get("status"))
                .and_then(Value::as_str)
                .unwrap_or("completed");
            let finish_reason = match status {
                "completed" | "requires_action" => match turn.finished_calls {
                    0 => FinishReason::Complete,
                    _ => FinishReason::ToolCall,
                },
                "incomplete" | "budget_exceeded" => FinishReason::TokenLimit,
                "failed" | "cancelled" => {
                    return Some(Some(Err(ProviderError::ApiError(format!(
                        "the interaction ended {status}: {}",
                        interaction.map(Value::to_string).unwrap_or_default()
                    )))));
                }
                _ => FinishReason::Unknown,
            };
            Some(Some(Ok(StreamChunk {
                tokens: Some(
                    turn.usage
                        .clone()
                        .unwrap_or_else(|| TokenUsage::new(0, 0, 0, 0)),
                ),
                finish_reason: Some(finish_reason),
                parts: turn.pcm.take().map(Pcm::into_wav).into_iter().collect(),
                ..text_chunk(String::new())
            })))
        }
        "error" => {
            let message = json
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("the stream reported an error");
            Some(Some(Err(ProviderError::ApiError(message.to_string()))))
        }
        _ => None,
    }
}

/// `value[key]` as a string, or empty.
fn str_of(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Keep a thought's signature for the calls that follow it.
fn remember_signature(turn: &mut Turn, thought: &Value) {
    if let Some(signature) = thought.get("signature").and_then(Value::as_str)
        && !signature.is_empty()
    {
        turn.signature = Some(signature.to_string());
    }
}

/// Merge a piece of a call's arguments: an object's keys join the call's, a
/// string is parsed as one.
fn merge_arguments(call: &mut Call, arguments: Option<&Value>) {
    let parsed = match arguments {
        Some(Value::String(text)) => serde_json::from_str::<Value>(text).ok(),
        other => other.cloned(),
    };
    if let Some(Value::Object(fields)) = parsed {
        call.arguments.extend(fields);
    }
}

/// Usage as the route reports it: input includes the cached tokens, and the
/// thinking the model did is billed as output.
fn record_usage(turn: &mut Turn, usage: Option<&Value>) {
    let Some(usage) = usage else {
        return;
    };
    let count = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0) as usize;
    let cached = count("total_cached_tokens");
    turn.usage = Some(TokenUsage::new(
        count("total_input_tokens").saturating_sub(cached),
        cached,
        0,
        count("total_output_tokens") + count("total_thought_tokens"),
    ));
}

/// A step's content items as a chunk: text joined, images and audio as parts.
/// Raw PCM is held in `turn` instead, to be handed on whole at the end.
fn content_chunk(
    content: Option<&Value>,
    turn: &mut Turn,
) -> Option<Option<crate::provider::Result<StreamChunk>>> {
    let mut chunk = text_chunk(String::new());
    for item in content.and_then(Value::as_array).into_iter().flatten() {
        match item.get("type").and_then(Value::as_str) {
            Some("text") => chunk.delta.push_str(&str_of(item, "text")),
            Some(_) => {
                if let Some(blob) = blob_of(item, turn) {
                    chunk.parts.push(blob);
                }
            }
            None => {}
        }
    }
    (!chunk.delta.is_empty() || !chunk.parts.is_empty()).then_some(Some(Ok(chunk)))
}

/// An inline media item the model produced, as a blob. Raw PCM
/// (`audio/L16`) comes in many deltas and is no file on its own, so it is
/// added to the turn's audio and yields nothing here.
fn blob_of(item: &Value, turn: &mut Turn) -> Option<leviath_core::mime::Blob> {
    let data = item.get("data").and_then(Value::as_str)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .ok()?;
    let mime = item.get("mime_type").and_then(Value::as_str)?;
    if mime.to_ascii_lowercase().starts_with("audio/l16") {
        let rate = item
            .get("sample_rate")
            .and_then(Value::as_u64)
            .or_else(|| {
                mime.split(';')
                    .find_map(|p| p.trim().strip_prefix("rate="))
                    .and_then(|r| r.parse().ok())
            })
            .unwrap_or(24_000);
        let channels = item.get("channels").and_then(Value::as_u64).unwrap_or(1);
        let audio = turn.pcm.get_or_insert_with(|| Pcm {
            rate: rate as u32,
            channels: channels as u16,
            samples: Vec::new(),
        });
        audio.samples.extend(bytes);
        return None;
    }
    let mime_type = leviath_core::mime::MimeType::parse(mime).ok()?;
    // Named as the other media routes name theirs (`image-1.jpg`), so a saved
    // result has a name and an extension to open it by.
    turn.parts_made += 1;
    let stem = mime.split('/').next().unwrap_or("part");
    let name = format!(
        "{stem}-{}.{}",
        turn.parts_made,
        crate::media::extension(mime_type.as_str())
    );
    Some(leviath_core::mime::Blob::new(mime_type, bytes).named(&name))
}

/// Raw 16-bit little-endian PCM a speech model streamed, joined.
pub(crate) struct Pcm {
    /// Samples a second.
    rate: u32,
    /// Channels, interleaved.
    channels: u16,
    /// The samples.
    samples: Vec<u8>,
}

impl Pcm {
    /// The audio as a WAV file: a 44-byte header in front of the samples.
    fn into_wav(self) -> leviath_core::mime::Blob {
        let block_align = self.channels * 2;
        let byte_rate = self.rate * u32::from(block_align);
        let data_len = self.samples.len() as u32;
        let mut wav = Vec::with_capacity(44 + self.samples.len());
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&self.channels.to_le_bytes());
        wav.extend_from_slice(&self.rate.to_le_bytes());
        wav.extend_from_slice(&byte_rate.to_le_bytes());
        wav.extend_from_slice(&block_align.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        wav.extend(self.samples);
        leviath_core::mime::Blob::new(
            leviath_core::mime::MimeType::parse("audio/wav").expect("audio/wav is a mime type"),
            wav,
        )
        .named("speech.wav")
    }
}

#[cfg(test)]
#[path = "stream_tests.rs"]
mod tests;
