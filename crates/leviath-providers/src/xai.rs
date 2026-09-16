//! xAI: Grok models over the Responses API.
//!
//! One provider type serves two registry names. `xai` authenticates with an
//! API key and bills an xAI API balance; `grok` authenticates with a signed-in
//! subscription (see [`crate::grok`]) and bills the plan. Measured against the
//! live API, both reach the same host, the same listings and the same routes,
//! so everything here is shared and only the credential differs.
//!
//! Every request goes to `POST /v1/responses` with `store: false`: xAI keeps a
//! stored response for thirty days, and a stateless request is also what
//! stays usable under a zero data retention arrangement. Reasoning is carried
//! between turns by replaying the encrypted items the route hands back.

pub mod catalog;

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use futures_core::Stream;

use crate::capabilities::{ModelCapabilities, ModelCapabilityOverride};
use crate::learned::LearnedModels;
use crate::provider::{
    InferenceRequest, InferenceResponse, ModelInfo, Provider, RateLimitConfig, Result, StreamChunk,
};
pub use crate::responses::client::Auth;
use crate::responses::client::{Endpoint, api_error};
use crate::responses::{Dialect, request as request_body, stream};

/// The registry name for an API key.
pub const PROVIDER_NAME: &str = "xai";

/// Where the API lives.
pub const DEFAULT_BASE_URL: &str = "https://api.x.ai/v1";

/// The route's rules with an API key. Measured: `max_output_tokens` and
/// `temperature` are both accepted, and the usage block prices the call.
pub const DIALECT: Dialect = Dialect {
    provider: PROVIDER_NAME,
    rejected_parameters: &["max_tokens", "max_completion_tokens"],
    output_cap: true,
    temperature: true,
    verbosity: false,
    reasoning_summary: false,
    reported_cost: true,
    cache_key: true,
};

/// The same route with a subscription's sign-in. The usage block still quotes
/// a list price, but the plan pays, so the call's marginal cost is zero.
pub const GROK_DIALECT: Dialect = Dialect {
    provider: crate::grok::PROVIDER_NAME,
    reported_cost: false,
    ..DIALECT
};

/// Grok over xAI's API.
pub struct XaiProvider {
    endpoint: Endpoint,
    dialect: Dialect,
    capability_overrides: HashMap<String, ModelCapabilityOverride>,
    /// The operator's reasoning effort, sent to the models that take one.
    effort: Option<String>,
    /// Models that refused an effort they were sent, asked without one after.
    effort_refused: crate::provider::ModelMemo,
    /// What the listings said, by canonical id.
    learned: LearnedModels,
    /// Alias to canonical id, from the same listings.
    aliases: Arc<RwLock<HashMap<String, String>>>,
}

