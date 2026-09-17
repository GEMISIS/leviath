//! OpenAI provider, on the Responses API.
//!
//! `POST /v1/responses` rather than Chat Completions: it is the route that
//! takes a file by id (an image or a PDF uploaded once and named after), that
//! takes function tools beside a reasoning effort, and that hands a reasoning
//! model's chain of thought back as an encrypted item Leviath can replay on the
//! next turn. `store: false` is always sent, so OpenAI keeps no response.
//!
//! A stage's `[model.parameters]` written for Chat Completions keep working:
//! `reasoning_effort` becomes `reasoning.effort`, and `response_format` becomes
//! `text.format`.
//!
//! The image, video, speech and transcription models are served too, each on
//! its own route (see `media`).

pub(crate) mod media;

use crate::capabilities::{Match, Row};
use crate::learned::{LearnedModel, LearnedModels};
use crate::provider::{
    InferenceRequest, InferenceResponse, ModelCapabilities, ModelCapabilityOverride, ModelInfo,
    Provider, ProviderError, Result, StreamChunk,
};
use crate::responses::client::{Auth, Endpoint};
use crate::responses::{Dialect, request as request_body, stream};
use async_trait::async_trait;
use futures_core::Stream;
use std::collections::HashMap;
use std::pin::Pin;

/// The registry name.
pub const PROVIDER_NAME: &str = "openai";

/// Where the API lives.
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// The route's rules: an output cap and a temperature are taken; the Chat
/// Completions names for the cap, and `n`, are not.
pub const DIALECT: Dialect = Dialect {
    provider: PROVIDER_NAME,
    rejected_parameters: &["max_tokens", "max_completion_tokens", "n"],
    output_cap: true,
    temperature: true,
    verbosity: false,
    reasoning_summary: false,
    reported_cost: false,
    cache_key: true,
};

/// OpenAI provider.
pub struct OpenAIProvider {
    /// The name it is registered under: `openai`, or the name a
    /// `[model_providers.<name>]` entry gave a second host.
    name: String,

    /// The host, the key, the operator's headers and the rate limit.
    endpoint: Endpoint,

    /// Ids a bare model name may route here on beyond OpenAI's own shapes:
    /// the deployment names an Azure resource serves.
    serves: Vec<String>,

    /// Per-model capability overrides
    capability_overrides: HashMap<String, ModelCapabilityOverride>,

    /// Models the API has refused a temperature for, so the next request to one
    /// omits it instead of spending a round trip learning the same thing again.
    temperature_unsupported: crate::provider::ModelMemo,
    /// How often a video job is polled.
    poll_interval: std::time::Duration,
    /// What `GET /v1/models` said, filled by [`Provider::prime_capabilities`].
    ///
    /// Ids and dates only: see that method for what the listing cannot say.
    /// Empty until primed, and empty for good if the endpoint could not be
    /// reached, in which case the compiled table answers everything.
    learned: LearnedModels,
}

/// Whether `model_key` is shaped like one of OpenAI's chat or reasoning
/// models: `gpt-*`, or `o<digit>*`, and not one of the `gpt-*` names that
/// speak a different API.
///
/// One rule for two questions. [`Provider::serves_model`] uses it to route a
/// bare model name, and [`Provider::served_catalog`] uses it to keep the
/// embeddings, transcription and image models the listing also carries from
/// being published as chat models. Sharing it is what guarantees the catalogue
/// can never refuse a name routing would have accepted.
///
/// The exclusions are the `gpt-` families measured in the live listing that
/// do not answer a chat request: realtime and transcription models speak
/// their own endpoints, and image and speech models produce no text. The
/// listing itself says nothing about which endpoint a model speaks, so the
/// name is the only signal.
fn is_chat_model_id(model_key: &str) -> bool {
    const NOT_CHAT: &[&str] = &[
        "realtime",
        "transcribe",
        "audio",
        "tts",
        "image",
        "search-api",
    ];
    (model_key.starts_with("gpt") || is_o_series(model_key))
        && !NOT_CHAT.iter().any(|s| model_key.contains(s))
}

/// Whether this provider runs `model_key`: a chat or reasoning model, or an
/// image, video, speech or transcription model on its own route.
fn serves_id(model_key: &str) -> bool {
    is_chat_model_id(model_key) || media::kind(model_key).is_some()
}

/// `o` followed by a digit: the reasoning line.
fn is_o_series(model_key: &str) -> bool {
    model_key.starts_with('o')
        && model_key
            .get(1..2)
            .is_some_and(|c| c.chars().all(|c| c.is_ascii_digit()))
}

