//! Images through the gateway's Images API, for the models chat refuses.
//!
//! Read from OpenRouter's own documentation (the image generation guide and
//! the `GET /models` reference, 2026-11-11) and its public listing:
//!
//! - `POST /images` takes `model`, `prompt`, and reference images as
//!   `input_references: [{type: "image_url", image_url: {url}}]`, where the
//!   url may be a base64 data URL; `n`, `resolution`, `aspect_ratio`, `size`,
//!   `quality`, `output_format` and `background` are optional;
//! - it answers `data: [{b64_json, media_type}]` and `usage.cost` in US
//!   dollars;
//! - each listing entry's `endpoints` names the request paths that serve the
//!   model. The image models chat refuses ("is an image generation model and
//!   cannot be used with the chat/completions endpoint. Use the
//!   /api/v1/images endpoint instead.") list `/api/v1/images` and no chat
//!   path; ones that draw on chat too (`google/gemini-3-pro-image`) list both.
//!
//! So the listing decides, never a list of names. A model the listing has not
//! described (it could not be read, or does not name the model) is sent to
//! chat, and the refusal that names the Images API sends it here and is
//! remembered for the rest of the process.

use super::OpenRouterProvider;
use super::catalog::gateway_id;
use crate::media::images;
use crate::provider::{InferenceRequest, InferenceResponse, ProviderError, Result};
use crate::responses::client::{Auth, Endpoint};

/// The path the listing names for the Images API.
const IMAGES_PATH: &str = "/api/v1/images";

/// The path the listing names for chat.
const CHAT_PATH: &str = "/api/v1/chat/completions";

/// The stage parameters an image request takes as written.
const HINTS: &[&str] = &[
    "resolution",
    "aspect_ratio",
    "size",
    "quality",
    "output_format",
    "background",
];

/// What a chat send came to: the gateway's answer, or the picture the Images
/// API drew after chat refused the model and named it.
pub(super) enum Sent {
    /// Chat's answer, still to be read.
    Chat(reqwest::Response),
    /// The picture, drawn on the Images API.
    Drawn(InferenceResponse),
}

/// Whether a listing's `endpoints` say the model is served on the Images API
/// and not on chat.
pub(super) fn images_only(endpoints: &[String]) -> bool {
    let serves = |path: &str| endpoints.iter().any(|p| p == path);
    serves(IMAGES_PATH) && !serves(CHAT_PATH)
}

impl OpenRouterProvider {
    /// Whether `request` goes to the Images API: the listing names only that
    /// path for its model, or chat has already refused the model and named
    /// it.
    pub(super) fn is_image_job(&self, request: &InferenceRequest) -> bool {
        let id = self.listed_id(gateway_id(&request.model));
        self.images_route.contains(&id)
            || self
                .learned
                .get(&id)
                .and_then(|m| m.endpoints)
                .is_some_and(|paths| images_only(&paths))
    }

    /// Send a chat request, retrying a refused temperature as every chat call
    /// does; a refusal naming the Images API is answered from there instead.
    pub(super) async fn send_chat(
        &self,
        url: &str,
        headers: &[(&str, String)],
        body: &mut serde_json::Value,
        request: &InferenceRequest,
    ) -> Result<Sent> {
        match self
            .send_with_temperature_retry(url, headers, body, request)
            .await
        {
            Err(refused) if self.learns_images_route(&request.model, &refused) => {
                Ok(Sent::Drawn(self.image(request).await?))
            }
            sent => sent.map(Sent::Chat),
        }
    }

    /// Whether chat's refusal `error` says the model is served on the Images
    /// API; if so the model is remembered as one, so this call and every
    /// later one go there.
    fn learns_images_route(&self, model: &str, error: &ProviderError) -> bool {
        let named = error.failure_kind() == Some(crate::FailureKind::WrongEndpoint)
            && error.to_string().contains(IMAGES_PATH);
        if named {
            let id = self.listed_id(gateway_id(model));
            tracing::info!(
                model = %id,
                "OpenRouter serves this model on its Images API only; sending it there"
            );
            self.images_route.insert(&id);
        }
        named
    }

    /// Make the picture `request` asks for on the Images API: the stage's
    /// prompt, the images it handed over as references, and the cost the
    /// reply reports.
    pub(super) async fn image(&self, request: &InferenceRequest) -> Result<InferenceResponse> {
        let mut endpoint = Endpoint::new(
            self.client.clone(),
            &self.base_url,
            Auth::Key(self.api_key.clone()),
        );
        endpoint.extra_headers = vec![
            ("HTTP-Referer".into(), "https://leviath.dev".into()),
            ("X-Title".into(), "Leviath".into()),
        ];
        endpoint
            .extra_headers
            .extend(self.extra_headers.iter().cloned());
        endpoint.rate_limiter = self.rate_limiter.clone();
        endpoint.request_timeout_secs = request.request_timeout_secs;
        let request = InferenceRequest {
            model: self.listed_id(gateway_id(&request.model)),
            ..request.clone()
        };
        images::run(
            &endpoint,
            &images::Route {
                provider: "openrouter",
                shape: images::EditShape::OpenRouter,
                default_mime: "image/png",
                hints: HINTS,
                reported: Some(images::USD),
                unit: None,
                tokens: None,
                response_format: false,
            },
            &request,
        )
        .await
    }
}

#[cfg(test)]
#[path = "images_tests.rs"]
mod tests;
