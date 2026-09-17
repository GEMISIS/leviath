//! Bedrock's image models, on `InvokeModel` rather than Converse.
//!
//! Measured against the live API with a Bedrock API key (2026-09-17):
//!
//! - **Stability** (`stability.*`): `POST /model/{id}/invoke` with the prompt
//!   and the model's own fields, answered with `images` (base64),
//!   `finish_reasons` (a reason when a filter refused) and `seeds`. The
//!   generators (`sd3-5-large`, `stable-image-core`, `stable-image-ultra`) take
//!   a prompt; the editing tools (`stable-image-remove-background`,
//!   `-search-recolor`, `-inpaint` and the rest) take an `image` too, and are
//!   reached through their `us.` inference profile.
//! - **Nova Canvas** and **Titan Image** (`amazon.nova-canvas-*`,
//!   `amazon.titan-image-*`): a `taskType` with its parameters and an
//!   `imageGenerationConfig`, answered with `images` and an `error`.
//!
//! Nova Reel (video) is not served: it writes its result to an S3 bucket the
//! caller names, and a Bedrock API key cannot read S3.

use base64::Engine as _;
use serde_json::{Map, Value, json};

use crate::media;
use crate::provider::{InferenceRequest, InferenceResponse, ProviderError, Result};

/// Whether `model` is an image model this provider runs.
pub(crate) fn is_image_model(model: &str) -> bool {
    let bare = super::catalog::bare_id(model);
    bare.starts_with("stability.")
        || bare.starts_with("amazon.nova-canvas")
        || bare.starts_with("amazon.titan-image")
}

/// The Stability tools that take an image and no prompt, and refuse one
/// (400 "Invalid field 'prompt' in request. Available fields for remove-bg are:
/// image, output_format").
const UNPROMPTED_TOOLS: &[&str] = &[
    "stability.stable-image-remove-background",
    "stability.stable-fast-upscale",
];

/// The InvokeModel body for `request` on `model`.
///
/// Stability: the prompt, the first image part as `image`, and every stage
/// parameter as written (`aspect_ratio`, `negative_prompt`, `seed`,
/// `select_prompt`, `strength` and whatever a tool takes). SD3.5 given an image
/// is an image-to-image call. Nova Canvas and Titan: `TEXT_IMAGE`, or
/// `IMAGE_VARIATION` when handed an image, with the parameters as the
/// generation config (`numberOfImages`, `width`, `height`, `cfgScale`,
/// `seed`, `quality`).
pub(crate) fn body(model: &str, request: &InferenceRequest) -> Value {
    let prompt = media::request_text(request);
    let image = media::first_part(request, |m| m.starts_with("image/"))
        .map(|part| base64::engine::general_purpose::STANDARD.encode(&part.bytes));
    let extra = request.extra.as_object().cloned().unwrap_or_default();
    let bare = super::catalog::bare_id(model);
    if bare.starts_with("stability.") {
        let mut body = Map::new();
        if !prompt.is_empty() && !UNPROMPTED_TOOLS.iter().any(|tool| bare.starts_with(tool)) {
            body.insert("prompt".into(), json!(prompt));
        }
        if let Some(image) = image {
            body.insert("image".into(), json!(image));
            if bare.starts_with("stability.sd3") {
                body.insert("mode".into(), json!("image-to-image"));
                body.insert("strength".into(), json!(0.7));
            }
        }
        body.insert("output_format".into(), json!("png"));
        body.extend(extra);
        return Value::Object(body);
    }
    let (task, params) = match image {
        Some(image) => (
            "IMAGE_VARIATION",
            json!({ "imageVariationParams": { "text": prompt, "images": [image] } }),
        ),
        None => (
            "TEXT_IMAGE",
            json!({ "textToImageParams": { "text": prompt } }),
        ),
    };
    let mut config = Map::new();
    config.insert("numberOfImages".into(), json!(1));
    config.extend(extra);
    let mut body = params.as_object().cloned().unwrap_or_default();
    body.insert("taskType".into(), json!(task));
    body.insert("imageGenerationConfig".into(), Value::Object(config));
    Value::Object(body)
}

/// The images a reply carries, or the refusal it reports.
pub(crate) fn images(model: &str, reply: &Value) -> Result<Vec<leviath_core::mime::Blob>> {
    if let Some(error) = reply.get("error").and_then(Value::as_str) {
        return Err(ProviderError::ApiError(format!(
            "bedrock/{model} refused: {error}"
        )));
    }
    if let Some(reason) = reply
        .get("finish_reasons")
        .and_then(Value::as_array)
        .and_then(|r| r.iter().find_map(Value::as_str))
    {
        return Err(ProviderError::ApiError(format!(
            "bedrock/{model} refused: {reason}"
        )));
    }
    let format = reply
        .get("output_format")
        .and_then(Value::as_str)
        .unwrap_or("png");
    let mime = format!("image/{format}");
    let mut parts = Vec::new();
    for (index, data) in reply
        .get("images")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .enumerate()
    {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|e| ProviderError::InvalidResponse(format!("an image was not base64: {e}")))?;
        parts.push(media::blob(
            &mime,
            bytes,
            &format!("image-{}.{}", index + 1, media::extension(&mime)),
        )?);
    }
    if parts.is_empty() {
        return Err(ProviderError::InvalidResponse(format!(
            "bedrock/{model} answered with no image"
        )));
    }
    Ok(parts)
}

impl super::BedrockProvider {
    /// Make or edit images with the model `request` names.
    pub(super) async fn run_image(&self, request: &InferenceRequest) -> Result<InferenceResponse> {
        let url = format!(
            "{}/model/{}/invoke",
            self.runtime_base(),
            super::encode_model_id(&request.model)
        );
        let response = self
            .post(
                &url,
                &body(&request.model, request),
                request.request_timeout_secs,
            )
            .await?;
        let reply: Value = crate::provider::decode_json(response).await?;
        let parts = images(&request.model, &reply)?;
        let cost = crate::pricing::published_unit_rate(
            super::PROVIDER_NAME,
            super::catalog::bare_id(&request.model),
        )
        .map(|row| row.usd * parts.len() as f64);
        Ok(media::response(
            media::summary(&format!("bedrock/{}", request.model), &parts),
            parts,
            cost,
        ))
    }
}

#[cfg(test)]
#[path = "media_tests.rs"]
mod tests;