/// Whether `model` reasons, and so hands back an encrypted chain of thought
/// worth replaying: the o-series and the GPT-5 family, but not its `chat`
/// variants, which do not reason.
fn reasons(model: &str) -> bool {
    (is_o_series(model) || model.starts_with("gpt-5")) && !model.contains("chat")
}

/// What [`MODELS`] says about `model`, for a caller with no provider in hand.
pub(crate) fn table_capabilities(model: &str) -> ModelCapabilities {
    crate::capabilities::lookup(MODELS, model, ModelCapabilities::default())
}

/// The models this build names when the listing cannot be read, as
/// `(id, display name)`.
pub(crate) const CATALOG: &[(&str, &str)] = &[
    ("gpt-5.5", "GPT-5.5"),
    ("gpt-5.4", "GPT-5.4"),
    ("gpt-5.4-mini", "GPT-5.4 Mini"),
    ("gpt-5.4-nano", "GPT-5.4 Nano"),
];

/// What this build knows about OpenAI's models, most specific first.
///
/// `gpt-5.5` sits above the `gpt-5` family row because it is the one member
/// that refuses a temperature (verified against the API: it takes only its
/// default and rejects any other value outright), and `gpt-4.1` above the
/// implicit `gpt-4` default because its window is eight times larger.
///
/// The 1,050,000-token models (5.4, 5.5, 5.6) are sized at 922,000: on the
/// Responses API the prompt and the reply share that budget, whatever the
/// model's nominal window, so a region sized past it is a refused request.
/// Azure publishes the same numbers for its deployments.
pub(crate) const MODELS: &[Row] = &[
    // The media models answer with parts, not tokens, and call no tools. Ahead
    // of the families below, whose prefixes some of them share.
    Row {
        matches: &[
            Match::Prefix("gpt-image"),
            Match::Prefix("chatgpt-image"),
            Match::Prefix("sora"),
            Match::Prefix("tts-"),
            Match::Contains("-tts"),
            Match::Prefix("whisper"),
            Match::Contains("transcribe"),
        ],
        temperature: false,
        tools: false,
        context: 32_000,
        output: 4_096,
    },
    Row {
        matches: &[Match::Prefix("gpt-5.5")],
        temperature: false,
        tools: true,
        context: 922_000,
        output: 128_000,
    },
    // The small 5.4 models keep the family window; the full-size ones below
    // do not, so these have to match first.
    Row {
        matches: &[Match::Prefix("gpt-5.4-mini"), Match::Prefix("gpt-5.4-nano")],
        temperature: true,
        tools: true,
        context: 272_000,
        output: 128_000,
    },
    Row {
        matches: &[Match::Prefix("gpt-5.4")],
        temperature: true,
        tools: true,
        context: 922_000,
        output: 128_000,
    },
    // Its own row rather than the family's: the 5.6 models carry a window
    // more than three times the family default, and without this they fell
    // through to it and every percentage region budget was sized for a
    // fraction of what the run had.
    Row {
        matches: &[Match::Prefix("gpt-5.6")],
        temperature: true,
        tools: true,
        context: 922_000,
        output: 128_000,
    },
    // The rest of the GPT-5 family (5-mini and earlier). 272,000 is the
    // family's published input window.
    Row {
        matches: &[Match::Prefix("gpt-5")],
        temperature: true,
        tools: true,
        context: 272_000,
        output: 128_000,
    },
    Row {
        matches: &[Match::Prefix("gpt-4.1")],
        temperature: true,
        tools: true,
        context: 1_047_576,
        output: 32_768,
    },
    // o-series reasoning models: no temperature.
    Row {
        matches: &[Match::Prefix("o3"), Match::Prefix("o4")],
        temperature: false,
        tools: true,
        context: 200_000,
        output: 100_000,
    },
];

impl OpenAIProvider {
    /// Create a new OpenAI provider.
    pub fn new(client: reqwest::Client, api_key: String) -> Self {
        Self {
            name: PROVIDER_NAME.to_string(),
            endpoint: Endpoint::new(client, DEFAULT_BASE_URL, Auth::Key(api_key)),
            serves: Vec::new(),
            capability_overrides: HashMap::new(),
            temperature_unsupported: Default::default(),
            learned: Default::default(),
            poll_interval: std::time::Duration::from_secs(10),
        }
    }

