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

/// The `output_format` values an images listing entry says its endpoint
/// accepts: `supported_parameters.output_format.values`, an enum the guide
/// calls definitive. Empty when the entry names none, which the guide reads
/// as unsupported.
pub(super) fn accepted_formats(entry: &serde_json::Value) -> Vec<String> {
    entry
        .pointer("/supported_parameters/output_format/values")
        .and_then(|v| v.as_array())
        .map(|values| {
            values
                .iter()
                .filter_map(|v| v.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
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

    /// Read `GET /images/models` into the catalogue: the `output_format`
    /// values each image model's endpoint accepts. Called by priming after
    /// `/models` has filled the catalogue, so once per prime and never per
    /// draw. Best effort: unread, no format is asked for and the provider's
    /// default stands, as before.
    pub(super) async fn read_image_formats(&self) {
        match self.fetch_image_formats().await {
            Ok(n) => tracing::debug!(models = n, "read OpenRouter's image output formats"),
            Err(e) => {
                tracing::debug!(error = %e, "OpenRouter's image models could not be read")
            }
        }
    }

    /// [`Self::read_image_formats`], answering how many models it amended.
    async fn fetch_image_formats(&self) -> Result<usize> {
        let body = crate::provider::fetch_listing(
            &self.client,
            &format!("{}/images/models", self.base_url),
            &self.listing_headers(),
            Some(crate::provider::SIDE_CALL_TIMEOUT_SECS),
        )
        .await?;
        let rows = body
            .get("data")
            .and_then(|d| d.as_array())
            .ok_or_else(|| ProviderError::InvalidResponse("Missing 'data' array".to_string()))?;
        Ok(rows
            .iter()
            .filter_map(|row| row.get("id").and_then(|v| v.as_str()).map(|id| (id, row)))
            .filter(|(id, row)| {
                let formats = accepted_formats(row);
                self.learned.amend(id, |m| m.image_formats = Some(formats))
            })
            .count())
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
        // The formats the images listing says this model takes; none known,
        // none asked for.
        let formats = self
            .learned
            .get(&request.model)
            .and_then(|m| m.image_formats)
            .unwrap_or_default();
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
                formats: Some(&formats),
            },
            &request,
        )
        .await
    }
}

#[cfg(test)]
#[path = "images_tests.rs"]
mod tests;
