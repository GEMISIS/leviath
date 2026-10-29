//! OpenAI's media models, each on its own route rather than the Responses API.
//!
//! Measured against the live API (2026-09-17):
//!
//! - `gpt-image-*`, `chatgpt-image-*`: `POST /images/generations`, or
//!   `/images/edits` with `images: [{image_url}]` (a data URL) when the stage
//!   hands the model an image. The reply is always base64 and refuses
//!   `response_format`; it names its format once (`output_format`) and counts
//!   the tokens it is billed by under `usage`.
//! - `sora-*`: `POST /videos` answers a video `id` with `status: queued`;
//!   `GET /videos/{id}` reports `queued`, `in_progress`, `completed` or
//!   `failed`, and `GET /videos/{id}/content` is the MP4, behind the key. A
//!   starting image goes as the multipart `input_reference`. The job is deleted
//!   once the file is kept, so nothing stays on OpenAI's side.
//! - `tts-*`, `gpt-4o-mini-tts`: `POST /audio/speech` answers the audio bytes,
//!   MP3 unless `response_format` says otherwise. `voice` is required.
//! - `whisper-*`, `*-transcribe*`: `POST /audio/transcriptions`, multipart. The
//!   reply's `usage` is either seconds of audio (`whisper-1`) or tokens.

#[cfg(feature = "openai")]
use std::time::Duration;

#[cfg(feature = "openai")]
use serde_json::{Value, json};

#[cfg(feature = "openai")]
use crate::media::{self, Poll, images};
#[cfg(feature = "openai")]
use crate::provider::{InferenceRequest, InferenceResponse, ProviderError, Result};
#[cfg(feature = "openai")]
use crate::responses::client::Endpoint;

/// What a media model does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Make or edit images.
    Image,
    /// Make a video.
    Video,
    /// Speak text.
    Speech,
    /// Transcribe audio.
    Transcribe,
}

/// The media kind `model` is, or `None` for a chat model or one this build
/// does not run: the realtime and live models speak a websocket, and
/// `gpt-audio*` is a chat model that also hears and speaks.
pub(crate) fn kind(model: &str) -> Option<Kind> {
    let m = model.to_ascii_lowercase();
    if m.contains("realtime") || m.contains("live") {
        return None;
    }
    match m.as_str() {
        m if m.starts_with("gpt-image") || m.starts_with("chatgpt-image") => Some(Kind::Image),
        m if m.starts_with("sora") => Some(Kind::Video),
        m if m.starts_with("tts-") || m.contains("-tts") => Some(Kind::Speech),
        m if m.starts_with("whisper") || m.contains("transcribe") => Some(Kind::Transcribe),
        _ => None,
    }
}

/// Whether `request` is a `gpt-audio*` chat call whose stage declares audio as
/// its output. Those models answer in speech only on Chat Completions (the
/// Responses API has no audio output), and only when asked for it; a stage that
/// declares no audio output keeps the ordinary text route.
#[cfg(feature = "openai")]
pub(crate) fn speaks_in_chat(request: &InferenceRequest) -> bool {
    let model = request.model.to_ascii_lowercase();
    model.starts_with("gpt-audio")
        && !model.contains("realtime")
        && request
            .extra
            .get(crate::provider::OUTPUT_MIMES_KEY)
            .and_then(Value::as_array)
            .is_some_and(|mimes| {
                mimes
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|m| m.starts_with("audio/"))
            })
}