    /// How often a video job is polled. Tests shorten it.
    #[must_use]
    pub fn with_poll_interval(mut self, interval: std::time::Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Run a media model: images, video, speech or transcription.
    async fn run_media(
        &self,
        kind: media::Kind,
        request: &InferenceRequest,
    ) -> Result<InferenceResponse> {
        let rates = self.pricing(&request.model);
        let billing = media::Billing {
            unit: rates.and_then(|p| p.unit),
            // A table row priced by the token always has an output rate; a
            // row priced by the unit alone has none.
            tokens: rates.filter(|p| p.output_per_mtok > 0.0),
        };
        media::run(&self.endpoint, kind, request, &billing, self.poll_interval).await
    }

    /// Create a new OpenAI provider with per-model capability overrides.
    pub fn with_overrides(
        client: reqwest::Client,
        api_key: String,
        overrides: HashMap<String, ModelCapabilityOverride>,
        rate_limit: Option<&crate::provider::RateLimitConfig>,
    ) -> Self {
        let mut provider = Self::new(client, api_key);
        provider.capability_overrides = overrides;
        provider.endpoint.set_rate_limit(rate_limit);
        provider
    }

    /// Extra headers on every request to the host, after the provider's own:
    /// what a gateway named in `with_base_url` wants of its own.
    pub fn with_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.endpoint.extra_headers = headers;
        self
    }

    /// Register under `name` rather than `openai`: a second host of OpenAI's
    /// API, with its own key, beside the first. The vendor tables, prices
    /// and dialect stay OpenAI's; only what the registry and the listing call
    /// it changes.
    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Send the key in `header` instead of as a bearer token. `None` keeps
    /// the bearer.
    pub fn with_auth_header(mut self, header: Option<String>) -> Self {
        self.endpoint.auth_header = header;
        self
    }

    /// Route these ids here too: deployment names that do not look like
    /// OpenAI's own model ids.
    pub fn with_serves(mut self, serves: Vec<String>) -> Self {
        self.serves = serves;
        self
    }

    /// Point this provider at a different host: an enterprise gateway or a
    /// self-hosted proxy speaking the same API. `None` keeps the built-in
    /// default, so a config that says nothing sends exactly what it did.
    pub fn with_base_url(mut self, base_url: Option<String>) -> Self {
        self.endpoint.set_base_url(base_url);
        self
    }

    /// Return built-in capability defaults for a model.
    fn builtin_capabilities(&self, model: &str) -> ModelCapabilities {
        table_capabilities(model)
    }

    /// The Responses body for `request`, with the Chat Completions names a
    /// stage may still use translated and a refused temperature left out.
    fn body(&self, request: &InferenceRequest) -> serde_json::Value {
        let replay = reasons(&request.model);
        let mut body = request_body::build(
            request,
            &DIALECT,
            &request_body::Settings {
                effort: None,
                verbosity: "medium",
                replay_reasoning: replay,
            },
        );
        let fields = body.as_object_mut().expect("a Responses body is an object");
        if let Some(effort) = fields.remove("reasoning_effort") {
            let reasoning = fields
                .entry("reasoning")
                .or_insert_with(|| serde_json::json!({}));
            if let Some(reasoning) = reasoning.as_object_mut() {
                reasoning.entry("effort").or_insert(effort);
            }
        }
        if let Some(format) = fields.remove("response_format") {
            let text = fields
                .entry("text")
                .or_insert_with(|| serde_json::json!({}));
            if let Some(text) = text.as_object_mut() {
                text.entry("format").or_insert_with(|| text_format(format));
            }
        }
        // A reasoning model hands its chain of thought back only when asked,
        // and it is only worth asking when the next turn replays it.
        if replay {
            fields
                .entry("include")
                .or_insert_with(|| serde_json::json!(["reasoning.encrypted_content"]));
        }
        // A model that takes no temperature is sent none, rather than zero:
        // the o-series accepts only its default and rejects `0.0` as firmly as
        // `0.7`.
        if !self.capabilities(&request.model).supports_temperature {
            fields.remove("temperature");
        }
        body
    }

    /// POST `body` to `/responses` with the request's own deadline, and the
    /// response when it succeeded.
    async fn post(
        &self,
        request: &InferenceRequest,
        body: &serde_json::Value,
    ) -> Result<reqwest::Response> {
        let url = self.endpoint.url("/responses");
        let timeout = request.request_timeout_secs;
        let response = self
            .endpoint
            .send(|client| {
                crate::provider::apply_request_timeout(client.post(&url).json(body), timeout)
            })
            .await?;
        crate::provider::check_http_response(response, self.endpoint.rate_limiter.as_ref()).await
    }

