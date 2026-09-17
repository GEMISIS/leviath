//! Google Gemini provider, on the Interactions API.
//!
//! `POST /v1beta/interactions` with `store: false`: a stored interaction is
//! kept 55 days on the paid tier, and nothing here reads one back, since every
//! request carries its whole conversation. Requests take a part by the `uri`
//! of a file uploaded to Google once (images, audio, video and PDFs), stream
//! as steps, and hand a function call's thought signature back on the turn
//! that answers it.
//!
//! The base URL is the native API root (`.../v1beta`). A configured
//! `google_base_url` ending in `/openai` names the OpenAI-compatible endpoint,
//! and is read as the root above it.
//!
//! Veo, and the image, speech and music models, are served too (see
//! `media`).

mod files;
pub(crate) mod media;
mod request;
mod stream;

use crate::learned::{LearnedModel, LearnedModels};
use crate::openai_compat::send_chat_request;
use crate::provider::{
    InferenceRequest, InferenceResponse, LimitsSource, ModelCapabilities, ModelCapabilityOverride,
    ModelInfo, Provider, ProviderError, Result, StreamChunk,
};
use crate::rate_limit::RateLimiter;
use async_trait::async_trait;
use futures_core::Stream;
use std::collections::HashMap;
use std::pin::Pin;
use std::time::Duration;

/// The native API root.
pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

/// Gemini model family, classified from a model id, used to pick per-family
/// capability defaults. Values are identical across families today; the split
/// exists so a family's limits can diverge without reworking the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GeminiFamily {
    /// Cost-efficient, high-volume variants (`*-flash-lite`).
    FlashLite,
    /// Reasoning-first pro variants (`*-pro*`).
    Pro,
    /// Standard flash variants (`*-flash*`, excluding flash-lite).
    Flash,
    /// Anything else / future models.
    Other,
}

impl GeminiFamily {
    /// `(max_context_tokens, max_output_tokens)` for the Flash-Lite family.
    const FLASH_LITE_LIMITS: (usize, usize) = (1_048_576, 65_535);
    /// The same, for Pro.
    const PRO_LIMITS: (usize, usize) = (1_048_576, 65_535);
    /// The same, for Flash.
    const FLASH_LIMITS: (usize, usize) = (1_048_576, 65_535);
    /// The same, for anything this classifier does not recognise.
    const OTHER_LIMITS: (usize, usize) = (1_048_576, 65_535);

    /// This family's context and output ceilings.
    ///
    /// Four named constants that happen to agree today, rather than one shared
    /// value: the point is that a family's limits can move without disturbing
    /// the others, and four constants say that where four identical match arms
    /// only looked like an oversight - which is what the lint kept reporting.
    const fn limits(self) -> (usize, usize) {
        match self {
            Self::FlashLite => Self::FLASH_LITE_LIMITS,
            Self::Pro => Self::PRO_LIMITS,
            Self::Flash => Self::FLASH_LIMITS,
            Self::Other => Self::OTHER_LIMITS,
        }
    }

    fn classify(model: &str) -> Self {
        if model.contains("flash-lite") {
            GeminiFamily::FlashLite
        } else if model.contains("pro") {
            GeminiFamily::Pro
        } else if model.contains("flash") {
            GeminiFamily::Flash
        } else {
            GeminiFamily::Other
        }
    }
}

/// How many entries one native listing page asks for.
///
/// The endpoint served 53 models in a single page at this size when measured;
/// the page token is followed regardless.
const NATIVE_PAGE_SIZE: usize = 200;

/// What the family table says about `model`, for a caller with no provider
/// in hand.
pub(crate) fn table_capabilities(model: &str) -> ModelCapabilities {
    let (max_context_tokens, max_output_tokens) = GeminiFamily::classify(model).limits();
    media::adjusted(
        model,
        ModelCapabilities {
            supports_temperature: true,
            supports_streaming: true,
            supports_tools: true,
            supports_system_prompt: true,
            max_context_tokens,
            max_output_tokens,
            limits_source: LimitsSource::Builtin,
        },
    )
}

