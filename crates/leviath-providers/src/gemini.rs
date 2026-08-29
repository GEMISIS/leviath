//! Google Gemini provider implementation (via OpenAI-compatible endpoint).

use crate::learned::{LearnedModel, LearnedModels};
use crate::openai_compat::{
    build_openai_request_body, openai_sse_stream, parse_openai_response, send_chat_request,
};
use crate::provider::{
    InferenceRequest, InferenceResponse, LimitsSource, ModelCapabilities, ModelCapabilityOverride,
    ModelInfo, Provider, ProviderError, Result, StreamChunk,
};
use crate::rate_limit::RateLimiter;
use async_trait::async_trait;
use futures_core::Stream;
use std::collections::HashMap;
use std::pin::Pin;

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
    ModelCapabilities {
        supports_temperature: true,
        supports_streaming: true,
        supports_tools: true,
        supports_system_prompt: true,
        max_context_tokens,
        max_output_tokens,
        limits_source: LimitsSource::Builtin,
    }
}

/// The models this build names when the listing cannot be read, as
/// `(id, display name)`.
pub(crate) const CATALOG: &[(&str, &str)] = &[
    ("gemini-3.5-flash", "Gemini 3.5 Flash"),
    ("gemini-3.1-pro-preview", "Gemini 3.1 Pro (preview)"),
    ("gemini-3-flash", "Gemini 3 Flash"),
    ("gemini-3.1-flash-lite", "Gemini 3.1 Flash Lite"),
];

/// Google Gemini provider using the OpenAI-compatible endpoint.
pub struct GeminiProvider {
    /// HTTP client
    client: reqwest::Client,

    /// API key
    api_key: String,

    /// API base URL
    base_url: String,

    /// Rate limiter
    rate_limiter: Option<RateLimiter>,

    /// What the native `/v1beta/models` listing said about each model, filled
    /// by [`Provider::prime_capabilities`].
    ///
    /// The listing has always read the two limits - `inputTokenLimit` and
    /// `outputTokenLimit` - and only ever handed them to the model picker. The
    /// runtime sizes percentage region budgets through the sync
    /// `capabilities()` path, which could not await a fetch and so answered
    /// from a table of family defaults matched off the model's name. The
    /// authoritative numbers were being fetched and thrown away, and so was
    /// `maxTemperature`, which says whether a model samples at all.
    learned: LearnedModels,

    /// Per-model capability overrides
    capability_overrides: HashMap<String, ModelCapabilityOverride>,
}

