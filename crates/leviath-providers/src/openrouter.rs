//! OpenRouter provider implementation.
//!
//! OpenRouter provides access to multiple models through a unified API.
//! Uses OpenAI-compatible format with additional headers.

mod catalog;

use crate::capabilities::{Match, Row};
use crate::learned::LearnedModels;
use crate::openai_compat::{
    openai_sse_stream, parse_openai_response, send_chat_request, temperature_refused,
};
use crate::provider::{
    InferenceRequest, InferenceResponse, LimitsSource, ModelCapabilities, ModelCapabilityOverride,
    ModelInfo, Provider, ProviderError, Result, StreamChunk,
};
use crate::rate_limit::RateLimiter;
use async_trait::async_trait;
use catalog::parse_entry;
use futures_core::Stream;
use std::collections::HashMap;
use std::pin::Pin;

/// OpenRouter provider.
pub struct OpenRouterProvider {
    /// HTTP client
    client: reqwest::Client,

    /// API key
    api_key: String,

    /// API base URL
    base_url: String,

    /// Rate limiter
    rate_limiter: Option<RateLimiter>,

    /// Per-model capability overrides
    capability_overrides: HashMap<String, ModelCapabilityOverride>,

    /// Models already reported as falling back, so the warning is once per
    /// model per process rather than once per inference.
    warned_unknown: crate::provider::ModelMemo,

    /// Models the gateway has refused a temperature for.
    ///
    /// Per instance rather than a table: OpenRouter fronts every vendor, so no
    /// static list can stay right about which of their models take one, and
    /// this build's table already names only a few dozen of hundreds.
    temperature_unsupported: crate::provider::ModelMemo,

    /// What OpenRouter's own `/models` endpoint says about each model, filled
    /// once by [`Provider::prime_capabilities`].
    ///
    /// OpenRouter fronts hundreds of models and this build's table names a few
    /// dozen, so the table is out of date the day it ships and an unlisted
    /// model silently got a conservative 128 000 tokens. Region budgets are
    /// percentages of the window, so that sized a `budget = "30%"` region on a
    /// 1M-token model at 38 400 instead of 314 572 (#337, #360). The same
    /// listing says whether a model takes a temperature or tools, what it
    /// charges, and whether its upstream bills a cache write (#568); all of it
    /// is kept now, and [`Self::capabilities`] answers from it.
    ///
    /// Empty until primed, and empty forever if the endpoint could not be
    /// reached - both mean "fall back to the built-in table", which is what
    /// happened before this existed.
    learned: LearnedModels,
}

impl OpenRouterProvider {
    /// Whether to send this model explicit `cache_control` markers.
    ///
    /// The listing's cache-write price decides once priming has read it: an
    /// upstream that bills a write expects a marker, and one that quotes only
    /// a read price caches by prefix on its own and may charge extra for the
    /// marker (see [`crate::learned::LearnedModel::explicit_cache_control`]).
    /// Before priming, or for a model the listing omits, the old rule stands:
    /// Anthropic and Google read markers and everyone else is sent plain
    /// text so an unknown field cannot be refused.
    fn explicit_cache_control(&self, model: &str) -> bool {
        self.learned
            .get(model)
            .and_then(|m| m.explicit_cache_control)
            .unwrap_or_else(|| model.contains("claude") || model.contains("gemini"))
    }

    /// Create a new OpenRouter provider.
    pub fn new(client: reqwest::Client, api_key: String) -> Self {
        Self {
            client,
            api_key,
            base_url: "https://openrouter.ai/api/v1".to_string(),
            rate_limiter: None,
            capability_overrides: HashMap::new(),
            warned_unknown: Default::default(),
            temperature_unsupported: Default::default(),
            learned: Default::default(),
        }
    }

    /// Create a new OpenRouter provider with per-model capability overrides.
    pub fn with_overrides(
        client: reqwest::Client,
        api_key: String,
        overrides: HashMap<String, ModelCapabilityOverride>,
        rate_limit: Option<&crate::provider::RateLimitConfig>,
    ) -> Self {
        Self {
            client,
            api_key,
            base_url: "https://openrouter.ai/api/v1".to_string(),
            rate_limiter: rate_limit.map(crate::rate_limit::RateLimiter::new),
            capability_overrides: overrides,
            warned_unknown: Default::default(),
            temperature_unsupported: Default::default(),
            learned: Default::default(),
        }
    }

    /// Point this provider at a different host.
    ///
    /// An enterprise gateway or self-hosted proxy speaks the same API on a
    /// different origin, and every part of that was already here - the struct
    /// holds a `base_url`, and the constructors honour one - except a way for
    /// configuration to reach the constructor the registry actually calls.
    /// a `base_url` constructor would drop the capability overrides;
    /// `with_overrides` does the reverse, and the registry needs the overrides,
    /// so the URL was the half that got lost.
    ///
    /// A builder rather than a fifth constructor parameter, following
    /// `with_cache_ttl`: one field that four providers each gained does not
    /// need to widen three constructors apiece.
    ///
    /// `None` keeps the built-in default, so a config that says nothing is
    /// byte-for-byte the request it was before.
    pub fn with_base_url(mut self, base_url: Option<String>) -> Self {
        if let Some(url) = base_url {
            self.base_url = url;
        }
        self
    }

    /// Build the request body (OpenAI-compatible format).
    ///
    /// For Anthropic models (detected by `claude` in name), pass through
    /// cache breakpoint markers as content-block cache_control annotations.
    ///
    /// The system blocks matter more than the conversation does. They are the
    /// stable prefix - the stage prompt and the pinned context regions - and
    /// they were being sent as plain strings, so nothing marked them and an
    /// Anthropic model reached this way cached nothing at all. Two measured
    /// research runs read 0 tokens from cache across 2.9M and 5.9M input
    /// tokens; the same request with the system block marked costs a ninth on
    /// its second call. DeepSeek hid this, because it caches server-side with
    /// no markers at all and so reported hits regardless.
    ///
    /// The choice of which blocks to mark is [`crate::anthropic`]'s, not a
    /// second implementation: a marker on content that changes every turn
    /// writes an entry that can never be read back, and that logic already
    /// exists and is tested.
    fn build_request_body(&self, request: &InferenceRequest) -> serde_json::Value {
        let is_anthropic = self.explicit_cache_control(&request.model);
        // Anthropic allows four `cache_control` markers per request across
        // system and messages together. System takes its claim first and the
        // conversation gets the rest, matching the direct provider. Gemini
        // reads the same markers through OpenRouter and has the same limit.
        let system_breakpoints: std::collections::HashSet<usize> = match is_anthropic {
            true => crate::anthropic::system_cache_breakpoints(
                &request.system,
                crate::anthropic::MAX_SYSTEM_BREAKPOINTS,
            )
            .into_iter()
            .collect(),
            false => std::collections::HashSet::new(),
        };
        let message_budget = 4usize.saturating_sub(system_breakpoints.len());
        let mut breakpoint_count = 0usize;

        let mut messages: Vec<serde_json::Value> = Vec::new();
        // System blocks go first; dropping them silently loses the system prompt.
        for (index, block) in request.system.iter().enumerate() {
            match system_breakpoints.contains(&index) {
                true => messages.push(serde_json::json!({
                    "role": "system",
                    "content": [{
                        "type": "text",
                        "text": block.text,
                        "cache_control": { "type": "ephemeral" }
                    }],
                })),
                false => {
                    messages.push(serde_json::json!({ "role": "system", "content": block.text }))
                }
            }
        }
        for msg in &request.messages {
            match &msg.content {
                // A cache-breakpointed text turn (Anthropic-via-OpenRouter) keeps
                // its ephemeral cache_control wrapper.
                crate::provider::MessageContent::Text(text)
                    if is_anthropic
                        && msg.cache_breakpoint
                        && breakpoint_count < message_budget =>
                {
                    breakpoint_count += 1;
                    messages.push(serde_json::json!({
                        "role": msg.role,
                        "content": [{
                            "type": "text",
                            "text": text,
                            "cache_control": { "type": "ephemeral" }
                        }],
                    }));
                }
                // Everything else - including tool_use/tool_result block history -
                // goes through the OpenAI-format conversion so it round-trips on
                // non-Anthropic models instead of being sent as raw block JSON.
                _ => messages.extend(crate::openai_compat::message_to_openai(
                    &msg.role,
                    &msg.content,
                )),
            }
        }

        let caps = self.capabilities(&request.model);
        let mut body = if caps.supports_temperature {
            serde_json::json!({
                "model": request.model,
                "max_tokens": request.max_tokens,
                "temperature": crate::provider::json_number(request.temperature),
                "messages": messages,
            })
        } else {
            serde_json::json!({
                "model": request.model,
                "max_tokens": request.max_tokens,
                "messages": messages,
            })
        };

        // Ask the gateway to report what the call cost. Without this it returns
        // token counts only and the cost has to be reconstructed from a rate
        // card, which is an estimate: it cannot see this account's actual
        // rates, the gateway's margin, or a request rerouted to a different
        // backend at a different price. With it, `usage.cost` comes back as the
        // real figure and nothing downstream has to guess.
        body["usage"] = serde_json::json!({ "include": true });

        if !request.tools.is_empty() {
            body["tools"] = crate::openai_compat::tools_array(&request.tools);
        }

        // Pass through extra model parameters (top_p, stop, seed, …).
        crate::openai_compat::merge_extra_params(
            body.as_object_mut()
                .expect("an OpenRouter request body is always a JSON object"),
            &request.extra,
        );
        body
    }
}

