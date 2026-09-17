//! Google's media models.
//!
//! Measured against the live API (2026-09-17):
//!
//! - **Veo** (`veo-*`): `POST /models/{model}:predictLongRunning` with
//!   `instances: [{prompt, image?}]` and `parameters` (`durationSeconds`,
//!   `aspectRatio`, `resolution`, `negativePrompt`) answers an operation
//!   `name`; `GET /{name}` reports `done`, and a finished one carries the
//!   video's URI under `response.generateVideoResponse.generatedSamples`. The
//!   URI is fetched with the key.
//! - **Image, speech and music** (`*-image*`, `nano-banana-*`, `*-tts*`,
//!   `lyria-*`): the same Interactions route as chat, with the prompt alone.
//!   A speech model refuses a system instruction (500) and any turn it would
//!   answer in words ("Model tried to generate text"), so none of the stage's
//!   own instructions, tools or history is sent: the prompt, and the images a
//!   stage hands an image model to edit. Speech arrives as raw 24 kHz PCM in
//!   many deltas, which the stream joins into one WAV.

use std::time::Duration;

use base64::Engine as _;
use serde_json::{Map, Value, json};

use crate::media::{self, Poll};
use crate::provider::{
    ContentBlock, InferenceRequest, InferenceResponse, MessageContent, Provider, ProviderError,
    Result,
};

/// What a media model does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Veo: a video, made in the background and waited for.
    Video,
    /// An image, speech or music model on the Interactions route, sent the
    /// prompt alone.
    Prompted,
}

/// The media models this build names, for `lev validate` and a listing that
/// cannot be read.
pub(crate) const CATALOG: &[(&str, &str)] = &[
    ("veo-3.1-generate-preview", "Veo 3.1"),
    ("veo-3.1-fast-generate-preview", "Veo 3.1 Fast"),
    ("veo-3.1-lite-generate-preview", "Veo 3.1 Lite"),
    ("gemini-3.1-flash-image", "Gemini 3.1 Flash Image"),
    ("gemini-3-pro-image", "Gemini 3 Pro Image"),
    ("nano-banana-pro-preview", "Nano Banana Pro"),
    ("gemini-3.1-flash-tts-preview", "Gemini 3.1 Flash TTS"),
    ("gemini-2.5-flash-preview-tts", "Gemini 2.5 Flash TTS"),
    ("gemini-2.5-pro-preview-tts", "Gemini 2.5 Pro TTS"),
    ("lyria-3.5", "Lyria 3.5"),
    ("lyria-3-pro-preview", "Lyria 3 Pro"),
    ("lyria-3-clip-preview", "Lyria 3 Clip"),
];

/// The media kind `model` is, or `None` for a chat model.
pub(crate) fn kind(model: &str) -> Option<Kind> {
    let m = model.to_ascii_lowercase();
    if m.starts_with("veo") {
        return Some(Kind::Video);
    }
    let prompted = m.contains("-image")
        || m.starts_with("nano-banana")
        || m.contains("-tts")
        || m.starts_with("lyria");
    prompted.then_some(Kind::Prompted)
}

/// `caps` as a media model has them: it calls no tools, it answers with parts
/// rather than tokens, so the reply budget stays small, and the window the
/// context guard measures is not the prompt limit the listing states (Veo
/// lists 480 tokens), since the model is sent the prompt alone and never the
/// stage's context. A chat model's are unchanged.
pub(crate) fn adjusted(
    model: &str,
    mut caps: crate::ModelCapabilities,
) -> crate::ModelCapabilities {
    if kind(model).is_some() {
        caps.supports_tools = false;
        caps.max_output_tokens = caps.max_output_tokens.min(4_096);
        caps.max_context_tokens = caps.max_context_tokens.max(32_000);
    }
    caps
}