    /// Whether this model has already refused a temperature.
    fn temperature_is_unsupported(&self, model: &str) -> bool {
        self.temperature_unsupported.contains(model)
    }
}

/// A Chat Completions `response_format` as a Responses `text.format`: the
/// `json_schema` object's fields move up a level; every other shape is the
/// same.
fn text_format(format: serde_json::Value) -> serde_json::Value {
    match (
        format.get("type").and_then(|t| t.as_str()),
        format.get("json_schema"),
    ) {
        (Some("json_schema"), Some(serde_json::Value::Object(schema))) => {
            let mut out = schema.clone();
            out.insert("type".to_string(), serde_json::json!("json_schema"));
            serde_json::Value::Object(out)
        }
        _ => format,
    }
}

#[async_trait]
impl Provider for OpenAIProvider {
    async fn infer(&self, request: &InferenceRequest) -> Result<InferenceResponse> {
        if let Some(kind) = media::kind(&request.model) {
            return self.run_media(kind, request).await;
        }
        // The route streams; a buffered call is the stream collected, so the
        // two paths cannot read the same answer differently.
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
        tracing::debug!(model = %request.model, "Calling OpenAI API");
        let mut body = self.body(request);
        let response = match self.post(request, &body).await {
            Err(ProviderError::ApiError(detail))
                if body.get("temperature").is_some()
                    && crate::openai_compat::temperature_refused(&detail) =>
            {
                tracing::debug!(
                    model = %request.model,
                    "OpenAI refused the temperature we sent; retrying without it"
                );
                self.temperature_unsupported.insert(&request.model);
                crate::provider::drop_temperature(&mut body);
                self.post(request, &body).await?
            }
            other => other?,
        };
        let peer = leviath_net::read_caps::peer_of(&response);
        Ok(crate::rate_limit::meter_stream(
            self.endpoint.rate_limiter.as_ref(),
            Box::pin(stream::sse_stream(response.bytes_stream(), DIALECT).sent_by(peer)),
        ))
    }

    async fn count_tokens(&self, text: &str, model: &str) -> usize {
        // tiktoken is exact for OpenAI models and runs locally - no network
        // call. Local is not free, though: BPE over a megabyte of prompt is
        // tens of milliseconds of CPU, and this runs on the runtime's worker
        // threads, where that long a stretch without a yield stalls every
        // other lane. Above the threshold it moves to a blocking thread.
        if text.len() <= TIKTOKEN_INLINE_BYTES {
            return crate::tokenizer::count_tokens(text, model);
        }
        let (text, model) = (text.to_string(), model.to_string());
        tokio::task::spawn_blocking(move || crate::tokenizer::count_tokens(&text, &model))
            .await
            .expect("tiktoken does not panic on any input")
    }