/// OpenRouter's own answer for a model, before any `[model_capabilities]`
/// entry is merged onto it.
///
/// A free function rather than a method: it reads nothing from the provider,
/// and lifting it out is what lets `capabilities` merge an override onto it
/// instead of replacing it wholesale.
fn builtin_capabilities(model: &str) -> ModelCapabilities {
    crate::capabilities::lookup(MODELS, model, FALLBACK_CAPABILITIES)
}

/// [`builtin_capabilities`], for the crate-level catalogue.
pub(crate) fn table_capabilities(model: &str) -> ModelCapabilities {
    builtin_capabilities(model)
}

/// A sample of the models the gateway fronts, named when its listing cannot
/// be read, as `(id, display name)`. The listing is the real answer: it
/// carried 398 models when measured, and this names two dozen.
pub(crate) const CATALOG: &[(&str, &str)] = &[
    ("x-ai/grok-4.6", "Grok 4.6"),
    ("meta/muse-spark-1.2", "Muse Spark 1.2"),
    ("anthropic/claude-opus-5", "Claude Opus 5 (via OpenRouter)"),
    (
        "anthropic/claude-sonnet-5",
        "Claude Sonnet 5 (via OpenRouter)",
    ),
    ("google/gemini-3.5-flash", "Gemini 3.5 Flash"),
    ("google/gemini-2.5-pro", "Gemini 2.5 Pro"),
    ("google/gemini-2.5-flash", "Gemini 2.5 Flash"),
    ("google/gemini-2.5-flash-lite", "Gemini 2.5 Flash Lite"),
    ("meta-llama/llama-4-maverick", "Llama 4 Maverick"),
    ("meta-llama/llama-4-scout", "Llama 4 Scout"),
    ("deepseek/deepseek-v4-pro", "DeepSeek V4 Pro"),
    ("deepseek/deepseek-v4-flash", "DeepSeek V4 Flash"),
    ("deepseek/deepseek-v3.2", "DeepSeek V3.2"),
    ("deepseek/deepseek-r1-0528", "DeepSeek R1 (0528)"),
    ("deepseek/deepseek-r1", "DeepSeek R1"),
    ("mistralai/mistral-large-2512", "Mistral Large 3"),
    ("mistralai/mistral-medium-3-5", "Mistral Medium 3.5"),
    ("mistralai/mistral-small-2603", "Mistral Small 4"),
    ("qwen/qwen3.6-plus", "Qwen 3.6 Plus"),
    ("qwen/qwen3-max", "Qwen3 Max"),
    ("qwen/qwen3-coder", "Qwen3 Coder 480B"),
];

/// What this build knows about the models OpenRouter routes to, most
/// specific first.
///
/// Rows for a single model (`llama-4-scout`, `deepseek-v4-pro`, the
/// Anthropic models that refuse a temperature) sit above their family rows.
pub(crate) const MODELS: &[Row] = &[
    // xAI Grok.
    Row {
        matches: &[Match::Prefix("x-ai/grok-4")],
        temperature: true,
        tools: true,
        context: 500_000,
        output: 64_000,
    },
    // Meta Muse.
    Row {
        matches: &[Match::Prefix("meta/muse")],
        temperature: true,
        tools: true,
        context: 1_048_576,
        output: 64_000,
    },
    // Google Gemini.
    Row {
        matches: &[Match::Prefix("google/gemini")],
        temperature: true,
        tools: true,
        context: 1_048_576,
        output: 65_536,
    },
    // Meta Llama 4 Scout: 10M context.
    Row {
        matches: &[Match::Contains("llama-4-scout")],
        temperature: true,
        tools: true,
        context: 10_000_000,
        output: 32_768,
    },
    // Meta Llama 4 (Maverick and others): 1M context.
    Row {
        matches: &[Match::Prefix("meta-llama/llama-4")],
        temperature: true,
        tools: true,
        context: 1_048_576,
        output: 32_768,
    },
    // DeepSeek R1: reasoning-only, no tools, no temperature.
    Row {
        matches: &[Match::Contains("deepseek-r1")],
        temperature: false,
        tools: false,
        context: 163_840,
        output: 32_768,
    },
    // DeepSeek V4 Pro: 1M context, 384K output.
    Row {
        matches: &[Match::Contains("deepseek-v4-pro")],
        temperature: true,
        tools: true,
        context: 1_048_576,
        output: 393_216,
    },
    // DeepSeek V4 Flash / V3.x.
    Row {
        matches: &[Match::Prefix("deepseek/deepseek-v")],
        temperature: true,
        tools: true,
        context: 1_048_576,
        output: 65_536,
    },
    // Mistral Large.
    Row {
        matches: &[Match::Contains("mistral-large")],
        temperature: true,
        tools: true,
        context: 262_144,
        output: 32_768,
    },
    // Mistral Medium / Small.
    Row {
        matches: &[Match::Prefix("mistralai/")],
        temperature: true,
        tools: true,
        context: 131_072,
        output: 32_768,
    },
    // Qwen 3.6+ / Qwen3 Coder: 1M context.
    Row {
        matches: &[Match::Contains("qwen3.6"), Match::Contains("qwen3-coder")],
        temperature: true,
        tools: true,
        context: 1_048_576,
        output: 65_536,
    },
    // Qwen3 general.
    Row {
        matches: &[Match::Prefix("qwen/")],
        temperature: true,
        tools: true,
        context: 131_072,
        output: 32_768,
    },
    // Anthropic models via OpenRouter inherit the direct provider's flags:
    // these four refuse a temperature, the rest of the family takes one.
    Row {
        matches: &[
            Match::PrefixAnd("anthropic/", "claude-opus-4-8"),
            Match::PrefixAnd("anthropic/", "claude-opus-4-7"),
            Match::PrefixAnd("anthropic/", "claude-fable-5"),
            Match::PrefixAnd("anthropic/", "claude-mythos-5"),
        ],
        temperature: false,
        tools: true,
        context: 1_000_000,
        output: 128_000,
    },
    Row {
        matches: &[Match::Prefix("anthropic/")],
        temperature: true,
        tools: true,
        context: 1_000_000,
        output: 128_000,
    },
    // OpenAI o-series via OpenRouter: no temperature.
    Row {
        matches: &[Match::Prefix("openai/o")],
        temperature: false,
        tools: true,
        context: 200_000,
        output: 100_000,
    },
    // OpenAI GPT-5.x via OpenRouter.
    Row {
        matches: &[Match::Prefix("openai/gpt-5")],
        temperature: true,
        tools: true,
        context: 1_050_000,
        output: 128_000,
    },
];

/// What an OpenRouter model this build has never heard of is assumed to be.
///
/// Deliberately conservative: guessing high would size regions past what the
/// model accepts and turn a quiet inefficiency into an API error. The cost of
/// guessing low is that percentage budgets resolve against a window eight times
/// smaller than a 1M-token model really has, which is why reaching this is
/// worth saying out loud - see [`OpenRouterProvider::warn_if_unknown`].
pub(crate) const FALLBACK_CAPABILITIES: ModelCapabilities = ModelCapabilities {
    supports_temperature: true,
    supports_streaming: true,
    supports_tools: true,
    supports_system_prompt: true,
    max_context_tokens: 128_000,
    max_output_tokens: 8_192,
    limits_source: LimitsSource::Builtin,
};

