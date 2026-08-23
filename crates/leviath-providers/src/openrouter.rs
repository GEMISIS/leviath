//! OpenRouter provider implementation.
//!
//! OpenRouter provides access to multiple models through a unified API.
//! Uses OpenAI-compatible format with additional headers.

use crate::openai_compat::{
    OpenAiSseStream, parse_openai_response, send_chat_request, temperature_refused,
};
use crate::provider::{
    InferenceRequest, InferenceResponse, ModelCapabilities, ModelCapabilityOverride, ModelInfo,
    Provider, ProviderConfig, ProviderError, Result, StreamChunk,
};
use crate::rate_limit::RateLimiter;
use async_trait::async_trait;
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
    warned_unknown: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,

    /// Models the gateway has refused a temperature for.
    ///
    /// Per instance rather than a table: OpenRouter fronts every vendor, so no
    /// static list can stay right about which of their models take one, and
    /// this build's table already names only a few dozen of hundreds.
    temperature_unsupported: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,

    /// What OpenRouter's own `/models` endpoint says each model's window is,
    /// filled once by [`Provider::prime_capabilities`].
    ///
    /// OpenRouter fronts hundreds of models and this build's table names a few
    /// dozen, so the table is out of date the day it ships and an unlisted
    /// model silently got a conservative 128 000 tokens. Region budgets are
    /// percentages of the window, so that sized a `budget = "30%"` region on a
    /// 1M-token model at 38 400 instead of 314 572 (#337, #360).
    ///
    /// Empty until primed, and empty forever if the endpoint could not be
    /// reached - both mean "fall back to the built-in table", which is what
    /// happened before this existed.
    api_windows: std::sync::Arc<std::sync::Mutex<HashMap<String, ApiWindow>>>,
}

/// What `/models` reports about one model's sizes.
#[derive(Debug, Clone, Copy)]
struct ApiWindow {
    /// The context window OpenRouter will actually accept for this model.
    context_length: usize,
    /// The largest completion it will return, when the endpoint says.
    max_completion_tokens: Option<usize>,
}