/// The models this build names when the listing cannot be read, as
/// `(id, display name)`.
pub(crate) const CATALOG: &[(&str, &str)] = &[
    ("gemini-3.5-flash", "Gemini 3.5 Flash"),
    ("gemini-3.1-pro-preview", "Gemini 3.1 Pro (preview)"),
    ("gemini-3-flash", "Gemini 3 Flash"),
    ("gemini-3.1-flash-lite", "Gemini 3.1 Flash Lite"),
    ("gemini-2.5-pro", "Gemini 2.5 Pro"),
    ("gemini-2.5-flash", "Gemini 2.5 Flash"),
    ("gemini-2.5-flash-lite", "Gemini 2.5 Flash Lite"),
    // The one that draws: an offline listing used to offer no model that
    // makes images, so the picker's "makes images" tag had nothing to sit on.
    ("gemini-2.5-flash-image", "Gemini 2.5 Flash Image"),
];

/// Google Gemini provider.
pub struct GeminiProvider {
    /// HTTP client
    client: reqwest::Client,

    /// API key
    api_key: String,

    /// The native API root, with no trailing slash.
    base_url: String,

    /// Rate limiter
    rate_limiter: Option<RateLimiter>,

    /// What the native `/v1beta/models` listing said about each model, filled
    /// by [`Provider::prime_capabilities`]: both limits and whether the model
    /// samples, which the sync `capabilities()` path reads.
    learned: LearnedModels,

    /// Per-model capability overrides
    capability_overrides: HashMap<String, ModelCapabilityOverride>,

    /// The operator's extra headers, sent after the provider's own on every
    /// request to `base_url`: a gateway's token, a tenant tag.
    extra_headers: Vec<(String, String)>,

    /// How often an uploaded file still processing is checked on.
    file_poll: Duration,

    /// How often a Veo operation is checked on.
    video_poll: Duration,
}

impl GeminiProvider {
    /// Create a new Gemini provider.
    pub fn new(client: reqwest::Client, api_key: String) -> Self {
        Self {
            client,
            api_key,
            base_url: DEFAULT_BASE_URL.to_string(),
            rate_limiter: None,
            learned: Default::default(),
            capability_overrides: HashMap::new(),
            extra_headers: Vec::new(),
            file_poll: Duration::from_secs(2),
            video_poll: Duration::from_secs(10),
        }
    }

    /// Create a new Gemini provider with per-model capability overrides.
    pub fn with_overrides(
        client: reqwest::Client,
        api_key: String,
        overrides: HashMap<String, ModelCapabilityOverride>,
        rate_limit: Option<&crate::provider::RateLimitConfig>,
    ) -> Self {
        Self {
            rate_limiter: rate_limit.map(crate::rate_limit::RateLimiter::new),
            capability_overrides: overrides,
            ..Self::new(client, api_key)
        }
    }