impl OpenRouterProvider {
    /// The headers every chat request carries.
    ///
    /// OpenRouter attributes a request to an app by the referer and title
    /// pair, and sending only the referer left every Leviath call unnamed on
    /// the account's activity page.
    fn chat_headers(&self) -> [(&'static str, String); 4] {
        [
            ("Authorization", format!("Bearer {}", self.api_key)),
            ("HTTP-Referer", "https://leviath.dev".to_string()),
            ("X-Title", "Leviath".to_string()),
            ("Content-Type", "application/json".to_string()),
        ]
    }

    /// Whether this model has already refused a temperature.
    fn temperature_is_unsupported(&self, model: &str) -> bool {
        self.temperature_unsupported.contains(model)
    }

    /// Record that it did, for the rest of this process.
    fn remember_temperature_unsupported(&self, model: &str) {
        self.temperature_unsupported.insert(model);
    }

    /// Say so when a model fell through to [`FALLBACK_CAPABILITIES`].
    ///
    /// OpenRouter fronts hundreds of models and this build's table names a few
    /// dozen, so an unlisted model is ordinary rather than exceptional - and it
    /// silently got a 128 000-token window. Region budgets are percentages of
    /// that window, so a `budget = "30%"` region on a model that really has
    /// 1M tokens was being sized at 38 400 instead of 314 572: no error, no
    /// warning, just an agent that evicts working material early and looks like
    /// a worse model.
    ///
    /// Reported once per model rather than per inference, and it names the
    /// stanza that fixes it, which is now a partial entry (#338).
    /// GET `/models`, shared by [`Provider::list_models`] and
    /// [`Provider::prime_capabilities`] so the two cannot disagree about what
    /// the endpoint is or how its failures read.
    async fn fetch_models_json(&self) -> Result<serde_json::Value> {
        let response = crate::provider::apply_request_timeout(
            self.client
                .get(format!("{}/models", self.base_url))
                .header("Authorization", format!("Bearer {}", self.api_key)),
            Some(crate::provider::SIDE_CALL_TIMEOUT_SECS),
        )
        .send()
        .await
        .map_err(|e| ProviderError::transport("listing models", &e))?;

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

    fn warn_if_unknown(&self, model: &str, resolved: &ModelCapabilities) {
        if resolved.max_context_tokens != FALLBACK_CAPABILITIES.max_context_tokens {
            return;
        }
        // The test is "did we end up on the fallback number", which cannot tell
        // a guess apart from a model that genuinely has a 128 000-token window.
        // If the API told us about this model, we are not guessing, whatever
        // the number came out as.
        if self.learned.contains(model) {
            return;
        }
        if !self.warned_unknown.insert(model) {
            return;
        }
        tracing::warn!(
            model = %model,
            assumed_context_tokens = FALLBACK_CAPABILITIES.max_context_tokens,
            "this build has no context window for this OpenRouter model, so it is \
             assuming a conservative one; percentage region budgets resolve against \
             it. Set the real window with [model_capabilities.\"{model}\"] \
             max_context_tokens = <n> (see `lev models show {model}`)",
        );
    }
}

#[async_trait]
impl Provider for OpenRouterProvider {
    async fn infer(&self, request: &InferenceRequest) -> Result<InferenceResponse> {
        tracing::debug!(model = %request.model, "Calling OpenRouter API");

        if let Some(limiter) = &self.rate_limiter {
            limiter.acquire().await?;
        }

        let mut body = self.build_request_body(request);
        let url = format!("{}/chat/completions", self.base_url);
        // A model this gateway has already refused a temperature for: send it
        // without one rather than spend a round trip learning the same thing.
        if self.temperature_is_unsupported(&request.model)
            && let Some(fields) = body.as_object_mut()
        {
            fields.remove("temperature");
        }
        let headers = self.chat_headers();

        let mut sent = send_chat_request(
            &self.client,
            "openrouter",
            &url,
            &headers,
            &body,
            self.rate_limiter.as_ref(),
            request.request_timeout_secs,
        )
        .await;
        // The gateway passes the upstream refusal through verbatim, so the
        // same answer works here as on the direct provider: drop the
        // temperature and ask again.
        if let Err(crate::ProviderError::ApiError(detail)) = &sent
            && temperature_refused(detail)
        {
            tracing::debug!(
                model = %request.model,
                "the API refused the temperature we sent; retrying without it"
            );
            self.remember_temperature_unsupported(&request.model);
            if let Some(fields) = body.as_object_mut() {
                fields.remove("temperature");
            }
            sent = send_chat_request(
                &self.client,
                "openrouter",
                &url,
                &headers,
                &body,
                self.rate_limiter.as_ref(),
                request.request_timeout_secs,
            )
            .await;
        }
        let response = sent?;

        let response_body: serde_json::Value = crate::provider::decode_json(response).await?;

        let result = parse_openai_response(&response_body)?;

        if let Some(limiter) = &self.rate_limiter {
            limiter.record_tokens(result.tokens_used.total_tokens);
        }

        Ok(result)
    }

    async fn infer_stream(
        &self,
        request: &InferenceRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>> {
        tracing::debug!(model = %request.model, "Calling OpenRouter API (streaming)");

        if let Some(limiter) = &self.rate_limiter {
            limiter.acquire().await?;
        }

        let mut body = self.build_request_body(request);
        crate::openai_compat::make_streaming(&mut body);
        let url = format!("{}/chat/completions", self.base_url);

        let response = send_chat_request(
            &self.client,
            "openrouter",
            &url,
            &self.chat_headers(),
            &body,
            self.rate_limiter.as_ref(),
            request.request_timeout_secs,
        )
        .await?;

        // Reuse OpenAI SSE parser since the format is identical
        let peer = leviath_net::read_caps::peer_of(&response);
        let byte_stream = response.bytes_stream();
        let stream = openai_sse_stream(byte_stream).sent_by(peer);

        Ok(crate::rate_limit::meter_stream(
            self.rate_limiter.as_ref(),
            Box::pin(stream),
        ))
    }

    async fn count_tokens(&self, text: &str, _model: &str) -> usize {
        // OpenRouter fronts many models and exposes no token-count endpoint;
        // approximate locally (provider-specific tokenizers not available).
        leviath_core::estimate_tokens(text)
    }

    fn max_context_tokens(&self, model: &str) -> usize {
        // Delegate to the per-model capabilities table rather than a flat 128K,
        // which badly under-budgeted large-context models (Llama-4 ~10M, several
        // ~1M) and over-budgeted small ones.
        self.capabilities(model).max_context_tokens
    }

    fn name(&self) -> &str {
        "openrouter"
    }

    fn capabilities(&self, model: &str) -> ModelCapabilities {
        // Three answers, narrowest first: what the user wrote, what OpenRouter
        // says, what this build was compiled with.
        let base = self.learned.corrected(model, builtin_capabilities(model));
        // Merged, not swapped: an entry names only what it corrects.
        let mut caps = match self.capability_overrides.get(model) {
            Some(o) => o.apply_to(base),
            None => {
                self.warn_if_unknown(model, &base);
                base
            }
        };
        // A refusal the gateway has already sent outranks every other source,
        // the operator's entry included: the request was made and the answer
        // was no, and the runtime reads this flag to decide whether to resolve
        // a temperature at all.
        if self.temperature_is_unsupported(model) {
            caps.supports_temperature = false;
        }
        caps
    }

    fn serves_model(&self, model_key: &str) -> Option<String> {
        // Answered from the primed catalogue rather than the built-in table:
        // OpenRouter fronts hundreds of models and the table lists a few dozen,
        // so the table would deny models the gateway serves perfectly well.
        // Matching on the last path segment because the gateway prefixes a
        // vendor namespace (`openai/gpt-5.5`) that the blueprint does not name.
        let primed = self.learned.find_by_key(model_key);
        // An empty catalogue means priming failed or has not run, and answering
        // "no" to everything there would deny models this gateway plainly
        // carries. The compiled-in table is the fallback, read directly rather
        // than through `serves_model_from_table`: that helper treats "differs
        // from the default capabilities" as "known", and an unlisted model here
        // falls through to FALLBACK_CAPABILITIES, which differs from the default
        // too. Going through it would claim every model in existence.
        primed.or_else(|| {
            (builtin_capabilities(model_key) != FALLBACK_CAPABILITIES)
                .then(|| model_key.to_string())
        })
    }

    /// The gateway's own listing, once priming has read it.
    ///
    /// Only the primed catalogue counts. The compiled-in table that
    /// [`Self::serves_model`] falls back to lists a few dozen of the hundreds
    /// this gateway carries, so reporting it here would call every model it
    /// does not happen to mention a model OpenRouter refuses - which is the
    /// opposite of true. An empty catalogue is "not asked yet", not "serves
    /// nothing".
    fn served_catalog(&self) -> Option<Vec<String>> {
        self.learned.catalog()
    }

    fn pricing(&self, model: &str) -> Option<crate::ModelPricing> {
        self.learned.get(model).and_then(|m| m.pricing)
    }

    /// Read the gateway's listing into `Self::learned`.
    ///
    /// What `GET /models` fills, measured against the live endpoint: both
    /// limits (`context_length`, `top_provider.max_completion_tokens`), the
    /// temperature and tools flags (`supported_parameters`), the cache-write
    /// signal and rates (`pricing`), the display name and release date. It
    /// carries no retirement date. See `catalog::parse_entry`.
    async fn prime_capabilities(&self) -> Result<()> {
        let body = self.fetch_models_json().await?;
        let data = body
            .get("data")
            .and_then(|d| d.as_array())
            .ok_or_else(|| ProviderError::InvalidResponse("Missing 'data' array".to_string()))?;

        let learned: HashMap<_, _> = data.iter().filter_map(parse_entry).collect();
        let count = learned.len();
        let priced = learned.values().filter(|m| m.pricing.is_some()).count();
        self.learned.replace(learned);
        tracing::debug!(
            models = count,
            priced,
            "learned OpenRouter model capabilities and rates"
        );
        Ok(())
    }

    /// The listing, answered from `Self::learned` so it cannot disagree
    /// with what an inference is told about the same model.
    ///
    /// Primes first when nothing has been learned yet, which is the one fetch
    /// this makes; a caller that already primed pays nothing.
    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        if self.learned.is_empty() {
            self.prime_capabilities().await?;
        }
        Ok(self
            .learned
            .to_model_infos("openrouter", |id| self.capabilities(id)))
    }
}

#[cfg(test)]
mod tests {

    /// `/models` quotes USD per token as strings; `ModelPricing` is per million.
    /// Getting that scale wrong is a factor of a million, which is the kind of
    /// error that looks like a bug in something else entirely.
    #[test]
    fn model_rates_are_read_per_million_from_the_catalog() {
        let entry = serde_json::json!({
            "id": "x-ai/grok-4.6",
            "pricing": {
                "prompt": "0.000002",
                "completion": "0.000006",
                "input_cache_read": "0.0000002",
                "input_cache_write": "0.0000025"
            }
        });
        let p = catalog::parse_pricing(&entry).expect("both sides quoted");
        // Compared with a tolerance, not for equality: scaling a per-token rate
        // by a million lands a hair off the decimal it came from
        // (0.0000002 * 1e6 is 0.19999999999999998), and a cost report does not
        // care about the sixteenth digit.
        let close = |got: f64, want: f64| {
            assert!((got - want).abs() < 1e-9, "{got} != {want}");
        };
        close(p.input_per_mtok, 2.0);
        close(p.output_per_mtok, 6.0);
        close(p.cached_input_per_mtok, 0.2);
        close(p.cache_write_per_mtok, 2.5);
    }

    /// Cache rates are often absent. A provider that does not price caching
    /// separately charges the input rate for it, which is what the fallback
    /// encodes - not zero, which would under-report every cached call.
    #[test]
    fn absent_cache_rates_fall_back_to_the_input_rate() {
        let entry = serde_json::json!({
            "id": "m",
            "pricing": { "prompt": "0.000003", "completion": "0.000009" }
        });
        let p = catalog::parse_pricing(&entry).expect("both sides quoted");
        assert_eq!(p.cached_input_per_mtok, 3.0);
        assert_eq!(p.cache_write_per_mtok, 3.0);
    }

    /// Half a rate card is not a rate card. A total built from an input rate
    /// with no output rate is wrong and still looks like a number.
    #[test]
    fn a_model_missing_either_side_is_left_unpriced() {
        for pricing in [
            serde_json::json!({ "prompt": "0.000003" }),
            serde_json::json!({ "completion": "0.000009" }),
            serde_json::json!({ "prompt": "free", "completion": "0.000009" }),
        ] {
            let entry = serde_json::json!({ "id": "m", "pricing": pricing });
            assert!(catalog::parse_pricing(&entry).is_none(), "{pricing}");
        }
        assert!(catalog::parse_pricing(&serde_json::json!({ "id": "m" })).is_none());
    }

    /// Until priming has run there are no rates, and a call is unpriced rather
    /// than free - the endpoint may be unreachable and never answer.
    #[test]
    fn an_unprimed_provider_quotes_nothing() {
        let p = OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "k".to_string(),
        );
        assert_eq!(p.pricing("x-ai/grok-4.6"), None);
    }
    use super::*;
    use crate::test_support::always_on_tracing_guard;
    use leviath_testkit::{spawn_mock_server, spawn_mock_server_truncated_body};

    #[test]
    fn test_provider_creation() {
        let provider = OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        assert_eq!(provider.name(), "openrouter");
    }

    #[test]
    fn test_build_request_body() {
        let provider = OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let request = InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Hello".into(),
                cache_breakpoint: false,
            }],
            model: "anthropic/claude-sonnet-4".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        assert_eq!(body["model"], "anthropic/claude-sonnet-4");
    }

    #[test]
    fn test_build_request_body_passes_through_extra_params() {
        let provider = OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let request = InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Hello".into(),
                cache_breakpoint: false,
            }],
            model: "openai/gpt-4o".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::json!({ "top_p": 0.9, "seed": 3 }),
            request_timeout_secs: None,
        };
        let body = provider.build_request_body(&request);
        assert_eq!(body["top_p"], serde_json::json!(0.9));
        assert_eq!(body["seed"], serde_json::json!(3));
    }

    #[test]
    fn test_build_request_body_prepends_system_blocks() {
        let provider = OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let request = InferenceRequest {
            system: vec![crate::provider::SystemBlock {
                text: "You are a helpful assistant.".to_string(),
                cache_hint: leviath_core::CacheHint::Never,
                volatility: leviath_core::Volatility::default(),
                region: String::new(),
            }],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Hello".into(),
                cache_breakpoint: false,
            }],
            model: "openai/gpt-4o".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        let messages = body["messages"].as_array().unwrap();
        // System block is delivered as the first message, not dropped.
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "You are a helpful assistant.");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "Hello");
    }

    #[test]
    fn test_build_request_body_serializes_tool_history() {
        let provider = OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let request = InferenceRequest {
            system: vec![],
            messages: vec![
                crate::provider::Message {
                    role: "assistant".to_string(),
                    content: crate::provider::MessageContent::Blocks(vec![
                        crate::provider::ContentBlock::ToolUse {
                            id: "call_1".to_string(),
                            name: "get_weather".to_string(),
                            input: serde_json::json!({ "city": "Paris" }),
                            thought_signature: None,
                        },
                    ]),
                    cache_breakpoint: false,
                },
                crate::provider::Message {
                    role: "user".to_string(),
                    content: crate::provider::MessageContent::Blocks(vec![
                        crate::provider::ContentBlock::ToolResult {
                            tool_use_id: "call_1".to_string(),
                            content: "sunny".to_string(),
                            is_error: false,
                        },
                    ]),
                    cache_breakpoint: false,
                },
            ],
            model: "openai/gpt-4o".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        let messages = body["messages"].as_array().unwrap();
        // Tool call → assistant message with an OpenAI `tool_calls` array.
        assert_eq!(messages[0]["role"], "assistant");
        assert_eq!(messages[0]["tool_calls"][0]["id"], "call_1");
        assert_eq!(
            messages[0]["tool_calls"][0]["function"]["name"],
            "get_weather"
        );
        // Tool result → a `tool`-role message keyed by the call id.
        assert_eq!(messages[1]["role"], "tool");
        assert_eq!(messages[1]["tool_call_id"], "call_1");
        assert_eq!(messages[1]["content"], "sunny");
    }

    // ── Additional coverage tests ──────────────────────────────────────────

    #[test]
    fn test_name() {
        let provider = OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        assert_eq!(provider.name(), "openrouter");
    }

    #[tokio::test]
    async fn test_count_tokens() {
        let provider = OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let tokens = provider.count_tokens("Hello, world!", "any-model").await;
        assert_eq!(tokens, 4); // ceil(13 / 4): the shared estimate rounds up
    }

    #[tokio::test]
    async fn test_count_tokens_empty() {
        let provider = OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        assert_eq!(provider.count_tokens("", "any-model").await, 0);
    }

    #[test]
    fn test_max_context_tokens() {
        let provider = OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        // Unknown model ⇒ the conservative fallback.
        assert_eq!(provider.max_context_tokens("any-model"), 128_000);
        // A known large-context model reports its real size (not a flat 128K) -
        // it now delegates to the per-model capabilities table.
        assert_eq!(
            provider.max_context_tokens("meta-llama/llama-4-scout"),
            provider
                .capabilities("meta-llama/llama-4-scout")
                .max_context_tokens
        );
        assert!(provider.max_context_tokens("meta-llama/llama-4-scout") > 128_000);
    }

    // ─── The conservative fallback is no longer silent ──────────────────────

    #[test]
    fn a_known_model_keeps_its_own_window() {
        // The table is what makes the warning meaningful: if everything fell
        // through, warning would be noise.
        let caps = builtin_capabilities("google/gemini-3.5-flash");
        assert_ne!(
            caps.max_context_tokens, FALLBACK_CAPABILITIES.max_context_tokens,
            "a listed model should not be resolving to the fallback"
        );
    }

    #[test]
    fn an_unlisted_model_lands_on_the_fallback() {
        // The reported models. OpenRouter fronts hundreds and this table names
        // a few dozen, so this is the ordinary case rather than the odd one.
        for model in ["moonshotai/kimi-k3", "sakana/fugu-ultra"] {
            assert_eq!(
                builtin_capabilities(model).max_context_tokens,
                FALLBACK_CAPABILITIES.max_context_tokens,
                "{model}"
            );
        }
    }

    /// The warning fires once per model, not once per inference.
    ///
    /// Asserted through the public path because that is where the run reaches
    /// it: `capabilities` is called for every call a stage makes.
    #[test]
    fn the_fallback_warning_is_once_per_model() {
        let provider = OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        for _ in 0..3 {
            provider.capabilities("moonshotai/kimi-k3");
            provider.capabilities("sakana/fugu-ultra");
        }
        let warned = &provider.warned_unknown;
        assert_eq!(warned.len(), 2, "one entry per model: {warned:?}");
    }

    /// An override silences it, because the window is then known.
    #[test]
    fn a_corrected_window_does_not_warn() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "moonshotai/kimi-k3".to_string(),
            ModelCapabilityOverride {
                max_context_tokens: Some(1_048_576),
                ..Default::default()
            },
        );
        let provider = OpenRouterProvider::with_overrides(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
            overrides,
            None,
        );
        let caps = provider.capabilities("moonshotai/kimi-k3");
        assert_eq!(caps.max_context_tokens, 1_048_576);
        assert!(
            provider.warned_unknown.is_empty(),
            "nothing to warn about once the window is known"
        );
    }

    #[test]
    fn test_with_overrides() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "custom/model".to_string(),
            ModelCapabilities {
                supports_temperature: false,
                supports_streaming: false,
                supports_tools: false,
                supports_system_prompt: false,
                max_context_tokens: 99,
                max_output_tokens: 10,
                limits_source: LimitsSource::Builtin,
            }
            .into(),
        );
        let provider = OpenRouterProvider::with_overrides(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
            overrides,
            None,
        );
        let caps = provider.capabilities("custom/model");
        assert_eq!(caps.max_context_tokens, 99);
    }

    #[test]
    fn test_capabilities_google_gemini() {
        let provider = OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let caps = provider.capabilities("google/gemini-3.5-flash");
        assert!(caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 1_048_576);
        assert_eq!(caps.max_output_tokens, 65_536);
    }

    #[test]
    fn test_capabilities_llama4_scout() {
        let provider = OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let caps = provider.capabilities("meta-llama/llama-4-scout-17b");
        assert_eq!(caps.max_context_tokens, 10_000_000);
    }

    #[test]
    fn test_capabilities_llama4_maverick() {
        let provider = OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let caps = provider.capabilities("meta-llama/llama-4-maverick");
        assert_eq!(caps.max_context_tokens, 1_048_576);
    }

    #[test]
    fn test_capabilities_deepseek_r1() {
        let provider = OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let caps = provider.capabilities("deepseek/deepseek-r1");
        assert!(!caps.supports_temperature);
        assert!(!caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 163_840);
    }

    #[test]
    fn test_capabilities_deepseek_v4_pro() {
        let provider = OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let caps = provider.capabilities("deepseek/deepseek-v4-pro");
        assert_eq!(caps.max_context_tokens, 1_048_576);
        assert_eq!(caps.max_output_tokens, 393_216);
    }

    #[test]
    fn test_capabilities_deepseek_v_series() {
        let provider = OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let caps = provider.capabilities("deepseek/deepseek-v3");
        assert_eq!(caps.max_context_tokens, 1_048_576);
        assert_eq!(caps.max_output_tokens, 65_536);
    }

    #[test]
    fn test_capabilities_mistral_large() {
        let provider = OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let caps = provider.capabilities("mistralai/mistral-large-latest");
        assert_eq!(caps.max_context_tokens, 262_144);
    }

    #[test]
    fn test_capabilities_mistralai_general() {
        let provider = OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let caps = provider.capabilities("mistralai/mistral-small-latest");
        assert_eq!(caps.max_context_tokens, 131_072);
    }

    #[test]
    fn test_capabilities_qwen36() {
        let provider = OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let caps = provider.capabilities("qwen/qwen3.6-235b");
        assert_eq!(caps.max_context_tokens, 1_048_576);
    }

    #[test]
    fn test_capabilities_qwen3_coder() {
        let provider = OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let caps = provider.capabilities("qwen/qwen3-coder-plus");
        assert_eq!(caps.max_context_tokens, 1_048_576);
    }

    #[test]
    fn test_capabilities_qwen_general() {
        let provider = OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let caps = provider.capabilities("qwen/qwen3-32b");
        assert_eq!(caps.max_context_tokens, 131_072);
    }

    #[test]
    fn test_capabilities_anthropic_via_openrouter() {
        let provider = OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let caps = provider.capabilities("anthropic/claude-sonnet-4-6");
        assert!(caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 1_000_000);
        assert_eq!(caps.max_output_tokens, 128_000);
    }

    #[test]
    fn test_capabilities_anthropic_no_temp() {
        let provider = OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let caps = provider.capabilities("anthropic/claude-opus-4-8");
        assert!(!caps.supports_temperature);
    }

    #[test]
    fn test_capabilities_anthropic_fable5_no_temp() {
        let provider = OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let caps = provider.capabilities("anthropic/claude-fable-5");
        assert!(!caps.supports_temperature);
    }

    #[test]
    fn test_capabilities_openai_o_series() {
        let provider = OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let caps = provider.capabilities("openai/o3-mini");
        assert!(!caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 200_000);
    }

    #[test]
    fn test_capabilities_openai_gpt5() {
        let provider = OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let caps = provider.capabilities("openai/gpt-5.4-mini");
        assert!(caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 1_050_000);
    }

    #[test]
    fn test_capabilities_unknown_fallback() {
        let provider = OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let caps = provider.capabilities("totally/unknown-model");
        assert!(caps.supports_temperature);
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 128_000);
        assert_eq!(caps.max_output_tokens, 8_192);
    }

    #[test]
    fn test_build_request_body_anthropic_cache_breakpoint() {
        let provider = OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let request = InferenceRequest {
            system: vec![],
            messages: vec![
                crate::provider::Message {
                    role: "user".to_string(),
                    content: "First".into(),
                    cache_breakpoint: true,
                },
                crate::provider::Message {
                    role: "user".to_string(),
                    content: "Second".into(),
                    cache_breakpoint: false,
                },
            ],
            model: "anthropic/claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        let msgs = body["messages"].as_array().unwrap();
        // First message should have cache_control in content block (anthropic model)
        assert!(msgs[0]["content"].is_array());
        assert_eq!(msgs[0]["content"][0]["cache_control"]["type"], "ephemeral");
        // Second message should be simple string content
        assert!(msgs[1]["content"].is_string());
    }

    /// A stable system block carries a marker, so an Anthropic model reached
    /// through the gateway can cache its prefix at all.
    ///
    /// System blocks used to be pushed as plain strings whatever the model, so
    /// nothing marked the one part of the request worth caching - the stage
    /// prompt and the pinned regions. Two research runs read zero tokens from
    /// cache across 2.9M and 5.9M input tokens because of it.
    #[test]
    fn a_stable_system_block_is_marked_for_an_anthropic_model() {
        let provider = OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        // Long enough to clear the minimum cacheable prefix; a shorter one is
        // correctly left unmarked and would prove nothing.
        let big = "stable reference material. ".repeat(400);
        let request = InferenceRequest {
            system: vec![crate::provider::SystemBlock {
                text: big,
                cache_hint: leviath_core::CacheHint::Always,
                region: String::new(),
                volatility: leviath_core::Volatility::Stable,
            }],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Hello".into(),
                cache_breakpoint: false,
            }],
            model: "anthropic/claude-sonnet-5".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert!(
            msgs[0]["content"].is_array(),
            "a marked system block is sent as content blocks: {}",
            body
        );
        assert_eq!(msgs[0]["content"][0]["cache_control"]["type"], "ephemeral");
    }

    /// The same block on a non-Anthropic model stays a plain string: the
    /// annotation means nothing there and the gateway would carry it anyway.
    #[test]
    fn a_system_block_is_not_marked_for_a_non_anthropic_model() {
        let provider = OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let big = "stable reference material. ".repeat(400);
        let request = InferenceRequest {
            system: vec![crate::provider::SystemBlock {
                text: big,
                cache_hint: leviath_core::CacheHint::Always,
                region: String::new(),
                volatility: leviath_core::Volatility::Stable,
            }],
            messages: vec![],
            model: "deepseek/deepseek-v4-flash".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        assert!(body["messages"][0]["content"].is_string());
    }

    /// System markers come out of the same four-marker budget as the messages,
    /// which is Anthropic's limit for the whole request. Spending two on the
    /// system leaves two for the conversation.
    #[test]
    fn system_markers_take_their_share_of_the_four_marker_budget() {
        let provider = OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let big = "stable reference material. ".repeat(400);
        let request = InferenceRequest {
            system: vec![
                crate::provider::SystemBlock {
                    text: big.clone(),
                    cache_hint: leviath_core::CacheHint::Always,
                    region: String::new(),
                    volatility: leviath_core::Volatility::Stable,
                },
                crate::provider::SystemBlock {
                    text: big,
                    cache_hint: leviath_core::CacheHint::Always,
                    region: String::new(),
                    volatility: leviath_core::Volatility::Stable,
                },
            ],
            messages: (0..6)
                .map(|i| crate::provider::Message {
                    role: "user".to_string(),
                    content: format!("msg {i}").into(),
                    cache_breakpoint: true,
                })
                .collect(),
            model: "anthropic/claude-sonnet-5".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        let marked = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["content"].is_array() && m["content"][0].get("cache_control").is_some())
            .count();
        assert!(
            marked <= 4,
            "Anthropic takes at most four markers per request, got {marked}: {body}"
        );
        assert!(
            marked > 2,
            "the system blocks must not eat the whole budget: {marked}"
        );
    }

    #[test]
    fn test_build_request_body_non_anthropic_no_cache_breakpoint() {
        let provider = OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let request = InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Hello".into(),
                cache_breakpoint: true,
            }],
            model: "openai/gpt-5.4-mini".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        let msgs = body["messages"].as_array().unwrap();
        // Non-anthropic model should not get cache_control blocks
        assert!(msgs[0]["content"].is_string());
    }

    /// Gemini reads the same markers through OpenRouter (measured: 0 cached
    /// tokens on 200k-token prompts without them), so it gets them too.
    #[test]
    fn gemini_gets_explicit_cache_markers_and_grok_does_not() {
        let provider = OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        assert!(provider.explicit_cache_control("google/gemini-3.1-pro-preview"));
        assert!(provider.explicit_cache_control("anthropic/claude-sonnet-5"));
        assert!(!provider.explicit_cache_control("x-ai/grok-4.6"));
        let request = InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Hello".into(),
                cache_breakpoint: true,
            }],
            model: "google/gemini-3.1-pro-preview".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };
        let body = provider.build_request_body(&request);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["content"][0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn test_build_request_body_max_4_breakpoints() {
        let provider = OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let messages: Vec<crate::provider::Message> = (0..6)
            .map(|i| crate::provider::Message {
                role: "user".to_string(),
                content: format!("msg {}", i).into(),
                cache_breakpoint: true,
            })
            .collect();

        let request = InferenceRequest {
            system: vec![],
            messages,
            model: "anthropic/claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        let msgs = body["messages"].as_array().unwrap();
        let bp_count = msgs.iter().filter(|m| m["content"].is_array()).count();
        assert_eq!(bp_count, 4);
    }

    #[test]
    fn test_build_request_body_with_tools() {
        let provider = OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let request = InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Search".into(),
                cache_breakpoint: false,
            }],
            model: "openai/gpt-5.4-mini".to_string(),
            max_tokens: 512,
            temperature: 0.5,
            tools: vec![crate::provider::Tool {
                name: "search".to_string(),
                description: "Search the web".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            }],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["function"]["name"], "search");
    }

    #[test]
    fn test_build_request_body_no_temp_for_deepseek_r1() {
        let provider = OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let request = InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Think".into(),
                cache_breakpoint: false,
            }],
            model: "deepseek/deepseek-r1".to_string(),
            max_tokens: 512,
            temperature: 0.5,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        // deepseek-r1 doesn't support temperature
        assert!(body.get("temperature").is_none());
    }

    // ─── HTTP-call-level tests via a raw-TCP mock server ───────────────────

    fn provider_with_url(url: String) -> OpenRouterProvider {
        OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        )
        .with_base_url(Some(url))
    }

    #[test]
    fn with_base_url_keeps_the_default_when_none_is_given() {
        let provider = OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        )
        .with_base_url(None);
        assert_eq!(provider.base_url, "https://openrouter.ai/api/v1");
    }

    #[test]
    fn with_base_url_replaces_the_default() {
        let provider = OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        )
        .with_base_url(Some("https://custom.example.com".to_string()));
        assert_eq!(provider.base_url, "https://custom.example.com");
    }

    fn simple_request() -> InferenceRequest {
        InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "hi".into(),
                cache_breakpoint: false,
            }],
            model: "openai/gpt-4o".to_string(),
            max_tokens: 100,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        }
    }

    /// The gateway path recovers too.
    ///
    /// OpenRouter fronts every vendor, so it reaches the same models that
    /// refuse a temperature - and its own capability table names a few dozen
    /// of hundreds, so it is even less able to know in advance which.
    #[tokio::test]
    async fn a_refused_temperature_is_retried_without_one() {
        let refusal = br#"{"error":{"message":"Unsupported value: 'temperature' does not support 0.7 with this model. Only the default (1) value is supported.","type":"invalid_request_error","param":"temperature","code":"unsupported_value"}}"#;
        let ok = br#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2}}"#;
        let (url, bodies) = leviath_testkit::spawn_mock_sequence(vec![
            (400, "Bad Request", refusal.to_vec()),
            (200, "OK", ok.to_vec()),
        ])
        .await;

        let provider = OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        )
        .with_base_url(Some(url));

        let request = InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "hi".into(),
                cache_breakpoint: false,
            }],
            model: "openai/gpt-5.5".to_string(),
            max_tokens: 16,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let out = provider.infer(&request).await;
        assert!(out.is_ok(), "the retry has to rescue the call: {out:?}");

        let sent = leviath_core::sync::lock(&bodies).clone();
        let carried: Vec<bool> = sent.iter().map(|b| b.contains("temperature")).collect();
        assert_eq!(
            carried,
            vec![true, false],
            "the first request carries the temperature and the retry drops it: {sent:?}"
        );

        // And it is remembered, so the next call to this model never spends
        // the refused round trip again.
        assert!(provider.temperature_is_unsupported("openai/gpt-5.5"));
    }

    #[tokio::test]
    async fn infer_success_parses_response() {
        // Registers a real Subscriber so the tracing::debug! call's field
        // arguments at the top of infer() are actually exercised.
        let _guard = always_on_tracing_guard();
        let body = br#"{"choices":[{"message":{"content":"hi there"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2}}"#;
        let url = spawn_mock_server(200, "OK", body).await;
        let provider = provider_with_url(url);
        let resp = provider.infer(&simple_request()).await.unwrap();
        assert_eq!(resp.content, "hi there");
    }

    // ─── HTTP error paths (connection refused) ─────────────────────────────

    #[tokio::test]
    async fn infer_connection_refused_returns_error() {
        let provider = provider_with_url("http://127.0.0.1:19997".to_string());
        let err = provider.infer(&simple_request()).await.unwrap_err();
        assert!(err.to_string().contains("Request failed:"));
    }

    #[tokio::test]
    async fn infer_stream_connection_refused_returns_error() {
        let provider = provider_with_url("http://127.0.0.1:19997".to_string());
        let result = provider.infer_stream(&simple_request()).await;
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("Request failed:")
        );
    }

    #[tokio::test]
    async fn list_models_connection_refused_returns_error() {
        let provider = provider_with_url("http://127.0.0.1:19997".to_string());
        let err = provider.list_models().await.unwrap_err();
        assert!(err.to_string().contains("Request failed:"));
    }

    #[tokio::test]
    async fn list_models_non_success_body_read_error_falls_back_to_unknown_error() {
        let url = spawn_mock_server_truncated_body(500, "Internal Server Error").await;
        let provider = provider_with_url(url);
        let err = provider.list_models().await.unwrap_err();
        assert!(err.to_string().contains("unknown error"));
    }

    #[tokio::test]
    async fn infer_non_success_status_returns_api_error() {
        let url = spawn_mock_server(500, "Internal Server Error", b"boom").await;
        let provider = provider_with_url(url);
        let err = provider.infer(&simple_request()).await.unwrap_err();
        assert!(err.to_string().contains("API error:"));
    }

    #[tokio::test]
    async fn infer_malformed_json_returns_invalid_response() {
        let url = spawn_mock_server(200, "OK", b"not json").await;
        let provider = provider_with_url(url);
        let err = provider.infer(&simple_request()).await.unwrap_err();
        assert!(err.to_string().contains("Invalid response:"));
    }

    #[tokio::test]
    async fn infer_stream_non_success_status_returns_api_error() {
        let url = spawn_mock_server(503, "Service Unavailable", b"down").await;
        let provider = provider_with_url(url);
        let result = provider.infer_stream(&simple_request()).await;
        assert!(result.is_err());
        assert!(result.err().unwrap().to_string().contains("API error:"));
    }

    #[tokio::test]
    async fn infer_stream_success_yields_chunks() {
        // Registers a real Subscriber so the tracing::debug! call's field
        // arguments at the top of infer_stream() are actually exercised.
        let _guard = always_on_tracing_guard();
        let sse_body =
            b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
        let url = spawn_mock_server(200, "OK", sse_body).await;
        let provider = provider_with_url(url);
        let mut stream = provider.infer_stream(&simple_request()).await.unwrap();
        use tokio_stream::StreamExt;
        let chunk = stream.next().await.unwrap().unwrap();
        assert_eq!(chunk.delta, "hi");
    }

    // ─── Windows come from the API, not the compiled table (#360) ────────────

    /// The reported case, end to end: a model this build's table does not name,
    /// which OpenRouter says has a 1M-token window. Before priming it resolved
    /// to the 128 000-token fallback, and every percentage region budget was
    /// sized against that.
    #[tokio::test]
    async fn priming_takes_the_window_from_the_models_api() {
        let body = br#"{"data":[{"id":"moonshotai/kimi-k3","context_length":1048576,"top_provider":{"max_completion_tokens":32768}}]}"#;
        let url = spawn_mock_server(200, "OK", body).await;
        let provider = provider_with_url(url);

        let before = provider.capabilities("moonshotai/kimi-k3");
        assert_eq!(
            before.max_context_tokens, 128_000,
            "unprimed, the conservative fallback still applies"
        );

        provider.prime_capabilities().await.expect("primes");

        let after = provider.capabilities("moonshotai/kimi-k3");
        assert_eq!(after.max_context_tokens, 1_048_576);
        assert_eq!(after.max_output_tokens, 32_768);
    }

    /// An entry with no `supported_parameters` says nothing about shape, so
    /// the compiled table keeps that answer while the size still moves.
    #[tokio::test]
    async fn priming_leaves_request_shape_alone_when_the_listing_is_silent() {
        let body = br#"{"data":[{"id":"openai/o3","context_length":200000}]}"#;
        let url = spawn_mock_server(200, "OK", body).await;
        let provider = provider_with_url(url);

        let before = provider.capabilities("openai/o3");
        provider.prime_capabilities().await.expect("primes");
        let after = provider.capabilities("openai/o3");

        assert_eq!(after.max_context_tokens, 200_000, "the size moved");
        assert_eq!(
            after.supports_temperature, before.supports_temperature,
            "the shape did not"
        );
        assert_eq!(after.supports_tools, before.supports_tools);
    }

    /// A `[model_capabilities]` entry still wins. The user's own number is the
    /// last word - it is how someone corrects an API that is itself wrong.
    #[tokio::test]
    async fn an_explicit_override_outranks_the_api() {
        let body = br#"{"data":[{"id":"some/model","context_length":1048576}]}"#;
        let url = spawn_mock_server(200, "OK", body).await;
        let mut overrides = HashMap::new();
        overrides.insert(
            "some/model".to_string(),
            ModelCapabilityOverride {
                max_context_tokens: Some(64_000),
                ..Default::default()
            },
        );
        let mut provider = OpenRouterProvider::with_overrides(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "k".to_string(),
            overrides,
            None,
        );
        provider.base_url = url;

        provider.prime_capabilities().await.expect("primes");
        assert_eq!(
            provider.capabilities("some/model").max_context_tokens,
            64_000
        );
    }

    /// An entry with no `context_length` is skipped rather than recorded, so
    /// the built-in table stays in charge instead of being outranked by a
    /// guess.
    #[tokio::test]
    async fn a_model_without_a_context_length_is_not_recorded() {
        let body =
            br#"{"data":[{"id":"openai/gpt-4o"},{"id":"other/model","context_length":300000}]}"#;
        let url = spawn_mock_server(200, "OK", body).await;
        let provider = provider_with_url(url);

        let compiled = provider.capabilities("openai/gpt-4o").max_context_tokens;
        provider.prime_capabilities().await.expect("primes");

        assert_eq!(
            provider.capabilities("openai/gpt-4o").max_context_tokens,
            compiled,
            "the table still answers for a model the API said nothing about"
        );
        assert_eq!(
            provider.capabilities("other/model").max_context_tokens,
            300_000,
            "and the one it did describe is recorded"
        );
    }

    /// An unreachable endpoint degrades to the built-in table, which is the
    /// behaviour that existed before priming did.
    #[tokio::test]
    async fn a_failed_prime_leaves_the_builtin_table_in_charge() {
        let url = spawn_mock_server(500, "Internal Server Error", b"boom").await;
        let provider = provider_with_url(url);
        assert!(provider.prime_capabilities().await.is_err());
        assert_eq!(
            provider
                .capabilities("moonshotai/kimi-k3")
                .max_context_tokens,
            128_000
        );
    }

    /// A body that is not the documented shape is an error, not a silently
    /// empty table that reads as "the API said nothing".
    #[tokio::test]
    async fn priming_rejects_a_body_with_no_data_array() {
        let url = spawn_mock_server(200, "OK", br#"{"models":[]}"#).await;
        let provider = provider_with_url(url);
        let err = provider.prime_capabilities().await.unwrap_err();
        assert!(err.to_string().contains("data"), "got: {err}");
    }

    /// A model the API describes as having exactly the fallback window is not
    /// a guess, and must not be reported as one. The warning tests the
    /// resolved number, which cannot tell those apart on its own.
    #[tokio::test]
    async fn a_model_the_api_reports_at_the_fallback_size_is_not_warned_about() {
        let _guard = always_on_tracing_guard();
        let body = br#"{"data":[{"id":"some/128k-model","context_length":128000}]}"#;
        let url = spawn_mock_server(200, "OK", body).await;
        let provider = provider_with_url(url);
        provider.prime_capabilities().await.expect("primes");

        assert_eq!(
            provider.capabilities("some/128k-model").max_context_tokens,
            128_000
        );
        assert!(
            !provider.warned_unknown.contains("some/128k-model"),
            "the API answered for this model, so nothing was assumed"
        );
        // The control: a model it said nothing about is still reported.
        let _ = provider.capabilities("nobody/knows");
        assert!(
            provider.warned_unknown.contains("nobody/knows"),
            "a model with no answer anywhere is still called out"
        );
    }

    /// The gateway answers a blueprint's bare model name from its live
    /// catalogue, translating to the vendor-prefixed id it wants in a request.
    /// The blueprint says `gpt-5.5`; only the gateway knows it is
    /// `openai/gpt-5.5` here.
    #[tokio::test]
    async fn serves_model_translates_a_bare_name_to_the_gateway_id() {
        let body = br#"{"data":[{"id":"openai/gpt-5.5","context_length":400000},{"id":"x-ai/grok-4.6","context_length":256000}]}"#;
        let url = spawn_mock_server(200, "OK", body).await;
        let provider = provider_with_url(url);
        provider.prime_capabilities().await.expect("primes");

        assert_eq!(
            provider.serves_model("gpt-5.5"),
            Some("openai/gpt-5.5".to_string())
        );
        assert_eq!(
            provider.serves_model("grok-4.6"),
            Some("x-ai/grok-4.6".to_string()),
            "a model no built-in table names is still served once the catalogue \
             says so, which is the whole reason to ask the gateway"
        );
        assert_eq!(
            provider.serves_model("not-on-this-gateway"),
            None,
            "a primed catalogue that does not list it, and no table entry either"
        );
    }

    /// Priming can fail or simply not have run. Answering "I serve nothing"
    /// there would deny models the gateway plainly carries, so an empty
    /// catalogue falls through to the table compiled into this build.
    #[test]
    fn an_unprimed_gateway_answers_from_the_built_in_table() {
        let provider = provider_with_url("http://127.0.0.1:1".to_string());
        assert!(provider.learned.is_empty(), "nothing has primed this one");
        assert_eq!(
            provider.serves_model("deepseek-v4-pro"),
            Some("deepseek-v4-pro".to_string()),
            "the built-in table names this model, so an unprimed gateway still \
             offers it rather than degrading to nothing"
        );
        assert_eq!(provider.serves_model("nobody-has-heard-of-this"), None);
    }

    /// The gateway may publish a catalogue only once it has read one. The
    /// compiled-in table `serves_model` falls back to lists a few dozen of the
    /// hundreds this gateway carries, so reporting it as complete would call
    /// every model it does not mention a model OpenRouter refuses.
    #[tokio::test]
    async fn only_a_primed_gateway_publishes_a_catalogue() {
        let unprimed = provider_with_url("http://127.0.0.1:1".to_string());
        assert_eq!(
            unprimed.served_catalog(),
            None,
            "an unprimed gateway has read no catalogue, and its table is not one"
        );

        let body = br#"{"data":[{"id":"openai/gpt-5.5","context_length":400000}]}"#;
        let url = spawn_mock_server(200, "OK", body).await;
        let provider = provider_with_url(url);
        provider.prime_capabilities().await.expect("primes");

        assert_eq!(
            provider.served_catalog(),
            Some(vec!["openai/gpt-5.5".to_string()])
        );
    }

    /// An entry with no `id` cannot be looked up by one, so it is skipped.
    #[tokio::test]
    async fn a_model_without_an_id_is_skipped() {
        let body =
            br#"{"data":[{"context_length":999},{"id":"real/model","context_length":300000}]}"#;
        let url = spawn_mock_server(200, "OK", body).await;
        let provider = provider_with_url(url);
        provider.prime_capabilities().await.expect("primes");
        assert_eq!(
            provider.capabilities("real/model").max_context_tokens,
            300_000
        );
    }

    #[tokio::test]
    async fn list_models_success_returns_models() {
        let body = br#"{"data":[{"id":"openai/gpt-4o","name":"GPT-4o","context_length":128000,"top_provider":{"max_completion_tokens":16384}},{"id":"anthropic/claude-3","context_length":200000}]}"#;
        let url = spawn_mock_server(200, "OK", body).await;
        let provider = provider_with_url(url);
        let models = provider.list_models().await.unwrap();
        // Sorted by id, so the two cannot come back in listing order.
        assert_eq!(models.len(), 2);
        assert_eq!(models[1].id, "openai/gpt-4o");
        assert_eq!(models[1].display_name, Some("GPT-4o".to_string()));
        assert_eq!(models[1].capabilities.max_output_tokens, 16384);
        assert!(models[1].learned);
        assert_eq!(models[0].id, "anthropic/claude-3");
        assert_eq!(models[0].display_name, None);
        // No `max_completion_tokens` on the entry: the table's number, not a
        // default dressed up as the API's.
        assert_eq!(
            models[0].capabilities.max_output_tokens,
            builtin_capabilities("anthropic/claude-3").max_output_tokens
        );
        assert_eq!(models[0].capabilities.max_context_tokens, 200_000);
    }

    #[tokio::test]
    async fn list_models_non_success_status_returns_error() {
        let url = spawn_mock_server(401, "Unauthorized", b"nope").await;
        let provider = provider_with_url(url);
        let err = provider.list_models().await.unwrap_err();
        assert!(err.to_string().contains("API error:"));
    }

    #[tokio::test]
    async fn list_models_malformed_json_returns_error() {
        let url = spawn_mock_server(200, "OK", b"not json").await;
        let provider = provider_with_url(url);
        let err = provider.list_models().await.unwrap_err();
        assert!(err.to_string().contains("Invalid response:"));
    }

    #[tokio::test]
    async fn list_models_missing_data_field_returns_error() {
        let url = spawn_mock_server(200, "OK", b"{}").await;
        let provider = provider_with_url(url);
        let err = provider.list_models().await.unwrap_err();
        assert!(err.to_string().contains("Invalid response:"));
    }

    #[test]
    fn with_overrides_wires_the_rate_limiter() {
        // The daemon path constructs providers exclusively through
        // with_overrides, so a rate limit that stops here is a rate limit
        // nobody gets.
        let cfg = crate::provider::RateLimitConfig {
            requests_per_minute: 5,
            tokens_per_minute: 1_000,
        };
        let limited = OpenRouterProvider::with_overrides(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "k".to_string(),
            HashMap::new(),
            Some(&cfg),
        );
        assert!(limited.rate_limiter.is_some());
        let unlimited = OpenRouterProvider::with_overrides(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "k".to_string(),
            HashMap::new(),
            None,
        );
        assert!(unlimited.rate_limiter.is_none());
    }
}