impl GeminiProvider {
    /// Create a new Gemini provider.
    pub fn new(client: reqwest::Client, api_key: String) -> Self {
        Self {
            client,
            api_key,
            base_url: "https://generativelanguage.googleapis.com/v1beta/openai".to_string(),
            rate_limiter: None,
            learned: Default::default(),
            capability_overrides: HashMap::new(),
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
            client,
            api_key,
            base_url: "https://generativelanguage.googleapis.com/v1beta/openai".to_string(),
            rate_limiter: rate_limit.map(crate::rate_limit::RateLimiter::new),
            capability_overrides: overrides,
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

    /// Return built-in capability defaults for a model, by family.
    ///
    /// Current model families all share 1M context / 65K output / full
    /// tool+streaming support, so the values are identical today. The per-family
    /// branching exists so a family can diverge (e.g. a smaller flash-lite output
    /// cap, or a future `supports_thinking` flag) without another refactor.
    /// `list_models` fetches the *authoritative* per-model limits from the native
    /// API; this is the offline default used for the sync `capabilities()` path.
    /// - gemini-3.5-flash (latest flash, near-Pro intelligence)
    /// - gemini-3.1-pro-preview (latest pro, reasoning-first)
    /// - gemini-3-flash (complex multimodal/agentic)
    /// - gemini-3.1-flash-lite (cost-efficient, high-volume)
    fn builtin_capabilities(&self, model: &str) -> ModelCapabilities {
        table_capabilities(model)
    }

    /// Derive the native Gemini API base (`.../v1beta`) from the OpenAI-compatible
    /// base (`.../v1beta/openai`). Native-only endpoints (`:countTokens`, the
    /// per-model `models` listing) live under the former. Returns `None` when the
    /// configured base doesn't follow the `/openai` convention (e.g. a custom
    /// proxy), so callers fall back rather than guessing a wrong URL.
    fn native_base(&self) -> Option<String> {
        self.base_url.strip_suffix("/openai").map(|s| s.to_string())
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
        let native = self.native_base().ok_or_else(|| {
            ProviderError::Other(
                "non-standard base_url; native countTokens unavailable".to_string(),
            )
        })?;
        let url = format!("{}/models/{}:countTokens", native, model);
        let body = serde_json::json!({
            "contents": [{ "role": "user", "parts": [{ "text": text }] }],
        });
        let response = send_chat_request(
            crate::provider::side_call_client(),
            "gemini",
            &url,
            &[
                ("x-goog-api-key", self.api_key.clone()),
                ("Content-Type", "application/json".to_string()),
            ],
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
        tracing::debug!(model = %request.model, "Calling Gemini API");

        if let Some(limiter) = &self.rate_limiter {
            limiter.acquire().await?;
        }

        let body = build_openai_request_body(request);
        let url = format!("{}/chat/completions", self.base_url);

        let response = send_chat_request(
            &self.client,
            "gemini",
            &url,
            &[
                ("Authorization", format!("Bearer {}", self.api_key)),
                ("Content-Type", "application/json".to_string()),
            ],
            &body,
            self.rate_limiter.as_ref(),
            request.request_timeout_secs,
        )
        .await?;

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
        tracing::debug!(model = %request.model, "Calling Gemini API (streaming)");

        if let Some(limiter) = &self.rate_limiter {
            limiter.acquire().await?;
        }

        let mut body = build_openai_request_body(request);
        crate::openai_compat::make_streaming(&mut body);
        let url = format!("{}/chat/completions", self.base_url);

        let response = send_chat_request(
            &self.client,
            "gemini",
            &url,
            &[
                ("Authorization", format!("Bearer {}", self.api_key)),
                ("Content-Type", "application/json".to_string()),
            ],
            &body,
            self.rate_limiter.as_ref(),
            request.request_timeout_secs,
        )
        .await?;

        let byte_stream = response.bytes_stream();
        let stream = openai_sse_stream(byte_stream);

        Ok(Box::pin(stream))
    }

    async fn count_tokens(&self, text: &str, model: &str) -> usize {
        // Through the limiter first, the way `infer` is: the count endpoint
        // spends the same request quota. Then prefer Gemini's exact native
        // `:countTokens` endpoint, and fall back to the local heuristic on any
        // error (network, non-2xx, parse, non-standard base).
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
        (model_key.starts_with("gemini") || self.capability_overrides.contains_key(model_key))
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

    fn capabilities(&self, model: &str) -> ModelCapabilities {
        let base = self
            .learned
            .corrected(model, self.builtin_capabilities(model));
        // Merged, not swapped: an entry names only what it corrects.
        match self.capability_overrides.get(model) {
            Some(o) => o.apply_to(base),
            None => base,
        }
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
    ///
    /// A compat base URL has no native listing, so nothing is learned and the
    /// family defaults stay in charge - the same outcome as an unreachable API,
    /// and reported the same way by `limits_source`.
    async fn prime_capabilities(&self) -> Result<()> {
        let Some(native) = self.native_base() else {
            return Ok(());
        };
        let learned = self.fetch_native_catalog(&native).await?;
        let count = learned.len();
        self.learned.replace(learned);
        tracing::debug!(models = count, "learned Gemini model capabilities");
        Ok(())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        // Prefer the native `/v1beta/models` listing: unlike the OpenAI-compat
        // `/models`, it returns real per-model limits and flags, and it is
        // answered from the primed store so it cannot disagree with what an
        // inference is told. Fall back to the compat listing (with builtin
        // caps) when the base URL isn't the standard `.../openai` form.
        if self.native_base().is_none() {
            return self.list_models_compat().await;
        }
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
    if let Some(methods) = item
        .get("supportedGenerationMethods")
        .and_then(|v| v.as_array())
        && !methods
            .iter()
            .any(|m| m.as_str() == Some("generateContent"))
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
        },
    ))
}

impl GeminiProvider {
    /// GET a models listing and return its body.
    ///
    /// The native and OpenAI-compatible listings differ in the auth header
    /// they take and the key the array sits under, and in nothing else about
    /// the request, so this is the one place the request is made.
    async fn fetch_model_listing(
        &self,
        url: String,
        auth: (&str, String),
    ) -> Result<serde_json::Value> {
        let response = crate::provider::apply_request_timeout(
            self.client.get(url).header(auth.0, auth.1),
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
    async fn fetch_native_catalog(
        &self,
        native_base: &str,
    ) -> Result<HashMap<String, LearnedModel>> {
        let mut learned = HashMap::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut url = format!("{}/models?pageSize={}", native_base, NATIVE_PAGE_SIZE);
            if let Some(token) = &page_token {
                url.push_str("&pageToken=");
                url.push_str(token);
            }
            let body = self
                .fetch_model_listing(url, ("x-goog-api-key", self.api_key.clone()))
                .await?;
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

    /// OpenAI-compat `/models` listing (no per-model token limits) used only when
    /// the configured base URL isn't the standard native-derivable form.
    async fn list_models_compat(&self) -> Result<Vec<ModelInfo>> {
        let body = self
            .fetch_model_listing(
                format!("{}/models", self.base_url),
                ("Authorization", format!("Bearer {}", self.api_key)),
            )
            .await?;
        let models = Self::listing_array(&body, "data")?
            .iter()
            .filter_map(|item| {
                let id = item.get("id")?.as_str()?.to_string();
                let capabilities = self.capabilities(&id);
                Some(ModelInfo::new(id, "google", capabilities))
            })
            .collect();

        Ok(models)
    }
}

#[cfg(test)]
mod tests {
    use crate::provider::FinishReason;

    /// Config beats the shipped table, and an unconfigured model still gets the
    /// published rate rather than falling to unpriced.
    #[test]
    fn pricing_prefers_config_then_the_published_table() {
        let mut overrides = std::collections::HashMap::new();
        overrides.insert(
            "gemini-3.5-flash".to_string(),
            crate::ModelCapabilityOverride {
                input_per_mtok: Some(1.0),
                output_per_mtok: Some(2.0),
                ..Default::default()
            },
        );
        let provider = GeminiProvider::with_overrides(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "k".to_string(),
            overrides,
            None,
        );

        // Configured: the operator's number, not the table's.
        let configured = provider.pricing("gemini-3.5-flash").expect("configured");
        assert_eq!(configured.input_per_mtok, 1.0);
        assert_eq!(configured.output_per_mtok, 2.0);

        // Not configured: the published rate.
        let listed = provider
            .pricing("gemini-3.1-pro-preview")
            .expect("in the table");
        assert_eq!(listed.input_per_mtok, 2.0);

        // Neither: unpriced, so the run reports its cost unavailable.
        assert_eq!(provider.pricing("no-such-model-9"), None);
    }
    use super::*;
    use crate::test_support::always_on_tracing_guard;
    use leviath_testkit::{spawn_mock_server, spawn_mock_server_truncated_body};

    /// The gap this closes: the native listing has always read the real limits
    /// and only ever handed them to the model picker, while the runtime sized
    /// its percentage region budgets from a table matched off the model's name.
    #[tokio::test]
    async fn priming_teaches_capabilities_what_the_listing_already_knew() {
        let body = br#"{"models":[
            {"name":"models/gemini-3.5-flash","displayName":"Flash",
             "inputTokenLimit":2000000,"outputTokenLimit":8192}
        ]}"#;
        let url = leviath_testkit::spawn_mock_server(200, "OK", body).await;
        let provider = GeminiProvider::new(reqwest::Client::new(), "k".to_string())
            .with_base_url(Some(format!("{url}/v1beta/openai")));

        let before = provider.capabilities("gemini-3.5-flash");
        assert_eq!(
            before.limits_source,
            LimitsSource::Builtin,
            "unprimed, the family default is all there is"
        );

        provider.prime_capabilities().await.expect("primes");

        let after = provider.capabilities("gemini-3.5-flash");
        assert_eq!(after.max_context_tokens, 2_000_000);
        assert_eq!(after.max_output_tokens, 8_192);
        assert_eq!(after.limits_source, LimitsSource::Api);
    }

    /// An entry the listing said nothing about is not stored as if it had. The
    /// listing starts each model from the family defaults, so recording those
    /// would relabel a guess as authoritative - worse than the guess, because
    /// nothing downstream would know to doubt it.
    #[tokio::test]
    async fn a_model_the_listing_gave_no_limits_for_is_not_learned() {
        let body = br#"{"models":[{"name":"models/gemini-bare","displayName":"Bare"}]}"#;
        let url = leviath_testkit::spawn_mock_server(200, "OK", body).await;
        let provider = GeminiProvider::new(reqwest::Client::new(), "k".to_string())
            .with_base_url(Some(format!("{url}/v1beta/openai")));

        provider.prime_capabilities().await.expect("primes");

        let caps = provider.capabilities("gemini-bare");
        assert_eq!(
            caps.limits_source,
            LimitsSource::Builtin,
            "nothing was reported, so nothing is claimed"
        );
    }

    /// An operator's entry is the last word, and says so, because someone who
    /// wrote the number down is usually correcting exactly what the API said.
    #[tokio::test]
    async fn an_operator_override_outranks_the_api() {
        let body = br#"{"models":[
            {"name":"models/gemini-3.5-flash","inputTokenLimit":2000000,"outputTokenLimit":8192}
        ]}"#;
        let url = leviath_testkit::spawn_mock_server(200, "OK", body).await;
        let mut overrides = HashMap::new();
        overrides.insert(
            "gemini-3.5-flash".to_string(),
            ModelCapabilityOverride {
                max_context_tokens: Some(64_000),
                ..Default::default()
            },
        );
        let provider = GeminiProvider::with_overrides(
            reqwest::Client::new(),
            "k".to_string(),
            overrides,
            None,
        )
        .with_base_url(Some(format!("{url}/v1beta/openai")));

        provider.prime_capabilities().await.expect("primes");

        let caps = provider.capabilities("gemini-3.5-flash");
        assert_eq!(caps.max_context_tokens, 64_000, "the operator's number");
        assert_eq!(
            caps.max_output_tokens, 8_192,
            "and the API's for what the operator did not name"
        );
        assert_eq!(caps.limits_source, LimitsSource::Override);
    }
    #[test]
    fn test_provider_name() {
        let provider = GeminiProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        assert_eq!(provider.name(), "google");
    }

    #[test]
    fn test_default_base_url() {
        let provider = GeminiProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        assert_eq!(
            provider.base_url,
            "https://generativelanguage.googleapis.com/v1beta/openai"
        );
    }

    #[test]
    fn test_builtin_capabilities_gemini_35_flash() {
        let provider = GeminiProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let caps = provider.builtin_capabilities("gemini-3.5-flash");
        assert!(caps.supports_temperature);
        assert!(caps.supports_streaming);
        assert!(caps.supports_tools);
        assert!(caps.supports_system_prompt);
        assert_eq!(caps.max_context_tokens, 1_048_576);
        assert_eq!(caps.max_output_tokens, 65_535);
    }

    #[test]
    fn test_builtin_capabilities_gemini_31_pro() {
        let provider = GeminiProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let caps = provider.builtin_capabilities("gemini-3.1-pro-preview");
        assert!(caps.supports_temperature);
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 1_048_576);
        assert_eq!(caps.max_output_tokens, 65_535);
    }

    #[test]
    fn test_builtin_capabilities_gemini_3_flash() {
        let provider = GeminiProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let caps = provider.builtin_capabilities("gemini-3-flash");
        assert!(caps.supports_temperature);
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 1_048_576);
        assert_eq!(caps.max_output_tokens, 65_535);
    }