    /// Extra headers on every request to the host, after the provider's own:
    /// what a gateway named in `with_base_url` wants of its own.
    pub fn with_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.extra_headers = headers;
        self
    }

    /// Point this provider at a different host speaking the native API: an
    /// enterprise gateway or a proxy. `None` keeps the built-in default. A URL
    /// ending in `/openai` names the compatibility endpoint, and the native
    /// root above it is what is used.
    pub fn with_base_url(mut self, base_url: Option<String>) -> Self {
        if let Some(url) = base_url {
            let url = url.trim_end_matches('/');
            self.base_url = url.strip_suffix("/openai").unwrap_or(url).to_string();
        }
        self
    }

    /// Return built-in capability defaults for a model, by family. The native
    /// listing's per-model limits correct these once primed.
    fn builtin_capabilities(&self, model: &str) -> ModelCapabilities {
        table_capabilities(model)
    }

    /// The key and content type every request carries, then the operator's.
    fn headers(&self) -> Vec<(&str, String)> {
        crate::provider::with_extra_header_pairs(
            vec![
                ("x-goog-api-key", self.api_key.clone()),
                ("Content-Type", "application/json".to_string()),
            ],
            &self.extra_headers,
        )
    }

    /// Call Gemini's exact native `:countTokens` endpoint for `text`.
    ///
    /// Wraps the text as a single user content part. Returns the reported
    /// `totalTokens`, or an error the caller turns into a heuristic fallback.
    ///
    /// Over the pooled side-call client and through the rate limiter, for the
    /// reasons given on the Anthropic twin: the guard makes this call before
    /// every large request, and it spends the same request quota.
    async fn count_tokens_remote(&self, text: &str, model: &str) -> Result<usize> {
        let url = format!("{}/models/{}:countTokens", self.base_url, model);
        let body = serde_json::json!({
            "contents": [{ "role": "user", "parts": [{ "text": text }] }],
        });
        let response = send_chat_request(
            crate::provider::side_call_client(),
            "gemini",
            &url,
            &self.headers(),
            &body,
            self.rate_limiter.as_ref(),
            Some(crate::provider::SIDE_CALL_TIMEOUT_SECS),
        )
        .await?;
        let value: serde_json::Value = crate::provider::decode_json(response).await?;
        value
            .get("totalTokens")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .ok_or_else(|| {
                ProviderError::InvalidResponse("countTokens missing totalTokens".to_string())
            })
    }
}

#[async_trait]
impl Provider for GeminiProvider {
    async fn infer(&self, request: &InferenceRequest) -> Result<InferenceResponse> {
        if media::kind(&request.model) == Some(media::Kind::Video) {
            return self.video(request, self.video_poll).await;
        }
        // The route streams; a buffered call is the stream collected, so the
        // two paths cannot read the same answer differently.
        crate::provider::collect_stream(self.infer_stream(request).await?).await
    }