#[cfg(test)]
mod learned_tests {
    use super::*;
    use leviath_testkit::{spawn_mock_sequence, spawn_mock_server};

    fn provider_at(url: String) -> OpenRouterProvider {
        OpenRouterProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        )
        .with_base_url(Some(url))
    }

    /// The listing measured on 2026-08-28, reduced to the four shapes that
    /// matter: a model that takes everything and bills a cache write (qwen), one
    /// listed without `temperature` (gpt-5.5, which refuses one), one with no
    /// `supported_parameters` at all, and one with no `context_length`.
    const LISTING: &[u8] = br#"{"data":[
        {"id":"qwen/qwen3.6-plus","name":"Qwen3.6 Plus","created":1775133557,"context_length":1000000,
         "top_provider":{"max_completion_tokens":65536},
         "pricing":{"prompt":"0.000001","completion":"0.000004","input_cache_write":"0.0000004"},
         "supported_parameters":["temperature","tools","max_tokens"]},
        {"id":"openai/gpt-5.5","context_length":1050000,
         "pricing":{"prompt":"0.00001","completion":"0.00003","input_cache_read":"0.000001"},
         "supported_parameters":["tools","max_tokens","reasoning"]},
        {"id":"sakana/sakana-namazu","context_length":32000},
        {"id":"x/no-size","supported_parameters":["temperature","tools"]}
    ]}"#;

    #[tokio::test]
    async fn the_listing_decides_shape_size_price_and_markers() {
        let url = spawn_mock_server(200, "OK", LISTING).await;
        let provider = provider_at(url);
        let gpt_before = provider.capabilities("openai/gpt-5.5");
        assert!(
            provider.explicit_cache_control("anthropic/claude-sonnet-5"),
            "unprimed, the name rule stands"
        );
        assert!(!provider.explicit_cache_control("qwen/qwen3.6-plus"));

        provider.prime_capabilities().await.expect("primes");

        let qwen = provider.capabilities("qwen/qwen3.6-plus");
        assert!(qwen.supports_temperature);
        assert!(qwen.supports_tools);
        assert_eq!(qwen.max_context_tokens, 1_000_000);
        assert_eq!(qwen.max_output_tokens, 65_536);
        assert_eq!(qwen.limits_source, LimitsSource::Api);
        assert!(
            provider.explicit_cache_control("qwen/qwen3.6-plus"),
            "a cache-write price is the marker signal"
        );
        assert!(
            !provider.explicit_cache_control("openai/gpt-5.5"),
            "a read-only price means the upstream caches on its own"
        );
        let rates = provider.pricing("qwen/qwen3.6-plus").expect("quoted");
        assert!((rates.input_per_mtok - 1.0).abs() < 1e-9);

        let gpt = provider.capabilities("openai/gpt-5.5");
        assert!(!gpt.supports_temperature, "listed without `temperature`");
        assert!(gpt.supports_tools);
        assert_eq!(
            gpt.max_output_tokens, gpt_before.max_output_tokens,
            "no `max_completion_tokens` on the entry: the table's number"
        );
        assert_eq!(gpt.max_context_tokens, 1_050_000);

        let namazu = provider.capabilities("sakana/sakana-namazu");
        assert!(
            namazu.supports_temperature && namazu.supports_tools,
            "no `supported_parameters`: the table (here, the fallback) answers"
        );
        assert_eq!(namazu.max_context_tokens, 32_000);

        let no_size = provider.capabilities("x/no-size");
        assert_eq!(
            no_size.limits_source,
            LimitsSource::Builtin,
            "flags learned, sizes not"
        );
        assert_eq!(
            provider.serves_model("no-size").as_deref(),
            Some("x/no-size"),
            "and it is still a model the gateway serves"
        );
        let mut catalog = provider.served_catalog().expect("primed");
        catalog.sort();
        assert_eq!(
            catalog,
            [
                "openai/gpt-5.5",
                "qwen/qwen3.6-plus",
                "sakana/sakana-namazu",
                "x/no-size"
            ]
        );
    }

    /// Markers land on the request only when the listing said the upstream
    /// bills a write, whatever the model's name.
    #[tokio::test]
    async fn markers_follow_the_cache_write_price_not_the_name() {
        let url = spawn_mock_server(200, "OK", LISTING).await;
        let provider = provider_at(url);
        provider.prime_capabilities().await.expect("primes");

        let request_for = |model: &str| InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "First".into(),
                cache_breakpoint: true,
            }],
            model: model.to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let qwen = provider.build_request_body(&request_for("qwen/qwen3.6-plus"));
        let msgs = qwen["messages"].as_array().unwrap();
        assert_eq!(
            msgs[0]["content"][0]["cache_control"]["type"], "ephemeral",
            "qwen bills a cache write, so the breakpoint is marked"
        );

        let gpt = provider.build_request_body(&request_for("openai/gpt-5.5"));
        let msgs = gpt["messages"].as_array().unwrap();
        assert!(
            msgs[0]["content"].is_string(),
            "a read-only price: plain text, no marker to refuse"
        );
        assert!(
            gpt.get("temperature").is_none(),
            "and no temperature, because the listing said not to"
        );
    }

    /// One fetch serves both priming and the listing.
    #[tokio::test]
    async fn the_listing_is_read_once() {
        // One response only: a second fetch would be answered with a 500.
        let (url, _bodies) = spawn_mock_sequence(vec![(200, "OK", LISTING.to_vec())]).await;
        let provider = provider_at(url);

        let listed = provider.list_models().await.expect("fetches once");
        assert_eq!(listed.len(), 4);
        assert_eq!(listed[0].id, "openai/gpt-5.5", "sorted by id");
        let qwen = listed.iter().find(|m| m.id == "qwen/qwen3.6-plus").unwrap();
        assert_eq!(qwen.display_name.as_deref(), Some("Qwen3.6 Plus"));
        assert_eq!(qwen.released, Some(1_775_133_557));
        assert!(qwen.pricing.is_some());
        assert!(qwen.learned);

        let again = provider.list_models().await.expect("from the store");
        assert_eq!(again.len(), 4);
    }

    /// A refusal the gateway has already sent shows in `capabilities`, above
    /// the listing and the operator's entry alike.
    #[test]
    fn a_refused_temperature_outranks_everything() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "x/model".to_string(),
            ModelCapabilityOverride {
                supports_temperature: Some(true),
                ..Default::default()
            },
        );
        let provider = OpenRouterProvider::with_overrides(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "k".to_string(),
            overrides,
            None,
        );
        assert!(provider.capabilities("x/model").supports_temperature);
        provider.remember_temperature_unsupported("x/model");
        assert!(!provider.capabilities("x/model").supports_temperature);
    }
}