/// The Interactions body for a prompted media model: the request's text and
/// the stored parts it carries, no system instruction, no tools, no history.
///
/// Parameters: `voice` and `language` for a speech model (sent as
/// `speech_config`), `aspect_ratio` and `image_size` for an image model
/// (`image_config`), and `temperature` and `seed`.
pub(crate) fn prompted_body(request: &InferenceRequest) -> Value {
    let mut content = vec![json!({ "type": "text", "text": media::request_text(request) })];
    for message in &request.messages {
        if let MessageContent::Blocks(blocks) = &message.content {
            content.extend(
                blocks
                    .iter()
                    .filter(|b| matches!(b, ContentBlock::Mime { data, .. } if !data.is_empty()))
                    .filter_map(crate::mime::gemini_part),
            );
        }
    }
    let mut generation = Map::new();
    let voice = media::extra_str(request, "voice");
    let language = media::extra_str(request, "language");
    if voice.is_some() || language.is_some() {
        let mut speaker = Map::new();
        if let Some(voice) = voice {
            speaker.insert("voice".into(), json!(voice));
        }
        if let Some(language) = language {
            speaker.insert("language".into(), json!(language));
        }
        generation.insert("speech_config".into(), json!([speaker]));
    }
    let mut image = Map::new();
    for key in ["aspect_ratio", "image_size"] {
        if let Some(value) = media::extra_str(request, key) {
            image.insert(key.into(), json!(value));
        }
    }
    if !image.is_empty() {
        generation.insert("image_config".into(), Value::Object(image));
    }
    if let Some(t) = media::extra_f64(request, "temperature") {
        generation.insert("temperature".into(), json!(t));
    }
    if let Some(seed) = media::extra_i64(request, "seed") {
        generation.insert("seed".into(), json!(seed));
    }
    let mut body = Map::new();
    body.insert("model".into(), json!(request.model));
    body.insert(
        "input".into(),
        json!([{ "type": "user_input", "content": content }]),
    );
    body.insert("store".into(), json!(false));
    body.insert("stream".into(), json!(true));
    if !generation.is_empty() {
        body.insert("generation_config".into(), Value::Object(generation));
    }
    Value::Object(body)
}

/// `stream` with its closing chunk priced at `unit` for every file the stream
/// handed on: a model billed by the clip (Lyria) reports no token cost, and
/// its usage alone would record the call as free.
pub(crate) fn priced_by_unit(
    stream: crate::rate_limit::ChunkStream,
    unit: crate::pricing::UnitPrice,
) -> crate::rate_limit::ChunkStream {
    use tokio_stream::StreamExt as _;
    let mut files = 0usize;
    Box::pin(stream.map(move |chunk| {
        chunk.map(|mut chunk| {
            files += chunk.parts.len();
            // The closing chunk is the one that carries usage.
            if chunk.finish_reason.is_some() {
                chunk.tokens = chunk
                    .tokens
                    .take()
                    .map(|usage| usage.with_reported_cost(Some(unit.cost(files as f64))));
            }
            chunk
        })
    }))
}