    async fn infer_stream(
        &self,
        request: &InferenceRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>> {
        tracing::debug!(model = %request.model, "Calling Gemini API");
        let kind = media::kind(&request.model);
        if kind == Some(media::Kind::Video) {
            return Ok(crate::media::one_chunk(
                self.video(request, self.video_poll).await?,
            ));
        }

        if let Some(limiter) = &self.rate_limiter {
            limiter.acquire().await?;
        }

        let body = match kind {
            Some(_) => media::prompted_body(request),
            None => request::build(
                request,
                self.capabilities(&request.model).supports_temperature,
            ),
        };
        let response = send_chat_request(
            &self.client,
            "gemini",
            &format!("{}/interactions", self.base_url),
            &self.headers(),
            &body,
            self.rate_limiter.as_ref(),
            request.request_timeout_secs,
        )
        .await?;

        let peer = leviath_net::read_caps::peer_of(&response);
        let stream = stream::sse_stream(response.bytes_stream()).sent_by(peer);
        let metered = crate::rate_limit::meter_stream(self.rate_limiter.as_ref(), Box::pin(stream));
        Ok(
            match kind.and(self.pricing(&request.model).and_then(|p| p.unit)) {
                Some(unit) => media::priced_by_unit(metered, unit),
                None => metered,
            },
        )
    }

    async fn count_tokens(&self, text: &str, model: &str) -> usize {
        // Through the limiter first, the way `infer` is: the count endpoint
        // spends the same request quota. Then prefer Gemini's exact native
        // `:countTokens` endpoint, and fall back to the local heuristic on any
        // error (network, non-2xx, parse).
        if let Some(limiter) = &self.rate_limiter {
            // Waits for a slot. `acquire` has no failure today, and this
            // method has no error to carry one anyway: the heuristic below is
            // the fallback for the count, not for the wait.
            limiter
                .acquire()
                .await
                .expect("the rate limiter only waits for capacity; it does not fail");
        }
        match self.count_tokens_remote(text, model).await {
            Ok(n) => n,
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    "Gemini countTokens endpoint failed; using heuristic"
                );
                crate::tokenizer::count_tokens(text, model)
            }
        }
    }

    fn max_context_tokens(&self, model: &str) -> usize {
        self.capabilities(model).max_context_tokens
    }

    fn name(&self) -> &str {
        "google"
    }

    fn serves_model(&self, model_key: &str) -> Option<String> {
        // Google's models are named `gemini-*`, which is a surer signal than
        // the capability table's shape: that table answers how big a window to
        // assume, and its fallback for an unknown model is a guess that can
        // look exactly like a real entry.
        (model_key.starts_with("gemini")
            || media::kind(model_key).is_some()
            || self.capability_overrides.contains_key(model_key))
        .then(|| model_key.to_string())
    }

    fn pricing(&self, model: &str) -> Option<crate::ModelPricing> {
        // Config first: it is the only source that can know a negotiated rate,
        // and the shipped table is a transcription of a public page that may
        // have moved since this build.
        self.capability_overrides
            .get(model)
            .and_then(|o| o.pricing())
            .or_else(|| crate::pricing::published_rates("google", model))
    }

    fn learned_models(&self) -> Option<&crate::learned::LearnedModels> {
        Some(&self.learned)
    }

    fn capabilities(&self, model: &str) -> ModelCapabilities {
        let base = media::adjusted(
            model,
            self.learned
                .corrected(model, self.builtin_capabilities(model)),
        );
        // Merged, not swapped: an entry names only what it corrects.
        match self.capability_overrides.get(model) {
            Some(o) => o.apply_to(base),
            None => base,
        }
    }

    fn mime(&self, model: &str) -> crate::capabilities::ModelMime {
        let base = self
            .learned
            .mime_corrected(model, crate::mime_tables::gemini(model));
        let mime = match self.capability_overrides.get(model) {
            Some(o) => o.apply_mime(base),
            None => base,
        };
        match media::kind(model) {
            Some(_) => mime,
            None => crate::mime::WireShape::Gemini.carried(mime),
        }
    }

    /// Images, audio, video and PDFs by file uri for a chat model. A media
    /// model is sent its inputs in the request.
    fn media_limits(&self, model: &str) -> crate::files::MediaLimits {
        let limits = crate::files::provider_limits("google");
        match media::kind(model) {
            Some(_) => limits.inline_only(),
            None => limits,
        }
    }

    async fn upload_file(
        &self,
        upload: &crate::files::FileUpload,
    ) -> Result<crate::files::RemoteFile> {
        self.upload(upload, self.file_poll).await
    }

    async fn delete_file(&self, file: &crate::files::RemoteFile) -> Result<()> {
        self.delete(file).await
    }

    /// Every id the native listing named, once primed.
    ///
    /// Chat models only: `parse_native_entry` keeps an entry only when it
    /// serves `generateContent`, so the embeddings and video models the
    /// listing also carries are never published as something a stage could
    /// run on.
    fn served_catalog(&self) -> Option<Vec<String>> {
        self.learned.catalog()
    }

    /// Read the native `/v1beta/models` listing into `Self::learned`.
    ///
    /// What the listing fills, measured against the live endpoint: the
    /// display name, both limits (`inputTokenLimit`, `outputTokenLimit`) and
    /// whether the model samples (`maxTemperature`, absent on embeddings and
    /// video models). It says nothing about tools, so tools are recorded as
    /// taken by every chat model, an assumption grounded in every
    /// `generateContent` model taking them; and nothing about price or dates.
    /// `thinking`, `topP` and `topK` are present and ignored. See
    /// `parse_native_entry`.
    async fn prime_capabilities(&self) -> Result<()> {
        let learned = self.fetch_native_catalog().await?;
        let count = learned.len();
        self.learned.replace(learned);
        tracing::debug!(models = count, "learned Gemini model capabilities");
        Ok(())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        // Answered from the primed store, so it cannot disagree with what an
        // inference is told.
        if self.learned.is_empty() {
            self.prime_capabilities().await?;
        }
        Ok(self
            .learned
            .to_model_infos("google", |id| self.capabilities(id)))
    }
}

