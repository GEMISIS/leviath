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

use std::time::Duration;

use serde_json::{Value, json};

use crate::media::{self, Poll, images};
use crate::provider::{InferenceRequest, InferenceResponse, ProviderError, Result};
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

/// The media models this build names, for `lev validate` and a listing that
/// cannot be read.
pub(crate) const CATALOG: &[(&str, &str)] = &[
    ("gpt-image-2", "GPT Image 2"),
    ("gpt-image-1.5", "GPT Image 1.5"),
    ("gpt-image-1", "GPT Image 1"),
    ("gpt-image-1-mini", "GPT Image 1 Mini"),
    ("chatgpt-image-latest", "ChatGPT Image"),
    ("sora-2", "Sora 2"),
    ("sora-2-pro", "Sora 2 Pro"),
    ("gpt-4o-mini-tts", "GPT-4o mini TTS"),
    ("tts-1", "TTS 1"),
    ("tts-1-hd", "TTS 1 HD"),
    ("whisper-1", "Whisper"),
    ("gpt-4o-transcribe", "GPT-4o Transcribe"),
    ("gpt-4o-mini-transcribe", "GPT-4o mini Transcribe"),
    ("gpt-4o-transcribe-diarize", "GPT-4o Transcribe Diarize"),
];

/// How a call is priced: the model's rates, when the shipped table has them.
pub(crate) struct Billing {
    /// The per-unit price (a second of video, a million characters, an hour
    /// of audio).
    pub(crate) unit: Option<crate::pricing::UnitPrice>,
    /// Token rates, for the models `usage` bills by the token.
    pub(crate) tokens: Option<crate::ModelPricing>,
}

/// Run the media model `request` names.
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
fn route(request: &InferenceRequest) -> String {
    format!("{}/{}", super::PROVIDER_NAME, request.model)
}

/// Make a video, wait for it, keep it, and delete the job.
async fn video(
    endpoint: &Endpoint,
    request: &InferenceRequest,
    billing: &Billing,
    poll_interval: Duration,
) -> Result<InferenceResponse> {
    let deadline = media::deadline(request);
    let prompt = media::request_text(request);
    let seconds = media::extra_i64(request, "seconds")
        .or_else(|| media::extra_i64(request, "duration"))
        .map(|s| s.to_string());
    let size = media::extra_str(request, "size");
    let reference = media::first_part(request, |m| m.starts_with("image/"));
    let url = endpoint.url("/videos");
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
    let id = created
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| ProviderError::InvalidResponse("the video request answered no id".into()))?
        .to_string();

    let status_url = endpoint.url(&format!("/videos/{id}"));
    let done = media::poll_until("the OpenAI video job", deadline, poll_interval, || {
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
    .await?;

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
        media::response(media::summary(&route(request), &parts), parts, cost)
    })
}

/// Speak the request's text.
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