    #[test]
    fn test_builtin_capabilities_gemini_31_flash_lite() {
        let provider = GeminiProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let caps = provider.builtin_capabilities("gemini-3.1-flash-lite");
        assert!(caps.supports_temperature);
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 1_048_576);
        assert_eq!(caps.max_output_tokens, 65_535);
    }

    #[test]
    fn test_default_capabilities() {
        let provider = GeminiProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let caps = provider.builtin_capabilities("gemini-future-model");
        assert!(caps.supports_temperature);
        assert!(caps.supports_streaming);
        assert!(caps.supports_tools);
        assert!(caps.supports_system_prompt);
        assert_eq!(caps.max_context_tokens, 1_048_576);
        assert_eq!(caps.max_output_tokens, 65_535);
    }

    #[test]
    fn test_capabilities_override() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "gemini-3.5-flash".to_string(),
            ModelCapabilities {
                supports_temperature: false,
                supports_streaming: false,
                supports_tools: false,
                supports_system_prompt: false,
                max_context_tokens: 1,
                max_output_tokens: 1,
                limits_source: LimitsSource::Builtin,
            }
            .into(),
        );
        let provider = GeminiProvider::with_overrides(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
            overrides,
            None,
        );
        let caps = provider.capabilities("gemini-3.5-flash");
        assert!(!caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 1);
    }

    #[test]
    fn test_build_request_body() {
        let request = InferenceRequest {
            system: vec![],
            messages: vec![
                crate::provider::Message {
                    role: "system".to_string(),
                    content: "You are helpful.".into(),
                    cache_breakpoint: false,
                },
                crate::provider::Message {
                    role: "user".to_string(),
                    content: "Hello".into(),
                    cache_breakpoint: false,
                },
            ],
            model: "gemini-3.5-flash".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = build_openai_request_body(&request);
        assert_eq!(body["model"], "gemini-3.5-flash");
        assert_eq!(body["messages"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_parse_response() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "Hello!"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15
            }
        });

        let response = parse_openai_response(&body).unwrap();
        assert_eq!(response.content, "Hello!");
        assert_eq!(response.tokens_used.prompt_tokens, 10);
        assert_eq!(response.finish_reason, FinishReason::Complete);
    }

    #[test]
    fn test_parse_response_with_tool_calls() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_123",
                        "type": "function",
                        "function": {
                            "name": "search",
                            "arguments": "{\"query\": \"rust\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 20, "completion_tokens": 15, "total_tokens": 35 }
        });

        let response = parse_openai_response(&body).unwrap();
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].name, "search");
        assert_eq!(response.finish_reason, FinishReason::ToolCall);
    }

    #[test]
    fn test_context_limits() {
        let provider = GeminiProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        assert_eq!(provider.max_context_tokens("gemini-3.5-flash"), 1_048_576);
        assert_eq!(
            provider.max_context_tokens("gemini-3.1-pro-preview"),
            1_048_576
        );
    }

    // ── Additional coverage tests ──────────────────────────────────────────

    /// Provider whose native base is a non-standard URL (no `/openai` suffix),
    /// so `count_tokens` skips the endpoint and uses the local heuristic.
    fn heuristic_only_provider() -> GeminiProvider {
        GeminiProvider {
            client: reqwest::Client::new(),
            api_key: "key".to_string(),
            base_url: "http://127.0.0.1:19997".to_string(),
            rate_limiter: None,
            learned: Default::default(),
            capability_overrides: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn test_count_tokens_heuristic_fallback() {
        let provider = heuristic_only_provider();
        // 8 chars / 4 = 2 (gemini heuristic branch)
        let tokens = provider.count_tokens("12345678", "gemini-3.5-flash").await;
        assert_eq!(tokens, 2);
    }

    #[tokio::test]
    async fn test_count_tokens_empty() {
        let provider = heuristic_only_provider();
        let tokens = provider.count_tokens("", "gemini-3.5-flash").await;
        assert_eq!(tokens, 0);
    }

    #[tokio::test]
    async fn test_count_tokens_uses_exact_endpoint() {
        let base = spawn_mock_server(200, "OK", br#"{"totalTokens": 99}"#).await;
        let provider = GeminiProvider {
            client: reqwest::Client::new(),
            api_key: "key".to_string(),
            base_url: format!("{}/openai", base),
            rate_limiter: None,
            learned: Default::default(),
            capability_overrides: HashMap::new(),
        };
        let tokens = provider.count_tokens("anything", "gemini-3.5-flash").await;
        assert_eq!(tokens, 99);
    }

    /// See the Anthropic twin: the native `countTokens` call spends the
    /// provider's request budget, so a limiter allowing one request a minute
    /// holds the second count.
    #[tokio::test]
    async fn the_count_call_goes_through_the_rate_limiter() {
        let (base, _bodies) = leviath_testkit::spawn_mock_sequence(vec![
            (200, "OK", br#"{"totalTokens": 9}"#.to_vec()),
            (200, "OK", br#"{"totalTokens": 9}"#.to_vec()),
        ])
        .await;
        let provider = GeminiProvider {
            client: reqwest::Client::new(),
            api_key: "key".to_string(),
            base_url: format!("{}/openai", base),
            rate_limiter: Some(RateLimiter::new(&crate::provider::RateLimitConfig {
                requests_per_minute: 1,
                tokens_per_minute: 1_000_000,
            })),
            learned: Default::default(),
            capability_overrides: HashMap::new(),
        };
        assert_eq!(provider.count_tokens("first", "gemini-3.5-flash").await, 9);
        let held = tokio::time::timeout(
            std::time::Duration::from_millis(300),
            provider.count_tokens("second", "gemini-3.5-flash"),
        )
        .await;
        assert!(
            held.is_err(),
            "the second count waits for the minute the limiter allows one request in"
        );
    }

    #[tokio::test]
    async fn test_count_tokens_falls_back_on_error_status() {
        let base = spawn_mock_server(500, "Internal Server Error", b"boom").await;
        let provider = GeminiProvider {
            client: reqwest::Client::new(),
            api_key: "key".to_string(),
            base_url: format!("{}/openai", base),
            rate_limiter: None,
            learned: Default::default(),
            capability_overrides: HashMap::new(),
        };
        // 8 chars / 4 = 2 (heuristic fallback)
        let tokens = provider.count_tokens("12345678", "gemini-3.5-flash").await;
        assert_eq!(tokens, 2);
    }

    #[tokio::test]
    async fn test_count_tokens_falls_back_on_connection_error() {
        // Base ends in `/openai` so `native_base` resolves, but the port is dead:
        // the POST fails at send() → RequestFailed → heuristic fallback.
        let provider = GeminiProvider {
            client: reqwest::Client::new(),
            api_key: "key".to_string(),
            base_url: "http://127.0.0.1:19997/openai".to_string(),
            rate_limiter: None,
            learned: Default::default(),
            capability_overrides: HashMap::new(),
        };
        let tokens = provider.count_tokens("12345678", "gemini-3.5-flash").await;
        assert_eq!(tokens, 2);
    }

    #[tokio::test]
    async fn test_count_tokens_falls_back_on_malformed_json() {
        let base = spawn_mock_server(200, "OK", b"not json").await;
        let provider = GeminiProvider {
            client: reqwest::Client::new(),
            api_key: "key".to_string(),
            base_url: format!("{}/openai", base),
            rate_limiter: None,
            learned: Default::default(),
            capability_overrides: HashMap::new(),
        };
        let tokens = provider.count_tokens("12345678", "gemini-3.5-flash").await;
        assert_eq!(tokens, 2);
    }

    #[tokio::test]
    async fn test_count_tokens_falls_back_on_missing_total_tokens() {
        let base = spawn_mock_server(200, "OK", br#"{"unexpected": true}"#).await;
        let provider = GeminiProvider {
            client: reqwest::Client::new(),
            api_key: "key".to_string(),
            base_url: format!("{}/openai", base),
            rate_limiter: None,
            learned: Default::default(),
            capability_overrides: HashMap::new(),
        };
        let tokens = provider.count_tokens("12345678", "gemini-3.5-flash").await;
        assert_eq!(tokens, 2);
    }

    #[test]
    fn test_capabilities_override_takes_precedence() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "gemini-custom".to_string(),
            ModelCapabilities {
                supports_temperature: false,
                supports_streaming: false,
                supports_tools: false,
                supports_system_prompt: false,
                max_context_tokens: 42,
                max_output_tokens: 10,
                limits_source: LimitsSource::Builtin,
            }
            .into(),
        );
        let provider = GeminiProvider::with_overrides(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
            overrides,
            None,
        );
        let caps = provider.capabilities("gemini-custom");
        assert_eq!(caps.max_context_tokens, 42);
        assert!(!caps.supports_temperature);
    }

    #[test]
    fn test_capabilities_builtin_fallthrough() {
        let provider = GeminiProvider::with_overrides(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
            HashMap::new(),
            None,
        );
        let caps = provider.capabilities("gemini-3.5-flash");
        assert_eq!(caps.max_context_tokens, 1_048_576);
    }

    #[test]
    fn test_max_context_tokens_delegates() {
        let provider = GeminiProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        assert_eq!(provider.max_context_tokens("gemini-3.5-flash"), 1_048_576);
    }

    #[test]
    fn test_parse_response_no_choices() {
        let body = serde_json::json!({});
        let result = parse_openai_response(&body);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_response_finish_reason_length() {
        let body = serde_json::json!({
            "choices": [{
                "message": { "content": "truncated" },
                "finish_reason": "length"
            }],
            "usage": { "prompt_tokens": 5, "completion_tokens": 100 }
        });
        let response = parse_openai_response(&body).unwrap();
        assert_eq!(response.finish_reason, FinishReason::TokenLimit);
    }

    // ─── HTTP-call-level tests via a raw-TCP mock server ───────────────────

    fn provider_with_url(url: String) -> GeminiProvider {
        GeminiProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        )
        .with_base_url(Some(url))
    }

    #[test]
    fn with_base_url_keeps_the_default_when_none_is_given() {
        let provider = GeminiProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        )
        .with_base_url(None);
        assert_eq!(
            provider.base_url,
            "https://generativelanguage.googleapis.com/v1beta/openai"
        );
    }

    #[test]
    fn with_base_url_replaces_the_default() {
        let provider = GeminiProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        )
        .with_base_url(Some("https://custom.example.com".to_string()));
        assert_eq!(provider.base_url, "https://custom.example.com");
    }

    #[test]
    fn with_overrides_installs_a_rate_limiter_when_given_one() {
        let provider = GeminiProvider::with_overrides(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
            HashMap::new(),
            Some(&crate::provider::RateLimitConfig {
                requests_per_minute: 10,
                tokens_per_minute: 50_000,
            }),
        );
        assert!(provider.rate_limiter.is_some());
    }

    fn simple_request() -> InferenceRequest {
        InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "hi".into(),
                cache_breakpoint: false,
            }],
            model: "gemini-3.5-flash".to_string(),
            max_tokens: 100,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        }
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

    #[tokio::test]
    async fn infer_non_success_status_returns_error() {
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
    async fn infer_stream_non_success_status_returns_error() {
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

    #[tokio::test]
    async fn list_models_success_returns_models() {
        let body = br#"{"data":[{"id":"gemini-3.5-flash"}]}"#;
        let url = spawn_mock_server(200, "OK", body).await;
        let provider = provider_with_url(url);
        let models = provider.list_models().await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gemini-3.5-flash");
        assert_eq!(models[0].provider, "google");
    }

    #[tokio::test]
    async fn list_models_non_success_status_returns_error() {
        let url = spawn_mock_server(401, "Unauthorized", b"bad key").await;
        let provider = provider_with_url(url);
        let err = provider.list_models().await.unwrap_err();
        assert!(err.to_string().contains("401"));
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

    #[tokio::test]
    async fn list_models_skips_entries_without_id() {
        let body = br#"{"data":[{"no_id": true}, {"id":"valid-model"}]}"#;
        let url = spawn_mock_server(200, "OK", body).await;
        let provider = provider_with_url(url);
        let models = provider.list_models().await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "valid-model");
    }

    #[tokio::test]
    async fn list_models_skips_entries_with_non_string_id() {
        // covers the `.as_str()?` None branch in the filter_map
        let body = br#"{"data":[{"id": 42}, {"id":"valid-model"}]}"#;
        let url = spawn_mock_server(200, "OK", body).await;
        let provider = provider_with_url(url);
        let models = provider.list_models().await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "valid-model");
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
    async fn list_models_non_success_body_read_error_falls_back_to_status() {
        // A truncated body makes reading the error text fail; `check_http_response`
        // still reports the status (falling back to the reqwest error string).
        let url = spawn_mock_server_truncated_body(500, "Internal Server Error").await;
        let provider = provider_with_url(url);
        let err = provider.list_models().await.unwrap_err();
        assert!(err.to_string().contains("500"));
    }

    // ─── Native models listing (real per-model token limits) ──────────────

    /// Provider whose base ends in `/openai`, so `native_base()` resolves and
    /// `list_models` takes the native path.
    fn native_provider_with_base(base: &str) -> GeminiProvider {
        GeminiProvider {
            client: reqwest::Client::new(),
            api_key: "test-key".to_string(),
            base_url: format!("{}/openai", base),
            rate_limiter: None,
            learned: Default::default(),
            capability_overrides: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn list_models_native_uses_real_token_limits() {
        // `name` carries the "models/" prefix; the API returns authoritative
        // per-model limits that override the builtin family defaults.
        let body = br#"{"models":[
            {"name":"models/gemini-3.5-flash","displayName":"Flash","inputTokenLimit":2000000,"outputTokenLimit":8192}
        ]}"#;
        let base = spawn_mock_server(200, "OK", body).await;
        let provider = native_provider_with_base(&base);
        let models = provider.list_models().await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gemini-3.5-flash");
        assert_eq!(models[0].display_name.as_deref(), Some("Flash"));
        assert_eq!(models[0].capabilities.max_context_tokens, 2_000_000);
        assert_eq!(models[0].capabilities.max_output_tokens, 8192);
    }

    #[tokio::test]
    async fn list_models_native_falls_back_to_builtin_when_limits_absent() {
        // No limit fields → builtin family defaults are kept.
        let body = br#"{"models":[{"name":"models/gemini-3.1-pro-preview"}]}"#;
        let base = spawn_mock_server(200, "OK", body).await;
        let provider = native_provider_with_base(&base);
        let models = provider.list_models().await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gemini-3.1-pro-preview");
        assert_eq!(models[0].capabilities.max_context_tokens, 1_048_576);
        assert_eq!(models[0].capabilities.max_output_tokens, 65_535);
    }

    #[tokio::test]
    async fn list_models_native_id_without_models_prefix_is_used_verbatim() {
        // A `name` lacking the "models/" prefix is used as-is (unwrap_or branch).
        let body = br#"{"models":[{"name":"gemini-bare","inputTokenLimit":500000}]}"#;
        let base = spawn_mock_server(200, "OK", body).await;
        let provider = native_provider_with_base(&base);
        let models = provider.list_models().await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gemini-bare");
        assert_eq!(models[0].capabilities.max_context_tokens, 500_000);
    }

    #[tokio::test]
    async fn list_models_native_connection_error() {
        // `/openai` base resolves native, dead port → RequestFailed.
        let provider = GeminiProvider {
            client: reqwest::Client::new(),
            api_key: "test-key".to_string(),
            base_url: "http://127.0.0.1:19997/openai".to_string(),
            rate_limiter: None,
            learned: Default::default(),
            capability_overrides: HashMap::new(),
        };
        let err = provider.list_models().await.unwrap_err();
        assert!(err.to_string().contains("Request failed:"));
    }

    #[tokio::test]
    async fn list_models_native_malformed_json_errors() {
        let base = spawn_mock_server(200, "OK", b"not json").await;
        let provider = native_provider_with_base(&base);
        let err = provider.list_models().await.unwrap_err();
        assert!(err.to_string().contains("Invalid response:"));
    }

    #[tokio::test]
    async fn list_models_native_non_success_status_errors() {
        // A non-2xx from the native endpoint propagates via check_http_response.
        let base = spawn_mock_server(401, "Unauthorized", b"bad key").await;
        let provider = native_provider_with_base(&base);
        let err = provider.list_models().await.unwrap_err();
        assert!(err.to_string().contains("401"));
    }

    #[tokio::test]
    async fn list_models_native_missing_models_field_errors() {
        let base = spawn_mock_server(200, "OK", b"{}").await;
        let provider = native_provider_with_base(&base);
        let err = provider.list_models().await.unwrap_err();
        assert!(err.to_string().contains("Invalid response:"));
    }

    #[tokio::test]
    async fn list_models_native_skips_entries_without_name() {
        // First entry has no `name` (get None), second's `name` is a non-string
        // (as_str None) - both filtered out; only the valid third survives.
        let body =
            br#"{"models":[{"no_name":true},{"name":123},{"name":"models/gemini-3-flash"}]}"#;
        let base = spawn_mock_server(200, "OK", body).await;
        let provider = native_provider_with_base(&base);
        let models = provider.list_models().await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gemini-3-flash");
    }

    #[test]
    fn native_base_strips_openai_suffix() {
        let provider = GeminiProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "k".to_string(),
        );
        assert_eq!(
            provider.native_base().as_deref(),
            Some("https://generativelanguage.googleapis.com/v1beta")
        );
    }

    #[test]
    fn native_base_none_for_nonstandard_url() {
        let provider = provider_with_url("http://proxy.local/custom".to_string());
        assert!(provider.native_base().is_none());
    }

    #[test]
    fn gemini_family_classification() {
        assert_eq!(
            GeminiFamily::classify("gemini-3.1-flash-lite"),
            GeminiFamily::FlashLite
        );
        assert_eq!(
            GeminiFamily::classify("gemini-3.1-pro-preview"),
            GeminiFamily::Pro
        );
        assert_eq!(
            GeminiFamily::classify("gemini-3.5-flash"),
            GeminiFamily::Flash
        );
        assert_eq!(GeminiFamily::classify("gemini-future"), GeminiFamily::Other);
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
        let limited = GeminiProvider::with_overrides(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "k".to_string(),
            HashMap::new(),
            Some(&cfg),
        );
        assert!(limited.rate_limiter.is_some());
        let unlimited = GeminiProvider::with_overrides(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "k".to_string(),
            HashMap::new(),
            None,
        );
        assert!(unlimited.rate_limiter.is_none());
    }

    /// This vendor claims the models it serves, and declines the rest.
    ///
    /// `serves_model` is what decides where a bare model name resolves, so a
    /// provider that over-claims wins a model it cannot run. Deciding it from
    /// the capability table did exactly that: the table answers how big a
    /// context window to assume, its fallback for an unknown model is a guess,
    /// and a guess is indistinguishable from a real entry. Measured, `google`
    /// claimed `claude-opus-5`.
    #[test]
    fn it_claims_its_own_models_and_no_one_elses() {
        let provider = GeminiProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "k".to_string(),
        );
        assert_eq!(
            provider.serves_model("gemini-3.1-pro-preview"),
            Some("gemini-3.1-pro-preview".to_string()),
            "its own model"
        );
        assert!(
            provider.serves_model("claude-opus-5").is_none(),
            "claude-opus-5 belongs to another vendor"
        );
        assert!(
            provider.serves_model("gpt-5.5").is_none(),
            "gpt-5.5 belongs to another vendor"
        );
        assert!(
            provider.serves_model("grok-4.6").is_none(),
            "grok-4.6 belongs to another vendor"
        );
        assert!(
            provider.serves_model("not-a-real-model-xyz").is_none(),
            "a model nobody has"
        );
    }
}