impl OpenRouterProvider {
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
            api_windows: Default::default(),
        }
    }

    /// Create a new OpenRouter provider with full configuration.
    pub fn with_config(client: reqwest::Client, config: ProviderConfig) -> Self {
        let rate_limiter = config.rate_limit.as_ref().map(RateLimiter::new);
        Self {
            client,
            api_key: config.api_key,
            base_url: config
                .base_url
                .unwrap_or_else(|| "https://openrouter.ai/api/v1".to_string()),
            rate_limiter,
            capability_overrides: HashMap::new(),
            warned_unknown: Default::default(),
            temperature_unsupported: Default::default(),
            api_windows: Default::default(),
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
            api_windows: Default::default(),
        }
    }

    /// Point this provider at a different host.
    ///
    /// An enterprise gateway or self-hosted proxy speaks the same API on a
    /// different origin, and every part of that was already here - the struct
    /// holds a `base_url`, and `with_config` honours one - except a way for
    /// configuration to reach the constructor the registry actually calls.
    /// `with_config` sets the URL and drops the capability overrides;
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
        let is_anthropic = request.model.contains("claude");
        // Anthropic allows four `cache_control` markers per request across
        // system and messages together. System takes its claim first and the
        // conversation gets the rest, matching the direct provider.
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
            let tools: Vec<serde_json::Value> = request
                .tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters,
                        }
                    })
                })
                .collect();
            body["tools"] = serde_json::Value::Array(tools);
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
    // ── Google Gemini ─────────────────────────────────────────────────────
    if model.starts_with("google/gemini") {
        return ModelCapabilities {
            supports_temperature: true,
            supports_streaming: true,
            supports_tools: true,
            supports_system_prompt: true,
            max_context_tokens: 1_048_576,
            max_output_tokens: 65_536,
        };
    }
    // ── Meta Llama 4 Scout - 10M context ─────────────────────────────────
    if model.contains("llama-4-scout") {
        return ModelCapabilities {
            supports_temperature: true,
            supports_streaming: true,
            supports_tools: true,
            supports_system_prompt: true,
            max_context_tokens: 10_000_000,
            max_output_tokens: 32_768,
        };
    }
    // ── Meta Llama 4 (Maverick + others) - 1M context ────────────────────
    if model.starts_with("meta-llama/llama-4") {
        return ModelCapabilities {
            supports_temperature: true,
            supports_streaming: true,
            supports_tools: true,
            supports_system_prompt: true,
            max_context_tokens: 1_048_576,
            max_output_tokens: 32_768,
        };
    }
    // ── DeepSeek R1 - reasoning-only, no tools, no temperature ───────────
    if model.contains("deepseek-r1") {
        return ModelCapabilities {
            supports_temperature: false,
            supports_streaming: true,
            supports_tools: false,
            supports_system_prompt: true,
            max_context_tokens: 163_840,
            max_output_tokens: 32_768,
        };
    }
    // ── DeepSeek V4 Pro - 1M context, 384K output ────────────────────────
    if model.contains("deepseek-v4-pro") {
        return ModelCapabilities {
            supports_temperature: true,
            supports_streaming: true,
            supports_tools: true,
            supports_system_prompt: true,
            max_context_tokens: 1_048_576,
            max_output_tokens: 393_216,
        };
    }
    // ── DeepSeek V4 Flash / V3.x ─────────────────────────────────────────
    if model.starts_with("deepseek/deepseek-v") {
        return ModelCapabilities {
            supports_temperature: true,
            supports_streaming: true,
            supports_tools: true,
            supports_system_prompt: true,
            max_context_tokens: 1_048_576,
            max_output_tokens: 65_536,
        };
    }
    // ── Mistral Large ─────────────────────────────────────────────────────
    if model.contains("mistral-large") {
        return ModelCapabilities {
            supports_temperature: true,
            supports_streaming: true,
            supports_tools: true,
            supports_system_prompt: true,
            max_context_tokens: 262_144,
            max_output_tokens: 32_768,
        };
    }
    // ── Mistral Medium / Small ────────────────────────────────────────────
    if model.starts_with("mistralai/") {
        return ModelCapabilities {
            supports_temperature: true,
            supports_streaming: true,
            supports_tools: true,
            supports_system_prompt: true,
            max_context_tokens: 131_072,
            max_output_tokens: 32_768,
        };
    }
    // ── Qwen 3.6+ / Qwen3 Coder - 1M context ────────────────────────────
    if model.contains("qwen3.6") || model.contains("qwen3-coder") {
        return ModelCapabilities {
            supports_temperature: true,
            supports_streaming: true,
            supports_tools: true,
            supports_system_prompt: true,
            max_context_tokens: 1_048_576,
            max_output_tokens: 65_536,
        };
    }
    // ── Qwen3 general ─────────────────────────────────────────────────────
    if model.starts_with("qwen/") {
        return ModelCapabilities {
            supports_temperature: true,
            supports_streaming: true,
            supports_tools: true,
            supports_system_prompt: true,
            max_context_tokens: 131_072,
            max_output_tokens: 32_768,
        };
    }
    // ── Anthropic models via OpenRouter - inherit direct-provider flags ───
    let anthropic_no_temp = model.contains("claude-opus-4-8")
        || model.contains("claude-opus-4-7")
        || model.contains("claude-fable-5")
        || model.contains("claude-mythos-5");
    if model.starts_with("anthropic/") {
        return ModelCapabilities {
            supports_temperature: !anthropic_no_temp,
            supports_streaming: true,
            supports_tools: true,
            supports_system_prompt: true,
            max_context_tokens: 1_000_000,
            max_output_tokens: 128_000,
        };
    }
    // ── OpenAI o-series via OpenRouter - no temperature ───────────────────
    if model.starts_with("openai/o") {
        return ModelCapabilities {
            supports_temperature: false,
            supports_streaming: true,
            supports_tools: true,
            supports_system_prompt: true,
            max_context_tokens: 200_000,
            max_output_tokens: 100_000,
        };
    }
    // ── OpenAI GPT-5.x via OpenRouter ────────────────────────────────────
    if model.starts_with("openai/gpt-5") {
        return ModelCapabilities {
            supports_temperature: true,
            supports_streaming: true,
            supports_tools: true,
            supports_system_prompt: true,
            max_context_tokens: 1_050_000,
            max_output_tokens: 128_000,
        };
    }
    // ── Conservative fallback for unknown OpenRouter models ───────────────
    FALLBACK_CAPABILITIES
}

