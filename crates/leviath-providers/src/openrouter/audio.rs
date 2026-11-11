//! Audio answers through the gateway: whether a request asks for one, the
//! format and voice it is asked for in, and the streamed audio reassembled
//! into one part.
//!
//! OpenRouter returns generated audio only on a streamed request that opts
//! into it, so both the request body and the stream reader turn on these.

use crate::provider::{InferenceRequest, Result, StreamChunk};

/// Whether the stage declares audio among its outputs, which is the only
/// thing that makes the gateway answer in audio.
pub(super) fn request_wants_audio(request: &InferenceRequest) -> bool {
    request
        .extra
        .get(crate::provider::OUTPUT_MIMES_KEY)
        .and_then(serde_json::Value::as_array)
        .is_some_and(|mimes| {
            mimes
                .iter()
                .filter_map(serde_json::Value::as_str)
                .any(|mime| mime.starts_with("audio/"))
        })
}

pub(super) fn request_audio_mime(request: &InferenceRequest) -> Option<&str> {
    request
        .extra
        .get(crate::provider::OUTPUT_MIMES_KEY)
        .and_then(serde_json::Value::as_array)?
        .iter()
        .filter_map(serde_json::Value::as_str)
        .find(|mime| mime.starts_with("audio/") && !mime.ends_with("/*"))
}

/// The voice the stage's `voice` parameter names, or the default every
/// route to a speaking model uses.
pub(super) fn request_audio_voice(request: &InferenceRequest) -> String {
    request
        .extra
        .get("voice")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|voice| !voice.is_empty())
        .unwrap_or(crate::provider::DEFAULT_AUDIO_VOICE)
        .to_string()
}

pub(super) fn request_audio_format(request: &InferenceRequest) -> String {
    let Some(mime) = request_audio_mime(request) else {
        // OpenRouter's audio-output API needs a format. A wildcard stage has
        // no preferred container, so ask for WAV and use the returned type if
        // the gateway reports something more specific.
        return "wav".to_string();
    };

    let extension = leviath_core::mime::MimeType::parse(mime)
        .ok()
        .and_then(|mime_type| {
            leviath_core::mime::MimeRegistry::builtin()
                .info(&mime_type)
                .extensions
                .first()
                .cloned()
        });
    extension.unwrap_or_else(|| mime.strip_prefix("audio/").unwrap_or("wav").to_string())
}

#[derive(Default)]
pub(super) struct AudioStreamAccumulator {
    requested_mime: Option<String>,
    returned_mime: Option<String>,
    format: Option<String>,
    chunks: Vec<String>,
    emitted: bool,
    /// Whether an event said the answer had finished. An OpenAI audio answer
    /// never does: its stream is the audio, the usage, and `[DONE]`.
    finished: bool,
}

impl AudioStreamAccumulator {
    pub(super) fn new(requested_mime: Option<&str>, format: &str) -> Self {
        Self {
            requested_mime: requested_mime.map(str::to_string),
            format: Some(format.to_string()),
            ..Self::default()
        }
    }

    /// Keep what `event` carries of the audio, and answer the piece of its
    /// transcript the event holds: the words being spoken, which an OpenAI
    /// model sends there and not as content.
    fn observe(&mut self, event: &serde_json::Value) -> Option<String> {
        let audio = event.pointer("/choices/0/delta/audio")?;
        if let Some(data) = audio.get("data").and_then(serde_json::Value::as_str) {
            self.chunks.push(data.to_string());
        }
        if let Some(mime) = audio
            .get("mime_type")
            .or_else(|| audio.get("content_type"))
            .and_then(serde_json::Value::as_str)
        {
            self.returned_mime = Some(mime.to_string());
        }
        if let Some(format) = audio.get("format").and_then(serde_json::Value::as_str) {
            self.format = Some(format.to_string());
        }
        audio
            .get("transcript")
            .and_then(serde_json::Value::as_str)
            .filter(|words| !words.is_empty())
            .map(str::to_string)
    }

    fn take_blob(&mut self) -> Option<leviath_core::mime::Blob> {
        if self.emitted {
            return None;
        }
        let blob = crate::mime_output::streamed_audio_blob(
            &self.chunks,
            self.requested_mime.as_deref(),
            self.returned_mime.as_deref(),
            self.format.as_deref(),
        );
        self.emitted = blob.is_some();
        blob
    }
}

/// One event of an audio answer's stream. The reasoning the model wrote is
/// kept for replay the same way a text stream's is, since an audio answer can
/// call a tool too: it goes out with whatever audio is still held, at the end.
pub(super) fn openrouter_audio_sse_event(
    buffer: &mut String,
    audio: &mut AudioStreamAccumulator,
    kept: &crate::openai_compat::details::SharedDetails,
    asked: &str,
) -> Option<Option<Result<StreamChunk>>> {
    if leviath_core::sync::lock(kept).is_sealed() {
        return Some(None);
    }
    let event = leviath_net::sse::next_event(buffer)?;
    let data = event.data.trim();
    if data == "[DONE]" {
        let last = crate::openai_compat::sealed_chunk(kept, asked);
        let blob = audio.take_blob();
        // Audio that reached `[DONE]` is a whole answer, said or not.
        let closing = (blob.is_some() && !std::mem::replace(&mut audio.finished, true))
            .then_some(crate::provider::FinishReason::Complete);
        return Some(match (last, blob, closing) {
            (None, None, None) => None,
            (last, blob, closing) => {
                let last = last.unwrap_or_else(|| text_chunk(String::new()));
                Some(Ok(StreamChunk {
                    parts: blob.into_iter().collect(),
                    finish_reason: last.finish_reason.clone().or(closing),
                    ..last
                }))
            }
        });
    }

    let json = serde_json::from_str::<serde_json::Value>(data).ok()?;
    // The audio is kept before the event is read as chat: an event that
    // carries audio and nothing a chat reader hands on is still audio.
    let words = audio.observe(&json);
    let mut framed = format!("data: {data}\n\n");
    let mut parsed = crate::openai_compat::parse_sse_event_keeping(&mut framed, kept, asked)?;
    if let Some(Ok(chunk)) = &mut parsed {
        if chunk.delta.is_empty()
            && let Some(words) = words
        {
            chunk.delta = words;
        }
        if chunk.finish_reason.is_some() {
            audio.finished = true;
            chunk.parts.extend(audio.take_blob());
        }
    }
    Some(parsed)
}

/// A chunk carrying only `delta`.
fn text_chunk(delta: String) -> StreamChunk {
    StreamChunk {
        reasoning_delta: None,
        delta,
        tool_calls: Vec::new(),
        tokens: None,
        finish_reason: None,
        reasoning: None,
        parts: Vec::new(),
    }
}