impl XaiProvider {
    /// A provider authenticating with `auth`, registered under the name its
    /// credential implies: `xai` for a key, `grok` for a sign-in.
    pub fn new(client: reqwest::Client, auth: Auth) -> Self {
        let dialect = match auth {
            Auth::Key(_) => DIALECT,
            Auth::Signin(_) => GROK_DIALECT,
        };
        Self {
            endpoint: Endpoint::new(client, DEFAULT_BASE_URL, auth),
            dialect,
            capability_overrides: HashMap::new(),
            effort: None,
            effort_refused: Default::default(),
            learned: Default::default(),
            aliases: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Per-model corrections from `[model_capabilities]`.
    #[must_use]
    pub fn with_overrides(mut self, overrides: HashMap<String, ModelCapabilityOverride>) -> Self {
        self.capability_overrides = overrides;
        self
    }

    /// Apply a rate limit.
    #[must_use]
    pub fn with_rate_limit(mut self, config: Option<&RateLimitConfig>) -> Self {
        self.endpoint.set_rate_limit(config);
        self
    }

    /// Point at another host. `None` keeps xAI's.
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

    /// The reasoning effort to send the models that take one. `None` leaves
    /// each model at its own default.
    #[must_use]
    pub fn with_reasoning_effort(mut self, effort: Option<String>) -> Self {
        self.effort = effort.filter(|e| !e.trim().is_empty());
        self
    }

    /// The canonical id `model` names: itself, or the model it is an alias of.
    fn canonical(&self, model: &str) -> String {
        match self.learned.contains(model) {
            true => model.to_string(),
            false => self
                .aliases
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(model)
                .cloned()
                .unwrap_or_else(|| model.to_string()),
        }
    }

    /// Stream one inference, asking a model that refuses a reasoning effort
    /// again without one and remembering that it did.
    async fn stream_inference(
        &self,
        request: &InferenceRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>> {
        let model = self.canonical(&request.model);
        let effort = self
            .effort
            .as_deref()
            .filter(|_| catalog::takes_effort(&model) && !self.effort_refused.contains(&model));
        let mut body = self.body(request, effort);
        let mut response = self.endpoint.post_json("/responses", &body).await?;
        if effort.is_some() && response.status().as_u16() == 400 {
            let text = response.text().await.unwrap_or_default();
            if !refuses_effort(&text) {
                return Err(api_error(400, &text));
            }
            self.effort_refused.insert(&model);
            tracing::info!(model = %model, "xAI refused a reasoning effort for this model; asking without one");
            body = self.body(request, None);
            response = self.endpoint.post_json("/responses", &body).await?;
        }
        let response =
            crate::provider::check_http_response(response, self.endpoint.rate_limiter.as_ref())
                .await?;
        let peer = leviath_net::read_caps::peer_of(&response);
        Ok(crate::rate_limit::meter_stream(
            self.endpoint.rate_limiter.as_ref(),
            Box::pin(stream::sse_stream(response.bytes_stream(), self.dialect).sent_by(peer)),
        ))
    }

    /// The Responses body for `request`.
    fn body(&self, request: &InferenceRequest, effort: Option<&str>) -> serde_json::Value {
        let mut request = request.clone();
        request.model = self.canonical(&request.model);
        let mut body = request_body::build(
            &request,
            &self.dialect,
            &request_body::Settings {
                effort,
                verbosity: "medium",
                replay_reasoning: true,
            },
        );
        let fields = body.as_object_mut().expect("a Responses body is an object");
        if !self.capabilities(&request.model).supports_temperature {
            fields.remove("temperature");
        }
        // A caller's own `reasoning` (the titling lane asks for a low effort)
        // is taken off a model that picks its own depth or has refused one:
        // sent anyway, it is the whole request's 400.
        if effort.is_none()
            && (!catalog::takes_effort(&request.model)
                || self.effort_refused.contains(&request.model))
        {
            fields.remove("reasoning");
            fields.remove("include");
        }
        body
    }
}

/// Whether a 400 body is the route refusing a reasoning effort.
fn refuses_effort(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("reasoning") && (lower.contains("effort") || lower.contains("not supported"))
}

#[async_trait]
impl Provider for XaiProvider {
    async fn infer(&self, request: &InferenceRequest) -> Result<InferenceResponse> {
        // The route streams; a buffered call is the stream collected, so the two
        // paths cannot parse the same answer differently.
        crate::provider::collect_stream(self.stream_inference(request).await?).await
    }

    async fn infer_stream(
        &self,
        request: &InferenceRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>> {
        self.stream_inference(request).await
    }

    async fn count_tokens(&self, text: &str, _model: &str) -> usize {
        leviath_core::estimate_tokens(text)
    }

    fn max_context_tokens(&self, model: &str) -> usize {
        self.capabilities(model).max_context_tokens
    }

    fn name(&self) -> &str {
        self.dialect.provider
    }

    fn capabilities(&self, model: &str) -> ModelCapabilities {
        let id = self.canonical(model);
        let base = self
            .learned
            .corrected(&id, catalog::table_capabilities(&id));
        match self
            .capability_overrides
            .get(model)
            .or_else(|| self.capability_overrides.get(&id))
        {
            Some(over) => over.apply_to(base),
            None => base,
        }
    }

    fn mime(&self, model: &str) -> crate::capabilities::ModelMime {
        let id = self.canonical(model);
        let base = self
            .learned
            .mime_corrected(&id, crate::mime_tables::builtin_mime(PROVIDER_NAME, &id));
        let mime = match self.capability_overrides.get(model) {
            Some(over) => over.apply_mime(base),
            None => base,
        };
        crate::mime::WireShape::Responses.carried(mime)
    }

    fn learned_models(&self) -> Option<&LearnedModels> {
        Some(&self.learned)
    }

    /// Read the four listings. The chat listing is required; the modality and
    /// media listings are best effort, and without them the compiled table
    /// answers for what they would have said.
    async fn prime_capabilities(&self) -> Result<()> {
        let mut listing = catalog::Listing::default();
        let chat = self.endpoint.listing("/models").await?;
        catalog::read_models(&chat, &mut listing);
        match self.endpoint.listing("/language-models").await {
            Ok(body) => catalog::read_modalities(&body, &mut listing),
            Err(e) => tracing::debug!(error = %e, "xAI's language-model listing could not be read"),
        }
        for path in ["/image-generation-models", "/video-generation-models"] {
            match self.endpoint.listing(path).await {
                Ok(body) => catalog::read_media(&body, &mut listing),
                Err(e) => {
                    tracing::debug!(error = %e, path, "an xAI media listing could not be read")
                }
            }
        }
        tracing::debug!(
            provider = self.dialect.provider,
            models = listing.models.len(),
            aliases = listing.aliases.len(),
            "learned xAI model capabilities and rates"
        );
        self.learned.replace(listing.models);
        *self
            .aliases
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = listing.aliases;
        Ok(())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        if self.learned.is_empty() {
            self.prime_capabilities().await?;
        }
        Ok(self
            .learned
            .to_model_infos(self.dialect.provider, |id| self.capabilities(id)))
    }

    /// Read the listings, which the credential has to be good for: a key or a
    /// sign-in that is not is refused there rather than at the first run.
    async fn check_credential(&self) -> Result<Vec<ModelInfo>> {
        self.prime_capabilities().await?;
        self.list_models().await
    }

    fn serves_model(&self, model_key: &str) -> Option<String> {
        let id = self.canonical(model_key);
        if self.learned.contains(&id) {
            return Some(id);
        }
        // Before the listing is read, the compiled table's own names.
        (self.learned.is_empty() && catalog::CATALOG.iter().any(|(known, _)| *known == id))
            .then_some(id)
    }

    fn served_catalog(&self) -> Option<Vec<String>> {
        self.learned.catalog()
    }

    fn pricing(&self, model: &str) -> Option<crate::ModelPricing> {
        if self.endpoint.is_signin() {
            // A subscription is a flat fee: each call's marginal cost is a
            // known zero, not an unknown.
            return Some(crate::ModelPricing::flat(0.0, 0.0));
        }
        let id = self.canonical(model);
        self.capability_overrides
            .get(model)
            .and_then(ModelCapabilityOverride::pricing)
            .or_else(|| self.learned.get(&id).and_then(|m| m.pricing))
            .or_else(|| crate::pricing::published_rates(PROVIDER_NAME, &id))
    }
}

#[cfg(test)]
mod tests;