impl super::GeminiProvider {
    /// Make a Veo video, wait for it, and keep it.
    pub(super) async fn video(
        &self,
        request: &InferenceRequest,
        poll_interval: Duration,
    ) -> Result<InferenceResponse> {
        let deadline = media::deadline(request);
        let prompt = media::request_text(request);
        if prompt.is_empty() {
            return Err(ProviderError::InvalidResponse(format!(
                "google/{} needs a prompt: the stage handed it no text",
                request.model
            )));
        }
        let mut instance = Map::new();
        instance.insert("prompt".into(), json!(prompt));
        if let Some(image) = media::first_part(request, |m| m.starts_with("image/")) {
            instance.insert(
                "image".into(),
                json!({
                    "bytesBase64Encoded": base64::engine::general_purpose::STANDARD.encode(&image.bytes),
                    "mimeType": image.mime_type.as_str(),
                }),
            );
        }
        let seconds = media::extra_i64(request, "duration")
            .or_else(|| media::extra_i64(request, "duration_seconds"));
        let mut parameters = Map::new();
        if let Some(seconds) = seconds {
            parameters.insert("durationSeconds".into(), json!(seconds));
        }
        for (key, field) in [
            ("aspect_ratio", "aspectRatio"),
            ("resolution", "resolution"),
            ("negative_prompt", "negativePrompt"),
        ] {
            if let Some(value) = media::extra_str(request, key) {
                parameters.insert(field.into(), json!(value));
            }
        }
        let body = json!({ "instances": [instance], "parameters": parameters });
        let url = format!(
            "{}/models/{}:predictLongRunning",
            self.base_url, request.model
        );
        let started = self.post_media(&url, &body).await?;
        let name = started
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ProviderError::InvalidResponse(
                    "the video request answered no operation name".into(),
                )
            })?
            .to_string();

        let operation_url = format!("{}/{name}", self.base_url);
        let done = media::poll_until("the Veo video operation", deadline, poll_interval, || {
            let url = operation_url.clone();
            async move {
                let operation = self.get_media_json(&url).await?;
                Ok(
                    match (
                        operation.get("done").and_then(Value::as_bool),
                        operation.get("error"),
                    ) {
                        (_, Some(error)) => Poll::Failed(
                            error
                                .get("message")
                                .and_then(Value::as_str)
                                .map_or_else(|| error.to_string(), str::to_string),
                        ),
                        (Some(true), None) => Poll::Done(operation),
                        _ => Poll::Running,
                    },
                )
            }
        })
        .await?;

        let samples = done.pointer("/response/generateVideoResponse/generatedSamples");
        let uri = samples
            .and_then(|s| s.pointer("/0/video/uri"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                // A prompt Veo's filters refused finishes with no sample and
                // says why beside it.
                let why = done
                    .pointer("/response/generateVideoResponse/raiMediaFilteredReasons/0")
                    .and_then(Value::as_str)
                    .unwrap_or("it carried no video");
                ProviderError::InvalidResponse(format!("the Veo operation finished, but {why}"))
            })?;
        // Veo makes MP4, whatever type the download is served as.
        let bytes = self.download_media(uri).await?;
        let parts = vec![media::typed_blob("video/mp4", bytes, "video.mp4")];
        // Veo 3.1 makes eight seconds unless asked otherwise.
        let length = seconds.unwrap_or(8) as f64;
        let cost = self
            .pricing(&request.model)
            .and_then(|p| p.unit)
            .map(|u| u.cost(length));
        Ok(media::response(
            media::summary(&format!("google/{}", request.model), &parts),
            parts,
            cost,
        ))
    }

    /// POST `body` to `url` with the key, and read the JSON answer.
    async fn post_media(&self, url: &str, body: &Value) -> Result<Value> {
        let response = crate::openai_compat::send_chat_request(
            &self.client,
            "gemini",
            url,
            &self.headers(),
            body,
            self.rate_limiter.as_ref(),
            Some(crate::provider::SIDE_CALL_TIMEOUT_SECS),
        )
        .await?;
        crate::provider::decode_json(response).await
    }

    /// GET JSON from `url` with the key.
    async fn get_media_json(&self, url: &str) -> Result<Value> {
        let response = crate::provider::with_extra_headers(
            self.client.get(url).header("x-goog-api-key", &self.api_key),
            &self.extra_headers,
        )
        .send()
        .await
        .map_err(|e| ProviderError::transport("reading a video operation", &e))?;
        let response =
            crate::provider::check_http_response(response, self.rate_limiter.as_ref()).await?;
        crate::provider::decode_json(response).await
    }

    /// Download a generated file, which Google serves behind the same key.
    async fn download_media(&self, url: &str) -> Result<Vec<u8>> {
        let response = crate::provider::with_extra_headers(
            self.client.get(url).header("x-goog-api-key", &self.api_key),
            &self.extra_headers,
        )
        .timeout(Duration::from_secs(300))
        .send()
        .await
        .map_err(|e| ProviderError::transport("downloading a generated video", &e))?;
        let response = crate::provider::check_http_response(response, None).await?;
        media::body_bytes(response).await
    }
}

#[cfg(test)]
#[path = "media_tests.rs"]
mod tests;
