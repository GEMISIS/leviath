//! xAI's media models: Imagine images and video, and the speech routes.
//!
//! - `grok-imagine-image*`: the shared images routes ([`crate::media::images`]).
//! - `grok-imagine-video*`: asynchronous. `POST /videos/generations` (or
//!   `/edits` when the stage handed it a video, `/extensions` when
//!   `operation = "extend"`) answers a `request_id`, and `GET
//!   /videos/{request_id}` reports `pending`, `done` or `failed`; the finished
//!   task carries the video's URL and duration.
//! - `grok-tts`: `POST /tts` with the text, answered with the audio bytes. The
//!   route takes no model field, so the id is Leviath's name for it.
//! - `grok-stt`: `POST /stt` as a multipart upload of the audio, answered with
//!   the transcript and word timings. Also a Leviath name.

use std::time::Duration;

use serde_json::{Value, json};

use crate::media::{self, Poll, images};
use crate::pricing::UnitPrice;
use crate::provider::{InferenceRequest, InferenceResponse, ProviderError, Result};
use crate::responses::client::Endpoint;

/// What a media model does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Make or edit images.
    Image,
    /// Make, edit or extend video.
    Video,
    /// Speak text.
    Speech,
    /// Transcribe audio.
    Transcribe,
}

/// The media kind `model` is, or `None` for a chat model.
pub(crate) fn kind(model: &str) -> Option<Kind> {
    match model {
        m if m.starts_with("grok-imagine-image") => Some(Kind::Image),
        m if m.starts_with("grok-imagine-video") => Some(Kind::Video),
        m if m.starts_with("grok-tts") => Some(Kind::Speech),
        m if m.starts_with("grok-stt") => Some(Kind::Transcribe),
        _ => None,
    }
}

/// The media models this build names, as `(id, display name)`, for a
/// listing that cannot be read. The speech ids are Leviath's own.
pub(crate) const CATALOG: &[(&str, &str)] = &[
    ("grok-imagine-image", "Grok Imagine Image"),
    ("grok-imagine-image-2.0", "Grok Imagine Image 2.0"),
    ("grok-imagine-image-quality", "Grok Imagine Image Quality"),
    ("grok-imagine-video", "Grok Imagine Video"),
    ("grok-imagine-video-1.5", "Grok Imagine Video 1.5"),
    ("grok-tts", "Grok Text to Speech"),
    ("grok-stt", "Grok Speech to Text"),
];

/// How one call is billed: whether the reply's own cost is the call's, and
/// the per-unit price to fall back on.
pub(crate) struct Billing {
    /// Whether a quoted `cost_in_usd_ticks` is this call's cost (an API key)
    /// or not (a subscription).
    pub(crate) reported: bool,
    /// The per-unit price, when one is known.
    pub(crate) unit: Option<UnitPrice>,
}

impl Billing {
    /// The cost from the reply's ticks, else `quantity` units, else unknown.
    /// Nothing at all on a subscription.
    fn cost(&self, reply: &Value, quantity: f64) -> Option<f64> {
        if !self.reported {
            return None;
        }
        reply
            .pointer("/usage/cost_in_usd_ticks")
            .and_then(Value::as_f64)
            .map(|t| t / crate::responses::TICKS_PER_USD)
            .or_else(|| self.unit.map(|u| u.cost(quantity)))
    }
}

/// Run the media model `request` names.
pub(crate) async fn run(
    endpoint: &Endpoint,
    provider: &str,
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
                    provider,
                    shape: images::EditShape::Xai,
                    default_mime: "image/jpeg",
                    hints: &["aspect_ratio", "resolution", "quality"],
                    reported_cost: billing.reported,
                    unit: billing.unit.filter(|_| billing.reported),
                    tokens: None,
                    response_format: true,
                },
                request,
            )
            .await
        }
        Kind::Video => video(endpoint, provider, request, billing, poll_interval).await,
        Kind::Speech => speech(endpoint, provider, request, billing).await,
        Kind::Transcribe => transcribe(endpoint, request, billing).await,
    }
}

/// Make, edit or extend a video, and wait for it.
async fn video(
    endpoint: &Endpoint,
    provider: &str,
    request: &InferenceRequest,
    billing: &Billing,
    poll_interval: Duration,
) -> Result<InferenceResponse> {
    let deadline = media::deadline(request);
    let mut body = serde_json::Map::new();
    body.insert("model".into(), json!(request.model));
    body.insert("prompt".into(), json!(media::request_text(request)));
    if let Some(duration) = media::extra_i64(request, "duration") {
        body.insert("duration".into(), json!(duration));
    }
    for key in ["aspect_ratio", "resolution"] {
        if let Some(value) = media::extra_str(request, key) {
            body.insert(key.into(), json!(value));
        }
    }
    if let Some(image) = media::data_uris(request, |m| m.starts_with("image/"))
        .into_iter()
        .next()
    {
        body.insert("image".into(), json!({ "url": image }));
    }
    let source = media::data_uris(request, |m| m.starts_with("video/"))
        .into_iter()
        .next();
    let path = match (
        source.as_ref(),
        media::extra_str(request, "operation").as_deref(),
    ) {
        (Some(_), Some("extend")) => "/videos/extensions",
        (Some(_), _) => "/videos/edits",
        (None, _) => "/videos/generations",
    };
    if let Some(video) = source {
        body.insert("video".into(), json!({ "url": video }));
    }

    let created = endpoint.post_json(path, &Value::Object(body)).await?;
    let created =
        crate::provider::check_http_response(created, endpoint.rate_limiter.as_ref()).await?;
    let created: Value = crate::provider::decode_json(created).await?;
    let id = created
        .get("request_id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ProviderError::InvalidResponse("the video request answered no request_id".into())
        })?
        .to_string();

    let status_url = endpoint.url(&format!("/videos/{id}"));
    let done = media::poll_until("the xAI video task", deadline, poll_interval, || {
        let url = status_url.clone();
        async move {
            let task = endpoint.get_json(&url).await?;
            Ok(match task.get("status").and_then(Value::as_str) {
                Some("done") => Poll::Done(task),
                Some("failed") => Poll::Failed(
                    task.pointer("/error/message")
                        .or_else(|| task.get("error"))
                        .map(|e| e.as_str().map_or_else(|| e.to_string(), str::to_string))
                        .unwrap_or_else(|| "no reason given".to_string()),
                ),
                _ => Poll::Running,
            })
        }
    })
    .await?;

    let video = done.get("video").unwrap_or(&done);
    let url = video.get("url").and_then(Value::as_str).ok_or_else(|| {
        ProviderError::InvalidResponse("a finished video task carried no URL".into())
    })?;
    let (bytes, mime) = media::download(&endpoint.client, url).await?;
    let mime = mime.unwrap_or_else(|| "video/mp4".to_string());
    let seconds = video.get("duration").and_then(Value::as_f64).unwrap_or(0.0);
    let parts = vec![media::blob(
        &mime,
        bytes,
        &format!("video.{}", media::extension(&mime)),
    )?];
    let cost = billing.cost(&done, seconds);
    let summary = media::summary(&format!("{provider}/{}", request.model), &parts);
    Ok(media::response(summary, parts, cost))
}

