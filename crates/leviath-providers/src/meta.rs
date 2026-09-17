//! Meta's Model API: Muse Spark over the Responses API, plus Muse Image and
//! Muse Voice Transcribe.
//!
//! Meta's API speaks three protocols (Responses, Chat Completions and
//! Anthropic's Messages). Responses is the one Meta says carries the model's
//! chain of thought between calls, which is what keeps a tool loop accurate,
//! and it is the one used here. Muse Spark always reasons: an effort of `none`
//! is refused, so it is never sent.
//!
//! The listing (`GET /v1/models`) names the models and when they were added,
//! and nothing else: the windows come from this build's table (every Muse Spark
//! has 1 048 576 tokens) and the prices from the shipped price table, which
//! `cargo xtask prices` keeps current.
//!
//! The `-contributor` models are cheaper because Meta trains on their prompts
//! and completions. They are listed and priced like any other; zero data
//! retention refuses them (see [`crate::retention`]).

pub(crate) mod media;

use std::collections::HashMap;
use std::pin::Pin;

use async_trait::async_trait;
use futures_core::Stream;

use crate::capabilities::{LimitsSource, Match, ModelCapabilities, ModelCapabilityOverride, Row};
use crate::learned::{LearnedModel, LearnedModels};
use crate::provider::{
    InferenceRequest, InferenceResponse, ModelInfo, Provider, RateLimitConfig, Result, StreamChunk,
};
use crate::responses::client::{Auth, Endpoint};
use crate::responses::{Dialect, request as request_body, stream};

/// The registry name.
pub const PROVIDER_NAME: &str = "meta";

/// Where the API lives.
pub const DEFAULT_BASE_URL: &str = "https://api.meta.ai/v1";

/// The route's rules, from Meta's Responses reference: an output cap and a
/// temperature are taken; `stop`, `logit_bias`, `n` and log probabilities are
/// refused with a 400.
pub const DIALECT: Dialect = Dialect {
    provider: PROVIDER_NAME,
    rejected_parameters: &[
        "stop",
        "logit_bias",
        "logprobs",
        "top_logprobs",
        "n",
        "max_tokens",
        "max_completion_tokens",
    ],
    output_cap: true,
    temperature: true,
    verbosity: false,
    reasoning_summary: false,
    reported_cost: false,
    cache_key: true,
};

/// The models named when the listing cannot be read.
pub(crate) const CATALOG: &[(&str, &str)] = &[
    ("muse-spark-1.3", "Muse Spark 1.3"),
    ("muse-spark-1.3-contributor", "Muse Spark 1.3 (contributor)"),
    ("muse-spark-1.2", "Muse Spark 1.2"),
    ("muse-spark-1.2-contributor", "Muse Spark 1.2 (contributor)"),
    ("muse-spark-1.1", "Muse Spark 1.1"),
    ("muse-image-1.0", "Muse Image 1.0"),
    ("muse-voice-transcribe-1.0", "Muse Voice Transcribe 1.0"),
];

/// What this build knows about the models. The output ceiling is not
/// published; this one is conservative, and a stage that asks for more is
/// refused by the API with a message saying so.
pub(crate) const MODELS: &[Row] = &[
    Row {
        matches: &[Match::Prefix("muse-spark")],
        temperature: true,
        tools: true,
        context: 1_048_576,
        output: 131_072,
    },
    Row {
        // Media models: no chat window, no tools, no temperature. They answer
        // with parts, not tokens, so the reply budget is small: one as large
        // as the window left the prompt no room and the context guard refused
        // every call.
        matches: &[Match::Prefix("muse-image"), Match::Prefix("muse-voice")],
        temperature: false,
        tools: false,
        context: 32_000,
        output: 4_096,
    },
];

/// The answer for a model this build does not name.
pub(crate) const FALLBACK_CAPABILITIES: ModelCapabilities = ModelCapabilities {
    supports_temperature: true,
    supports_streaming: true,
    supports_tools: true,
    supports_system_prompt: true,
    max_context_tokens: 128_000,
    max_output_tokens: 32_000,
    limits_source: LimitsSource::Builtin,
};