    fn max_context_tokens(&self, model: &str) -> usize {
        self.capabilities(model).max_context_tokens
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn serves_model(&self, model_key: &str) -> Option<String> {
        // OpenAI's chat models are `gpt-*`, and its reasoning line is `o1`/`o3`
        // and successors. See the note on the Gemini provider for why the
        // capability table is the wrong thing to ask.
        (serves_id(model_key)
            || self.capability_overrides.contains_key(model_key)
            || self.serves.iter().any(|id| id == model_key))
        .then(|| model_key.to_string())
    }

    fn pricing(&self, model: &str) -> Option<crate::ModelPricing> {
        // Config first: it is the only source that can know a negotiated rate,
        // and the shipped table is a transcription of a public page that may
        // have moved since this build.
        self.capability_overrides
            .get(model)
            .and_then(|o| o.pricing())
            .or_else(|| crate::pricing::published_rates(PROVIDER_NAME, model))
    }

    fn learned_models(&self) -> Option<&crate::learned::LearnedModels> {
        Some(&self.learned)
    }

    fn capabilities(&self, model: &str) -> ModelCapabilities {
        // The listing says nothing about size or shape (see
        // `prime_capabilities`), so the table is the base and the operator's
        // entry is merged onto it, not swapped in: an entry names only what
        // it corrects.
        let mut caps = match self.capability_overrides.get(model) {
            Some(o) => o.apply_to(self.builtin_capabilities(model)),
            None => self.builtin_capabilities(model),
        };
        // A refusal the API has already sent outranks every other source,
        // the operator's entry included: the request was made and the answer
        // was no, and the runtime reads this flag to decide whether to resolve
        // a temperature at all.
        if self.temperature_is_unsupported(model) {
            caps.supports_temperature = false;
        }
        caps
    }

    fn mime(&self, model: &str) -> crate::capabilities::ModelMime {
        // The listing says nothing about mime either, so the table answers
        // and the operator's entry corrects it.
        let base = crate::mime_tables::openai(model);
        let mime = match self.capability_overrides.get(model) {
            Some(o) => o.apply_mime(base),
            None => base,
        };
        // A media model's own route takes what it takes; only a chat model is
        // narrowed to what a Responses body carries.
        match media::kind(model) {
            Some(_) => mime,
            None => crate::mime::WireShape::Responses.carried(mime),
        }
    }

    /// Images and PDFs by file id for a chat model. A media model's route
    /// takes its inputs in the request and names no file.
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

    /// The chat and reasoning models the listing named, once primed.
    ///
    /// Filtered through `serves_id` because `GET /v1/models` also carries
    /// embeddings and realtime models (130 entries against a few dozen this
    /// provider can drive, measured), and a complete catalogue that named
    /// `text-embedding-3-large` would let a blueprint route a stage to it.
    /// The same rule routes a bare name, so nothing routing accepts is
    /// refused here, and a named host's own deployments follow.
    fn served_catalog(&self) -> Option<Vec<String>> {
        self.learned.catalog().map(|ids| {
            ids.into_iter()
                .filter(|id| serves_id(id))
                .chain(self.serves.iter().cloned())
                .collect()
        })
    }

    /// Read `GET /v1/models` into `Self::learned`.
    ///
    /// What the listing fills, measured against the live endpoint: the id,
    /// `created` and `shutdown_date`. It carries no context window, no output
    /// cap, no display name and nothing about temperature or tools, which is
    /// why every one of those stays `None` here and the compiled table plus
    /// the temperature-refusal memo remain the sources for them. This is the
    /// one provider whose listing says nothing about size.
    async fn prime_capabilities(&self) -> Result<()> {
        let body = self.fetch_models_json().await?;
        let learned: HashMap<String, LearnedModel> = body
            .get("data")
            .and_then(|d| d.as_array())
            .ok_or_else(|| {
                ProviderError::InvalidResponse("No data field in models response".to_string())
            })?
            .iter()
            .filter_map(|item| {
                let id = item.get("id")?.as_str()?.to_string();
                Some((
                    id,
                    LearnedModel {
                        released: item.get("created").and_then(|v| v.as_i64()),
                        retires: item
                            .get("shutdown_date")
                            .and_then(|v| v.as_str())
                            .map(str::to_string),
                        ..Default::default()
                    },
                ))
            })
            .collect();
        let count = learned.len();
        self.learned.replace(learned);
        tracing::debug!(provider = %self.name, models = count, "learned OpenAI model ids and dates");
        Ok(())
    }

    /// The chat and reasoning models, answered from `Self::learned`.
    ///
    /// Primes first when nothing has been learned yet, so this is the one
    /// fetch. Filtered the way [`Self::served_catalog`] is, for the same
    /// reason: a picker offering `whisper-1` as a chat model is a trap.
    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        if self.learned.is_empty() {
            self.prime_capabilities().await?;
        }
        Ok(self
            .learned
            .to_model_infos(&self.name, |id| self.capabilities(id))
            .into_iter()
            .filter(|m| serves_id(&m.id))
            .collect())
    }
}

/// The largest text tiktoken is run on inline, on the async thread that asked.
///
/// Below this the encode finishes in well under a millisecond and a thread
/// hop would cost more than it saves; above it the count is a real stretch of
/// CPU and goes to a blocking thread.
const TIKTOKEN_INLINE_BYTES: usize = 256 * 1024;

impl OpenAIProvider {
    /// GET `/models`, as the endpoint answers it.
    async fn fetch_models_json(&self) -> Result<serde_json::Value> {
        let url = self.endpoint.url("/models");
        let response = self
            .endpoint
            .send(|client| {
                crate::provider::apply_request_timeout(
                    client.get(&url),
                    Some(crate::provider::SIDE_CALL_TIMEOUT_SECS),
                )
            })
            .await?;

        let status = response.status();
        if !status.is_success() {
            let error_body = leviath_net::read_caps::read_text_capped(
                response,
                leviath_net::read_caps::JSON_BODY_CAP,
            )
            .await
            .unwrap_or_else(|_| "unknown error".to_string());
            return Err(ProviderError::ApiError(format!(
                "HTTP {}: {}",
                status, error_body
            )));
        }

        crate::provider::decode_json(response).await
    }
}

#[cfg(test)]
mod mime_tests;

#[cfg(test)]
mod tests;