/// Speak the request's text.
async fn speech(
    endpoint: &Endpoint,
    provider: &str,
    request: &InferenceRequest,
    billing: &Billing,
) -> Result<InferenceResponse> {
    let text = media::request_text(request);
    if text.is_empty() {
        return Err(ProviderError::InvalidResponse(
            "grok-tts needs text to speak: the stage handed it none".into(),
        ));
    }
    let mut body = serde_json::Map::new();
    body.insert("text".into(), json!(text));
    body.insert(
        "language".into(),
        json!(media::extra_str(request, "language").unwrap_or_else(|| "auto".to_string())),
    );
    if let Some(voice) = media::extra_str(request, "voice_id") {
        body.insert("voice_id".into(), json!(voice));
    }
    if let Some(speed) = media::extra_f64(request, "speed") {
        body.insert("speed".into(), json!(speed));
    }
    let codec = media::extra_str(request, "codec");
    let sample_rate = media::extra_i64(request, "sample_rate");
    if codec.is_some() || sample_rate.is_some() {
        let mut format = serde_json::Map::new();
        if let Some(codec) = &codec {
            format.insert("codec".into(), json!(codec));
        }
        if let Some(rate) = sample_rate {
            format.insert("sample_rate".into(), json!(rate));
        }
        body.insert("output_format".into(), Value::Object(format));
    }

    let response = endpoint.post_json("/tts", &Value::Object(body)).await?;
    let response =
        crate::provider::check_http_response(response, endpoint.rate_limiter.as_ref()).await?;
    let mime = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or(v).trim().to_string())
        .filter(|m| m.starts_with("audio/"))
        .unwrap_or_else(|| "audio/mpeg".to_string());
    let bytes = leviath_net::read_caps::read_body_capped(response, media::DOWNLOAD_CAP)
        .await
        .map_err(ProviderError::from)?
        .to_vec();
    let parts = vec![media::blob(
        &mime,
        bytes,
        &format!("speech.{}", media::extension(&mime)),
    )?];
    let chars = text.chars().count() as f64 / 1_000_000.0;
    let cost = billing.cost(&Value::Null, chars);
    let summary = media::summary(&format!("{provider}/{}", request.model), &parts);
    Ok(media::response(summary, parts, cost))
}

/// Transcribe the request's audio.
async fn transcribe(
    endpoint: &Endpoint,
    request: &InferenceRequest,
    billing: &Billing,
) -> Result<InferenceResponse> {
    let audio = media::first_part(request, |m| m.starts_with("audio/")).ok_or_else(|| {
        ProviderError::InvalidResponse(
            "grok-stt needs an audio part to transcribe: the stage handed it none".into(),
        )
    })?;
    let language = media::extra_str(request, "language");
    let diarization = media::extra_bool(request, "diarization");
    let url = endpoint.url("/stt");
    let name = audio.name.clone().unwrap_or_else(|| "audio".to_string());
    let response = endpoint
        .send(|client| {
            let file = reqwest::multipart::Part::bytes(audio.bytes.clone())
                .file_name(name.clone())
                .mime_str(audio.mime_type.as_str())
                .expect("a stored part's type is a valid mime type");
            let mut form = reqwest::multipart::Form::new().part("file", file);
            if let Some(language) = &language {
                form = form.text("language", language.clone());
            }
            if let Some(on) = diarization {
                form = form.text("diarization", on.to_string());
            }
            client.post(&url).multipart(form)
        })
        .await?;
    let response =
        crate::provider::check_http_response(response, endpoint.rate_limiter.as_ref()).await?;
    let reply: Value = crate::provider::decode_json(response).await?;
    let text = reply
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let hours = reply.get("duration").and_then(Value::as_f64).unwrap_or(0.0) / 3600.0;
    let transcript = serde_json::to_vec_pretty(&reply).expect("a JSON value serialises");
    let parts = vec![media::json_blob(transcript, "transcript.json")];
    let cost = billing.cost(&reply, hours);
    Ok(media::response(text, parts, cost))
}

#[cfg(test)]
mod tests;