#[cfg(test)]
mod learned_tests {
    use super::*;
    use leviath_testkit::spawn_mock_sequence;

    fn native_provider_at(base: &str) -> GeminiProvider {
        GeminiProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        )
        .with_base_url(Some(format!("{base}/openai")))
    }

    /// The native listing pages through `nextPageToken`, carries embeddings
    /// and video models beside the chat models, and says per model whether
    /// it samples (`maxTemperature`). All three are read.
    #[tokio::test]
    async fn priming_follows_the_page_token_and_keeps_chat_models_only() {
        let page_one = br#"{"models":[
            {"name":"models/gemini-3.7-flash","displayName":"Gemini 3.7 Flash",
             "inputTokenLimit":1048576,"outputTokenLimit":65536,"maxTemperature":2,
             "supportedGenerationMethods":["generateContent","countTokens"]}
        ],"nextPageToken":"page-two"}"#;
        let page_two = br#"{"models":[
            {"name":"models/gemini-embedding-2","inputTokenLimit":8192,
             "supportedGenerationMethods":["embedContent"]},
            {"name":"models/gemini-fixed","inputTokenLimit":32768,"maxTemperature":0,
             "supportedGenerationMethods":["generateContent"]},
            {"name":"models/gemini-quiet","supportedGenerationMethods":["generateContent"]}
        ]}"#;
        let (url, _bodies) = spawn_mock_sequence(vec![
            (200, "OK", page_one.to_vec()),
            (200, "OK", page_two.to_vec()),
        ])
        .await;
        let provider = native_provider_at(&url);
        assert_eq!(provider.served_catalog(), None, "unprimed: cannot say");

        provider.prime_capabilities().await.expect("primes");

        let mut catalog = provider.served_catalog().expect("primed");
        catalog.sort();
        assert_eq!(
            catalog,
            ["gemini-3.7-flash", "gemini-fixed", "gemini-quiet"],
            "the embedding model is not something a stage can run on"
        );

        let flash = provider.capabilities("gemini-3.7-flash");
        assert!(flash.supports_temperature);
        assert_eq!(flash.max_context_tokens, 1_048_576);
        assert_eq!(flash.limits_source, LimitsSource::Api);

        let fixed = provider.capabilities("gemini-fixed");
        assert!(
            !fixed.supports_temperature,
            "a ceiling of zero is no sampling"
        );
        assert_eq!(fixed.max_context_tokens, 32_768);

        let quiet = provider.capabilities("gemini-quiet");
        assert!(
            quiet.supports_temperature,
            "no `maxTemperature` is the listing not saying, so the table's answer stands"
        );
        assert_eq!(quiet.limits_source, LimitsSource::Builtin);
    }

    /// A compat base URL has no native listing: priming learns nothing and
    /// says so with `None`, and the listing still answers from the compat
    /// endpoint.
    #[tokio::test]
    async fn a_compat_base_learns_nothing() {
        let body = br#"{"data":[{"id":"gemini-3.5-flash"}]}"#;
        let (url, _bodies) = spawn_mock_sequence(vec![(200, "OK", body.to_vec())]).await;
        let provider = GeminiProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "k".to_string(),
        )
        .with_base_url(Some(url));

        provider.prime_capabilities().await.expect("nothing to do");
        assert_eq!(provider.served_catalog(), None);

        let listed = provider.list_models().await.expect("compat listing");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "gemini-3.5-flash");
        assert!(!listed[0].learned);
    }

    /// An empty page token ends the walk the same way a missing one does.
    #[tokio::test]
    async fn an_empty_page_token_is_the_last_page() {
        let body = br#"{"models":[{"name":"models/gemini-3.5-flash"}],"nextPageToken":""}"#;
        let (url, _bodies) = spawn_mock_sequence(vec![(200, "OK", body.to_vec())]).await;
        let provider = native_provider_at(&url);
        provider.prime_capabilities().await.expect("one page");
        assert_eq!(
            provider.served_catalog(),
            Some(vec!["gemini-3.5-flash".to_string()])
        );
    }
}