/// The media models this build names, for `lev validate` and a listing that
/// cannot be read.
///
/// Only models still in service. `sora-2` and `sora-2-pro` left on their
/// `shutdown_date`, 2026-09-24: OpenAI's listing still names them and every
/// call 404s, so a row here would offer a route that cannot answer to a
/// machine that has not read the listing. The `sora` shape is still routed
/// as video (see [`kind`]), for whatever OpenAI lists next.
pub(crate) const CATALOG: &[(&str, &str)] = &[
    ("gpt-image-2", "GPT Image 2"),
    ("gpt-image-1.5", "GPT Image 1.5"),
    ("gpt-image-1", "GPT Image 1"),
    ("gpt-image-1-mini", "GPT Image 1 Mini"),
    ("chatgpt-image-latest", "ChatGPT Image"),
    ("gpt-4o-mini-tts", "GPT-4o mini TTS"),
    ("tts-1", "TTS 1"),
    ("tts-1-hd", "TTS 1 HD"),
    ("whisper-1", "Whisper"),
    ("gpt-4o-transcribe", "GPT-4o Transcribe"),
    ("gpt-4o-mini-transcribe", "GPT-4o mini Transcribe"),
    ("gpt-4o-transcribe-diarize", "GPT-4o Transcribe Diarize"),
];

/// How a call is priced: the model's rates, when the shipped table has them.
#[cfg(feature = "openai")]
pub(crate) struct Billing {
    /// The per-unit price (a second of video, a million characters, an hour
    /// of audio).
    pub(crate) unit: Option<crate::pricing::UnitPrice>,
    /// Token rates, for the models `usage` bills by the token.
    pub(crate) tokens: Option<crate::ModelPricing>,
}

/// Run the media model `request` names.
#[cfg(feature = "openai")]
pub(crate) async fn run(
    endpoint: &Endpoint,
    kind: Kind,
    request: &InferenceRequest,
    billing: &Billing,
    poll_interval: Duration,
) -> Result<InferenceResponse> {
    match kind {
        Kind::Image => {
            images::run(
                endpoint,
                &images::Route {
                    provider: super::PROVIDER_NAME,
                    shape: images::EditShape::Meta,
                    default_mime: "image/png",
                    hints: &[
                        "size",
                        "quality",
                        "background",
                        "output_format",
                        "moderation",
                    ],
                    reported_cost: false,
                    unit: None,
                    tokens: billing.tokens,
                    response_format: false,
                },
                request,
            )
            .await
        }
        Kind::Video => video(endpoint, request, billing, poll_interval).await,
        Kind::Speech => speech(endpoint, request, billing).await,
        Kind::Transcribe => transcribe(endpoint, request, billing).await,
    }
}

/// The OpenAI route and model, for the summary line.
#[cfg(feature = "openai")]
fn route(request: &InferenceRequest) -> String {
    format!("{}/{}", super::PROVIDER_NAME, request.model)
}

/// The audio formats Chat Completions returns, with the type each is.
#[cfg(feature = "openai")]
fn chat_audio_type(format: &str) -> Option<&'static str> {
    match format {
        "mp3" => Some("audio/mpeg"),
        "wav" => Some("audio/wav"),
        "flac" => Some("audio/flac"),
        "opus" => Some("audio/ogg"),
        "pcm16" => Some("audio/l16"),
        _ => None,
    }
}