/// The table's answer for `model`.
pub(crate) fn table_capabilities(model: &str) -> ModelCapabilities {
    crate::capabilities::lookup(MODELS, model, FALLBACK_CAPABILITIES)
}

/// Muse Spark on Meta's Model API.
pub struct MetaProvider {
    endpoint: Endpoint,
    capability_overrides: HashMap<String, ModelCapabilityOverride>,
    effort: Option<String>,
    learned: LearnedModels,
}

impl MetaProvider {
    /// A provider with `api_key`.
    pub fn new(client: reqwest::Client, api_key: String) -> Self {
        Self {
            endpoint: Endpoint::new(client, DEFAULT_BASE_URL, Auth::Key(api_key)),
            capability_overrides: HashMap::new(),
            effort: None,
            learned: Default::default(),
        }
    }

    /// Per-model corrections from `[model_capabilities]`.
    #[must_use]
    pub fn with_overrides(mut self, overrides: HashMap<String, ModelCapabilityOverride>) -> Self {
        self.capability_overrides = overrides;
        self
    }

    /// Apply a rate limit. Meta's limits are per team, so a shared team
    /// wants this set below the team's figure.
    #[must_use]
    pub fn with_rate_limit(mut self, config: Option<&RateLimitConfig>) -> Self {
        self.endpoint.set_rate_limit(config);
        self
    }

    /// Point at another host. `None` keeps Meta's.
    #[must_use]
    pub fn with_base_url(mut self, base_url: Option<String>) -> Self {
        self.endpoint.set_base_url(base_url);
        self
    }

    /// Extra headers on every request, after the provider's own.
    #[must_use]
    pub fn with_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.endpoint.extra_headers = headers;
        self
    }

    /// Bound every request to `secs`.
    #[must_use]
    pub fn with_request_timeout(mut self, secs: Option<u64>) -> Self {
        self.endpoint.request_timeout_secs = secs;
        self
    }

    /// The reasoning effort to send. `none` is dropped: Muse Spark always
    /// reasons and refuses it.
    #[must_use]
    pub fn with_reasoning_effort(mut self, effort: Option<String>) -> Self {
        self.effort = effort.filter(|e| !e.trim().is_empty() && e.trim() != "none");
        self
    }
}

impl MetaProvider {
    /// Run a media model, priced by its unit row.
    async fn run_media(
        &self,
        kind: media::Kind,
        request: &InferenceRequest,
    ) -> Result<InferenceResponse> {
        let unit = self.pricing(&request.model).and_then(|p| p.unit);
        media::run(&self.endpoint, kind, request, unit).await
    }
}

#[async_trait]
impl Provider for MetaProvider {
    async fn infer(&self, request: &InferenceRequest) -> Result<InferenceResponse> {
        if let Some(kind) = media::kind(&request.model) {
            return self.run_media(kind, request).await;
        }
        crate::provider::collect_stream(self.infer_stream(request).await?).await
    }