/// One native listing entry as a [`LearnedModel`], or `None` for a model a
/// stage cannot run on.
///
/// `name` is like `models/gemini-3.5-flash`; the id drops the prefix. An
/// entry whose `supportedGenerationMethods` leaves out `generateContent` is
/// not a chat model (embeddings, video, `aqa`) and is dropped rather than
/// listed with limits a stage could never use. An entry with no such array
/// is kept: absent is "did not say", and the live listing always says.
fn parse_native_entry(item: &serde_json::Value) -> Option<(String, LearnedModel)> {
    let name = item.get("name")?.as_str()?;
    let id = name.strip_prefix("models/").unwrap_or(name).to_string();
    // Veo speaks `predictLongRunning` alone, and is served.
    if let Some(methods) = item
        .get("supportedGenerationMethods")
        .and_then(|v| v.as_array())
        && !methods.iter().any(|m| {
            matches!(
                m.as_str(),
                Some("generateContent") | Some("predictLongRunning")
            )
        })
    {
        return None;
    }
    let size = |key: &str| item.get(key).and_then(|v| v.as_u64()).map(|n| n as usize);
    // `maxTemperature` is the listing's own word on sampling: a chat model
    // that takes a temperature publishes its ceiling, and one that does not
    // publishes zero. Measured, every chat model carried the field, so an
    // entry without it is one the listing did not describe.
    let samples = item
        .get("maxTemperature")
        .and_then(|v| v.as_f64())
        .map(|t| t > 0.0);
    Some((
        id,
        LearnedModel {
            display_name: item
                .get("displayName")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            max_context_tokens: size("inputTokenLimit"),
            max_output_tokens: size("outputTokenLimit"),
            supports_temperature: samples,
            supports_tools: Some(true),
            // The native API caches through `createCachedContent`, not
            // through markers on a chat request, so this signal has no
            // meaning here.
            explicit_cache_control: None,
            pricing: None,
            released: None,
            retires: None,
            // The listing names no modalities; the family table answers.
            input_types: None,
            output_types: None,
        },
    ))
}

impl GeminiProvider {
    /// GET one page of the models listing and return its body.
    async fn fetch_model_listing(&self, url: String) -> Result<serde_json::Value> {
        let response = crate::provider::apply_request_timeout(
            crate::provider::with_extra_headers(
                self.client.get(url).header("x-goog-api-key", &self.api_key),
                &self.extra_headers,
            ),
            Some(crate::provider::SIDE_CALL_TIMEOUT_SECS),
        )
        .send()
        .await
        .map_err(|e| ProviderError::transport("listing models", &e))?;
        let response = crate::provider::check_http_response(response, None).await?;
        crate::provider::decode_json(response).await
    }

    /// The array under `field`, or the error a listing without one is.
    fn listing_array(body: &serde_json::Value, field: &str) -> Result<Vec<serde_json::Value>> {
        body.get(field)
            .and_then(|d| d.as_array())
            .cloned()
            .ok_or_else(|| {
                ProviderError::InvalidResponse(format!("No {field} field in models response"))
            })
    }

    /// Every page of the native `/v1beta/models` listing, parsed.
    ///
    /// Paginated through `nextPageToken`: the endpoint answered 53 entries in
    /// one page at `pageSize=200` when measured, but it documents the token and
    /// a listing that stopped at page one would silently truncate the day it
    /// is needed.
    async fn fetch_native_catalog(&self) -> Result<HashMap<String, LearnedModel>> {
        let mut learned = HashMap::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut url = format!("{}/models?pageSize={}", self.base_url, NATIVE_PAGE_SIZE);
            if let Some(token) = &page_token {
                url.push_str("&pageToken=");
                url.push_str(token);
            }
            let body = self.fetch_model_listing(url).await?;
            learned.extend(
                Self::listing_array(&body, "models")?
                    .iter()
                    .filter_map(parse_native_entry),
            );
            match body.get("nextPageToken").and_then(|v| v.as_str()) {
                Some(token) if !token.is_empty() => page_token = Some(token.to_string()),
                _ => return Ok(learned),
            }
        }
    }
}

#[cfg(test)]
mod mime_tests;

#[cfg(test)]
mod tests;