/// Ask a `gpt-audio*` model for a spoken answer over Chat Completions:
/// `modalities: ["text", "audio"]` and `audio: {voice, format}`. The audio comes
/// back base64 under the message's `audio.data`, with its transcript, and
/// becomes an output part; the transcript is the reply's text.
///
/// The `voice` (default `alloy`) and `audio_format` (`mp3`, `wav`, `flac`,
/// `opus` or `pcm16`; default `mp3`) come from the stage's model parameters.
/// Not streamed, and not priced per call: the reply's token usage is all it
/// reports.
#[cfg(feature = "openai")]
pub(crate) async fn chat_audio(
    endpoint: &Endpoint,
    request: &InferenceRequest,
    send_temperature: bool,
) -> Result<InferenceResponse> {
    use base64::Engine as _;
    let format = media::extra_str(request, "audio_format").unwrap_or_else(|| "mp3".to_string());
    let mime = chat_audio_type(&format).ok_or_else(|| {
        ProviderError::InvalidResponse(format!(
            "audio_format '{format}' is not one {} returns: use mp3, wav, flac, opus or pcm16",
            request.model
        ))
    })?;
    let voice = media::extra_str(request, "voice")
        .unwrap_or_else(|| crate::provider::DEFAULT_AUDIO_VOICE.to_string());
    let mut body = crate::openai_compat::build_openai_request_body(request);
    let fields = body
        .as_object_mut()
        .expect("a Chat Completions body is a JSON object");
    // These models take the cap under its newer name, and the two stage
    // parameters above are not Chat Completions fields.
    fields.remove("max_tokens");
    fields.insert("max_completion_tokens".into(), json!(request.max_tokens));
    fields.remove("voice");
    fields.remove("audio_format");
    // A model that takes no temperature is sent none, as on the ordinary route.
    if !send_temperature {
        fields.remove("temperature");
    }
    fields.insert("modalities".into(), json!(["text", "audio"]));
    fields.insert("audio".into(), json!({ "voice": voice, "format": format }));
    let url = endpoint.url("/chat/completions");
    let timeout = request.request_timeout_secs;
    let response = endpoint
        .send(|client| {
            crate::provider::apply_request_timeout(client.post(&url).json(&body), timeout)
        })
        .await?;
    let response =
        crate::provider::check_http_response(response, endpoint.rate_limiter.as_ref()).await?;
    let reply: Value = response
        .json()
        .await
        .map_err(|e| ProviderError::InvalidResponse(format!("unreadable audio reply: {e}")))?;
    let mut parsed = crate::openai_compat::parse_openai_response(&reply)?;
    let message = reply.pointer("/choices/0/message");
    let audio = message.and_then(|m| m.get("audio"));
    let data = audio.and_then(|a| a.get("data")).and_then(Value::as_str);
    let Some(data) = data else {
        // A turn can answer without speaking: it called a tool, or declined.
        // That is the model's reply, and the stage decides what it is worth.
        let refusal = message
            .and_then(|m| m.get("refusal"))
            .and_then(Value::as_str)
            .filter(|r| !r.trim().is_empty());
        if parsed.content.trim().is_empty()
            && let Some(refusal) = refusal
        {
            parsed.content = refusal.to_string();
        }
        if !parsed.tool_calls.is_empty() || !parsed.content.trim().is_empty() {
            return Ok(parsed);
        }
        return Err(ProviderError::InvalidResponse(format!(
            "{} answered without audio although the stage asked for it",
            request.model
        )));
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|e| ProviderError::InvalidResponse(format!("undecodable audio reply: {e}")))?;
    let name = format!("speech.{}", media::extension(mime));
    parsed.parts.push(
        leviath_core::mime::Blob::new(
            leviath_core::mime::MimeType::parse(mime).expect("a fixed audio type is a mime type"),
            bytes,
        )
        .named(&name),
    );
    if parsed.content.trim().is_empty() {
        parsed.content = audio
            .and_then(|a| a.get("transcript"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| media::summary(&route(request), &parsed.parts));
    }
    Ok(parsed)
}

/// Make a video, wait for it, keep it, and delete the job.
#[cfg(feature = "openai")]
async fn video(
    endpoint: &Endpoint,
    request: &InferenceRequest,
    billing: &Billing,
    poll_interval: Duration,
) -> Result<InferenceResponse> {
    let deadline = media::deadline(request);
    let prompt = media::request_text(request);
    // Both are billed: a length or a size that does not read is refused
    // here rather than left out for Sora to answer at its default.
    let seconds = media::extra_seconds(request, &["seconds", "duration"])?.map(|s| s.to_string());
    let size = video_size(request)?;
    let reference = media::first_part(request, |m| m.starts_with("image/"));
    let url = endpoint.url("/videos");
    let submit = || async {
        let created = endpoint
            .send(|client| {
                let mut form = reqwest::multipart::Form::new()
                    .text("model", request.model.clone())
                    .text("prompt", prompt.clone());
                if let Some(seconds) = &seconds {
                    form = form.text("seconds", seconds.clone());
                }
                if let Some(size) = &size {
                    form = form.text("size", size.clone());
                }
                if let Some(image) = &reference {
                    let part = reqwest::multipart::Part::bytes(image.bytes.clone())
                        .file_name(image.name.clone().unwrap_or_else(|| "reference".into()))
                        .mime_str(image.mime_type.as_str())
                        .expect("a stored part's type is a valid mime type");
                    form = form.part("input_reference", part);
                }
                client.post(&url).multipart(form)
            })
            .await?;
        let created =
            crate::provider::check_http_response(created, endpoint.rate_limiter.as_ref()).await?;
        let created: Value = crate::provider::decode_json(created).await?;
        created
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| {
                ProviderError::InvalidResponse("the video request answered no id".into())
            })
    };
    let wait = |id: String| {
        let status_url = endpoint.url(&format!("/videos/{id}"));
        media::poll_until("the OpenAI video job", deadline, poll_interval, move || {
            let url = status_url.clone();
            async move {
                let job = endpoint.get_json(&url).await?;
                Ok(match job.get("status").and_then(Value::as_str) {
                    Some("completed") => Poll::Done(job),
                    Some("failed") => Poll::Failed(
                        job.pointer("/error/message")
                            .and_then(Value::as_str)
                            .unwrap_or("no reason given")
                            .to_string(),
                    ),
                    _ => Poll::Running,
                })
            }
        })
    };
    let step = format!("{}/{}/video", super::PROVIDER_NAME, request.model);
    let crate::jobs::Ran {
        id,
        value: done,
        note,
    } = crate::jobs::submit_or_resume(&step, submit, wait).await?;

    let content_url = endpoint.url(&format!("/videos/{id}/content"));
    let response = endpoint.send(|client| client.get(&content_url)).await?;
    let response =
        crate::provider::check_http_response(response, endpoint.rate_limiter.as_ref()).await?;
    let bytes = media::body_bytes(response).await;
    // The job holding a copy at OpenAI is not needed once the file is read,
    // or could not be. A failed delete leaves it to expire, which OpenAI does
    // on its own.
    let delete_url = endpoint.url(&format!("/videos/{id}"));
    if let Err(e) = endpoint.send(|client| client.delete(&delete_url)).await {
        tracing::debug!(error = %e, "a finished OpenAI video job could not be deleted");
    }
    bytes.map(|bytes| {
        let parts = vec![media::typed_blob("video/mp4", bytes, "video.mp4")];
        let length = done
            .get("seconds")
            .and_then(|s| {
                s.as_str()
                    .and_then(|s| s.parse().ok())
                    .or_else(|| s.as_f64())
            })
            .unwrap_or(0.0);
        let cost = billing.unit.map(|u| u.cost(length));
        let summary = crate::jobs::noted(media::summary(&route(request), &parts), [note]);
        media::response(summary, parts, cost)
    })
}