    async fn infer_stream(
        &self,
        request: &InferenceRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>> {
        if let Some(kind) = media::kind(&request.model) {
            return Ok(crate::media::one_chunk(
                self.run_media(kind, request).await?,
            ));
        }
        let mut body = request_body::build(
            request,
            &DIALECT,
            &request_body::Settings {
                effort: self.effort.as_deref(),
                verbosity: "medium",
                replay_reasoning: true,
            },
        );
        if !self.capabilities(&request.model).supports_temperature {
            body.as_object_mut()
                .expect("a Responses body is an object")
                .remove("temperature");
        }
        let response = self.endpoint.post_json("/responses", &body).await?;
        let response =
            crate::provider::check_http_response(response, self.endpoint.rate_limiter.as_ref())
                .await?;
        let peer = leviath_net::read_caps::peer_of(&response);
        Ok(crate::rate_limit::meter_stream(
            self.endpoint.rate_limiter.as_ref(),
            Box::pin(stream::sse_stream(response.bytes_stream(), DIALECT).sent_by(peer)),
        ))
    }

    async fn count_tokens(&self, text: &str, _model: &str) -> usize {
        leviath_core::estimate_tokens(text)
    }

    fn max_context_tokens(&self, model: &str) -> usize {
        self.capabilities(model).max_context_tokens
    }

    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    /// Images, video, audio and PDFs by file id for Muse Spark. The media
    /// models' own routes take their inputs inline and name no file.
    fn media_limits(&self, model: &str) -> crate::files::MediaLimits {
        let limits = crate::files::provider_limits(PROVIDER_NAME);
        match media::kind(model) {
            Some(_) => limits.inline_only(),
            None => limits,
        }
    }

    async fn upload_file(
        &self,
        upload: &crate::files::FileUpload,
    ) -> Result<crate::files::RemoteFile> {
        let limits = crate::files::provider_limits(PROVIDER_NAME);
        crate::files::upload_openai_shape(&self.endpoint, upload, "user_data", &limits).await
    }

    async fn delete_file(&self, file: &crate::files::RemoteFile) -> Result<()> {
        crate::files::delete_openai_shape(&self.endpoint, file).await
    }

    fn capabilities(&self, model: &str) -> ModelCapabilities {
        let base = self.learned.corrected(model, table_capabilities(model));
        match self.capability_overrides.get(model) {
            Some(over) => over.apply_to(base),
            None => base,
        }
    }

    fn mime(&self, model: &str) -> crate::capabilities::ModelMime {
        let base = crate::mime_tables::meta(model);
        let mime = match self.capability_overrides.get(model) {
            Some(over) => over.apply_mime(base),
            None => base,
        };
        match crate::mime_tables::is_media_model(PROVIDER_NAME, model) {
            true => mime,
            false => crate::mime::WireShape::ResponsesAv.carried(mime),
        }
    }

    fn learned_models(&self) -> Option<&LearnedModels> {
        Some(&self.learned)
    }

    /// Read the listing: which models this key can reach, and when each was
    /// added. The windows and prices it does not carry come from the tables.
    async fn prime_capabilities(&self) -> Result<()> {
        let body = self.endpoint.listing("/models").await?;
        let models: HashMap<String, LearnedModel> = body
            .get("data")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                let id = entry.get("id")?.as_str()?.to_string();
                let learned = LearnedModel {
                    // The listing sends `created: 0` for every model (2026-09-17),
                    // which is no date rather than 1970.
                    released: entry
                        .get("created")
                        .and_then(serde_json::Value::as_i64)
                        .filter(|t| *t > 0),
                    pricing: crate::pricing::published_rates(PROVIDER_NAME, &id),
                    ..LearnedModel::default()
                };
                Some((id, learned))
            })
            .collect();
        tracing::debug!(models = models.len(), "read Meta's model listing");
        self.learned.replace(models);
        Ok(())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        if self.learned.is_empty() {
            self.prime_capabilities().await?;
        }
        Ok(self
            .learned
            .to_model_infos(PROVIDER_NAME, |id| self.capabilities(id)))
    }

    async fn check_credential(&self) -> Result<Vec<ModelInfo>> {
        self.prime_capabilities().await?;
        self.list_models().await
    }

    fn serves_model(&self, model_key: &str) -> Option<String> {
        if self.learned.contains(model_key) {
            return Some(model_key.to_string());
        }
        (self.learned.is_empty() && CATALOG.iter().any(|(id, _)| *id == model_key))
            .then(|| model_key.to_string())
    }

    fn served_catalog(&self) -> Option<Vec<String>> {
        self.learned.catalog()
    }

    fn pricing(&self, model: &str) -> Option<crate::ModelPricing> {
        self.capability_overrides
            .get(model)
            .and_then(ModelCapabilityOverride::pricing)
            .or_else(|| crate::pricing::published_rates(PROVIDER_NAME, model))
    }
}

#[cfg(test)]
mod tests;