/// What an OpenRouter model this build has never heard of is assumed to be.
///
/// Deliberately conservative: guessing high would size regions past what the
/// model accepts and turn a quiet inefficiency into an API error. The cost of
/// guessing low is that percentage budgets resolve against a window eight times
/// smaller than a 1M-token model really has, which is why reaching this is
/// worth saying out loud - see [`OpenRouterProvider::warn_if_unknown`].
const FALLBACK_CAPABILITIES: ModelCapabilities = ModelCapabilities {
    supports_temperature: true,
    supports_streaming: true,
    supports_tools: true,
    supports_system_prompt: true,
    max_context_tokens: 128_000,
    max_output_tokens: 8_192,
};

impl OpenRouterProvider {
    /// Whether this model has already refused a temperature.
    fn temperature_is_unsupported(&self, model: &str) -> bool {
        leviath_core::sync::lock(&self.temperature_unsupported).contains(model)
    }

    /// Record that it did, for the rest of this process.
    fn remember_temperature_unsupported(&self, model: &str) {
        leviath_core::sync::lock(&self.temperature_unsupported).insert(model.to_string());
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
    /// `base` with the sizes OpenRouter reported for this model, if it did.
    ///
    /// Only the two sizes. The rest of `ModelCapabilities` is about how a
    /// request must be *shaped* - whether temperature is accepted, whether
    /// tools work - and `/models` describes what a model is, not the quirks of
    /// talking to it. Taking sizes from the live answer and shape from the
    /// compiled table gives each the question it can answer.
    fn api_corrected(&self, model: &str, base: ModelCapabilities) -> ModelCapabilities {
        let windows = leviath_core::sync::lock(&self.api_windows);
        let Some(api) = windows.get(model) else {
            return base;
        };
        ModelCapabilities {
            max_context_tokens: api.context_length,
            max_output_tokens: api.max_completion_tokens.unwrap_or(base.max_output_tokens),
            ..base
        }
    }

    /// GET `/models`, shared by [`Provider::list_models`] and
    /// [`Provider::prime_capabilities`] so the two cannot disagree about what
    /// the endpoint is or how its failures read.
    async fn fetch_models_json(&self) -> Result<serde_json::Value> {
        let response = self
            .client
            .get(format!("{}/models", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .send()
            .await
            .map_err(|e| ProviderError::RequestFailed(e.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            let error_body = response
                .text()
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
        if leviath_core::sync::lock(&self.api_windows).contains_key(model) {
            return;
        }
        let mut warned = leviath_core::sync::lock(&self.warned_unknown);
        if !warned.insert(model.to_string()) {
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
        let headers = [
            ("Authorization", format!("Bearer {}", self.api_key)),
            // OpenRouter attributes a request to an app by this pair, and
            // sending only the referer left every Leviath call unnamed on
            // the account's activity page.
            ("HTTP-Referer", "https://leviath.dev".to_string()),
            ("X-Title", "Leviath".to_string()),
            ("Content-Type", "application/json".to_string()),
        ];

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
            limiter.record_tokens(result.tokens_used.total_tokens).await;
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
        body["stream"] = serde_json::Value::Bool(true);
        let url = format!("{}/chat/completions", self.base_url);

        let response = send_chat_request(
            &self.client,
            "openrouter",
            &url,
            &[
                ("Authorization", format!("Bearer {}", self.api_key)),
                // OpenRouter attributes a request to an app by this pair, and
                // sending only the referer left every Leviath call unnamed on
                // the account's activity page.
                ("HTTP-Referer", "https://leviath.dev".to_string()),
                ("X-Title", "Leviath".to_string()),
                ("Content-Type", "application/json".to_string()),
            ],
            &body,
            self.rate_limiter.as_ref(),
            request.request_timeout_secs,
        )
        .await?;

        // Reuse OpenAI SSE parser since the format is identical
        let byte_stream = response.bytes_stream();
        let stream = OpenAiSseStream::new(byte_stream);

        Ok(Box::pin(stream))
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
        let base = self.api_corrected(model, builtin_capabilities(model));
        // Merged, not swapped: an entry names only what it corrects.
        match self.capability_overrides.get(model) {
            Some(o) => o.apply_to(base),
            None => {
                self.warn_if_unknown(model, &base);
                base
            }
        }
    }

    async fn prime_capabilities(&self) -> Result<()> {
        let body = self.fetch_models_json().await?;
        let data = body
            .get("data")
            .and_then(|d| d.as_array())
            .ok_or_else(|| ProviderError::InvalidResponse("Missing 'data' array".to_string()))?;

        let mut windows = HashMap::with_capacity(data.len());
        for entry in data {
            let Some(id) = entry.get("id").and_then(|v| v.as_str()) else {
                continue;
            };
            // No `context_length` means the endpoint is telling us nothing
            // useful; skipping leaves the built-in table in charge rather than
            // recording a guess that outranks it.
            let Some(context_length) = entry.get("context_length").and_then(|v| v.as_u64()) else {
                continue;
            };
            windows.insert(
                id.to_string(),
                ApiWindow {
                    context_length: context_length as usize,
                    max_completion_tokens: entry
                        .get("top_provider")
                        .and_then(|tp| tp.get("max_completion_tokens"))
                        .and_then(|v| v.as_u64())
                        .map(|v| v as usize),
                },
            );
        }

        let count = windows.len();
        *leviath_core::sync::lock(&self.api_windows) = windows;
        tracing::debug!(models = count, "learned OpenRouter model windows");
        Ok(())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let body = self.fetch_models_json().await?;

        let data = body
            .get("data")
            .and_then(|d| d.as_array())
            .ok_or_else(|| ProviderError::InvalidResponse("Missing 'data' array".to_string()))?;

        let mut models = Vec::with_capacity(data.len());
        for entry in data {
            let id = entry
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let name = entry
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let context_length = entry
                .get("context_length")
                .and_then(|v| v.as_u64())
                .unwrap_or(128_000) as usize;
            let max_completion_tokens = entry
                .get("top_provider")
                .and_then(|tp| tp.get("max_completion_tokens"))
                .and_then(|v| v.as_u64())
                .map(|v| v as usize);

            let base_caps = self.capabilities(&id);
            let capabilities = ModelCapabilities {
                max_context_tokens: context_length,
                max_output_tokens: max_completion_tokens.unwrap_or(8192),
                ..base_caps
            };

            models.push(ModelInfo {
                id,
                display_name: name,
                provider: "openrouter".into(),
                capabilities,
            });
        }

        Ok(models)
    }
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn test_with_config_default_url() {
        let config = ProviderConfig {
            api_key: "key".to_string(),
            base_url: None,
            rate_limit: None,
            request_timeout_secs: None,
        };
        let provider = OpenRouterProvider::with_config(
            crate::provider::build_http_client(None).expect("a test client builds"),
            config,
        );
        assert_eq!(provider.base_url, "https://openrouter.ai/api/v1");
    }

    #[test]
    fn test_with_config_custom_url() {
        let config = ProviderConfig {
            api_key: "key".to_string(),
            base_url: Some("https://custom.openrouter.ai".to_string()),
            rate_limit: None,
            request_timeout_secs: None,
        };
        let provider = OpenRouterProvider::with_config(
            crate::provider::build_http_client(None).expect("a test client builds"),
            config,
        );
        assert_eq!(provider.base_url, "https://custom.openrouter.ai");
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
        for model in ["moonshotai/kimi-k3", "meta/muse-spark-1.2"] {
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
            provider.capabilities("meta/muse-spark-1.2");
        }
        let warned = leviath_core::sync::lock(&provider.warned_unknown);
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
            leviath_core::sync::lock(&provider.warned_unknown).is_empty(),
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
        OpenRouterProvider::with_config(
            crate::provider::build_http_client(None).expect("a test client builds"),
            ProviderConfig {
                api_key: "test-key".to_string(),
                base_url: Some(url),
                rate_limit: None,
                request_timeout_secs: None,
            },
        )
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

    /// Sizes come from the API; how a request must be shaped stays with the
    /// compiled table, which is the only thing that knows it.
    #[tokio::test]
    async fn priming_leaves_request_shape_alone() {
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
            !leviath_core::sync::lock(&provider.warned_unknown).contains("some/128k-model"),
            "the API answered for this model, so nothing was assumed"
        );
        // The control: a model it said nothing about is still reported.
        let _ = provider.capabilities("nobody/knows");
        assert!(
            leviath_core::sync::lock(&provider.warned_unknown).contains("nobody/knows"),
            "a model with no answer anywhere is still called out"
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
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "openai/gpt-4o");
        assert_eq!(models[0].display_name, Some("GPT-4o".to_string()));
        assert_eq!(models[0].capabilities.max_output_tokens, 16384);
        assert_eq!(models[1].id, "anthropic/claude-3");
        assert_eq!(models[1].display_name, None);
        assert_eq!(models[1].capabilities.max_output_tokens, 8192);
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