/// The stage's `size` for a video, `WIDTHxHEIGHT` (`1280x720`), or the
/// refusal naming what it takes. Which sizes a model makes is Sora's to say,
/// and it refuses one it does not make; a value that is not a size at all is
/// refused here.
#[cfg(feature = "openai")]
fn video_size(request: &InferenceRequest) -> Result<Option<String>> {
    let Some(value) = request.extra.get("size") else {
        return Ok(None);
    };
    let size = value.as_str().map(str::trim).filter(|text| {
        text.split_once('x').is_some_and(|(w, h)| {
            [w, h]
                .iter()
                .all(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        })
    });
    size.map(|s| Some(s.to_string())).ok_or_else(|| {
        ProviderError::Other(format!(
            "`size` must be WIDTHxHEIGHT, such as \"1280x720\", not {value}"
        ))
    })
}

/// Speak the request's text.
#[cfg(feature = "openai")]
async fn speech(
    endpoint: &Endpoint,
    request: &InferenceRequest,
    billing: &Billing,
) -> Result<InferenceResponse> {
    let text = media::request_text(request);
    if text.is_empty() {
        return Err(ProviderError::InvalidResponse(format!(
            "{} needs text to speak: the stage handed it none",
            request.model
        )));
    }
    let mut body = serde_json::Map::new();
    body.insert("model".into(), json!(request.model));
    body.insert("input".into(), json!(text));
    body.insert(
        "voice".into(),
        json!(media::extra_str(request, "voice").unwrap_or_else(|| "alloy".to_string())),
    );
    for key in ["response_format", "instructions"] {
        if let Some(value) = media::extra_str(request, key) {
            body.insert(key.into(), json!(value));
        }
    }
    if let Some(speed) = media::extra_f64(request, "speed") {
        body.insert("speed".into(), json!(speed));
    }
    let response = endpoint
        .post_json("/audio/speech", &Value::Object(body))
        .await?;
    let response =
        crate::provider::check_http_response(response, endpoint.rate_limiter.as_ref()).await?;
    let mime = media::audio_type(&response);
    let name = format!("speech.{}", media::extension(mime.as_str()));
    let parts =
        vec![leviath_core::mime::Blob::new(mime, media::body_bytes(response).await?).named(&name)];
    // `tts-1` bills the characters sent; `gpt-4o-mini-tts` bills the seconds
    // of audio made, which the reply does not count, so they are read from
    // the file. A file whose length cannot be read leaves the call unpriced.
    let cost = billing.unit.and_then(|unit| match unit.unit {
        crate::pricing::PriceUnit::AudioHour => {
            media::audio_seconds(&parts[0]).map(|seconds| unit.cost(seconds / 3600.0))
        }
        _ => Some(unit.cost(text.chars().count() as f64 / 1_000_000.0)),
    });
    Ok(media::response(
        media::summary(&route(request), &parts),
        parts,
        cost,
    ))
}

/// Transcribe the request's audio.
#[cfg(feature = "openai")]
async fn transcribe(
    endpoint: &Endpoint,
    request: &InferenceRequest,
    billing: &Billing,
) -> Result<InferenceResponse> {
    let audio = media::first_part(request, |m| m.starts_with("audio/")).ok_or_else(|| {
        ProviderError::InvalidResponse(format!(
            "{} needs an audio part to transcribe: the stage handed it none",
            request.model
        ))
    })?;
    let url = endpoint.url("/audio/transcriptions");
    let name = audio.name.clone().unwrap_or_else(|| "audio".to_string());
    let format = media::extra_str(request, "response_format").unwrap_or_else(|| {
        match request.model.as_str() {
            "whisper-1" => "verbose_json",
            m if m.ends_with("diarize") => "diarized_json",
            _ => "json",
        }
        .to_string()
    });
    let language = media::extra_str(request, "language");
    let prompt = media::extra_str(request, "prompt");
    let response = endpoint
        .send(|client| {
            let file = reqwest::multipart::Part::bytes(audio.bytes.clone())
                .file_name(name.clone())
                .mime_str(audio.mime_type.as_str())
                .expect("a stored part's type is a valid mime type");
            // The settings ahead of the file, so a reader that stops at the
            // file has them all.
            let mut form = reqwest::multipart::Form::new()
                .text("model", request.model.clone())
                .text("response_format", format.clone());
            if format == "diarized_json" {
                form = form.text("chunking_strategy", "auto");
            }
            if let Some(language) = &language {
                form = form.text("language", language.clone());
            }
            if let Some(prompt) = &prompt {
                form = form.text("prompt", prompt.clone());
            }
            client.post(&url).multipart(form.part("file", file))
        })
        .await?;
    let response =
        crate::provider::check_http_response(response, endpoint.rate_limiter.as_ref()).await?;
    let reply: Value = crate::provider::decode_json(response).await?;
    let text = reply
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let cost = match reply.pointer("/usage/type").and_then(Value::as_str) {
        Some("tokens") => images::token_cost(billing.tokens.as_ref(), &reply),
        _ => {
            let seconds = reply
                .pointer("/usage/seconds")
                .or_else(|| reply.get("duration"))
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            billing.unit.map(|u| u.cost(seconds / 3600.0))
        }
    };
    let transcript = serde_json::to_vec_pretty(&reply).expect("a JSON value serialises");
    let parts = vec![media::json_blob(transcript, "transcript.json")];
    Ok(media::response(text, parts, cost))
}

#[cfg(test)]
mod tests;
