//! A provider for any server that speaks the OpenAI chat API.
//!
//! llama.cpp, vLLM, LM Studio, LocalAI, and most gateways answer
//! `POST /chat/completions` and `GET /models` in OpenAI's shape, and until now
//! reaching one from Leviath meant writing a Rhai provider script for a wire
//! format this crate already implements twice. This is the third use of
//! the crate-private OpenAI compatibility module, with nothing vendor-specific on top: no compiled
//! model table, no pricing, no cache markers, just the request, the stream and
//! the listing.
//!
//! What it knows about a model it learns from the server. `GET /models` fills
//! the catalogue at priming; a server that will not list (some gateways refuse
//! the route, llama.cpp before a model is loaded answers an empty list) falls
//! back to the ids the config named, and with neither it says nothing rather
//! than guessing, so a blueprint that pins a model on it is never refused.

use crate::learned::{LearnedModel, LearnedModels};
use crate::openai_compat::{
    build_openai_request_body, openai_sse_stream, parse_openai_response, send_chat_request,
    temperature_refused,
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

/// What an unlisted model is assumed to hold, and what it is assumed to take.
///
/// The same conservative window OpenRouter assumes for a model its listing
/// does not size, for the same reason: `/models` on a compatible server says
/// which ids exist and nothing about how large they are, and a percentage
/// region budget has to resolve against some number. The number is reported
/// as [`LimitsSource::Builtin`] so `lev models` and the API say it is a guess,
/// and [`EndpointProvider::capabilities`] warns once per model naming the
/// `[model_capabilities]` entry that replaces it.
const FALLBACK_CAPABILITIES: ModelCapabilities = ModelCapabilities {
    supports_temperature: true,
    supports_streaming: true,
    supports_tools: true,
    supports_system_prompt: true,
    max_context_tokens: 128_000,
    max_output_tokens: 8_192,
    limits_source: LimitsSource::Builtin,
};

/// A provider for one OpenAI-compatible endpoint.
///
/// Registered under the name the config gave it, so several can coexist: a
/// llama.cpp on one port and a vLLM on another are two entries and two of
/// these, each with its own catalogue.
pub struct EndpointProvider {
    /// HTTP client, shared with every other provider on the same timeout.
    client: reqwest::Client,
    /// The registry name, which is also what a blueprint writes before the
    /// slash.
    name: String,
    /// Where the server is, without a trailing slash and including any path
    /// prefix (`http://localhost:8080/v1`).
    base_url: String,
    /// Sent as a bearer token when the server wants one. A local server
    /// usually does not.
    api_key: Option<String>,
    /// Extra headers on every request, as the config wrote them.
    headers: Vec<(String, String)>,
    /// Client-side rate limit, when the entry set one.
    rate_limiter: Option<RateLimiter>,
    /// `[model_capabilities]` entries, merged onto what the server said.
    capability_overrides: HashMap<String, ModelCapabilityOverride>,
    /// The ids the config named for a server that cannot list its own.
    /// `Some` is a complete catalogue; `None` is "the config did not say".
    configured_models: Option<Vec<String>>,
    /// Ids a bare model name may route here on, from the entry's `serves`.
    serves: Vec<String>,
    /// Models this server has refused a temperature for, so the next request
    /// omits it instead of paying the round trip again.
    temperature_unsupported: crate::provider::ModelMemo,
    /// Models already warned about as falling back to the assumed window.
    warned_unknown: crate::provider::ModelMemo,
    /// What `GET /models` said, once priming has read it.
    learned: LearnedModels,
    /// The entry's `request_timeout_secs`, which bounds the side calls this
    /// provider makes on its own (the model listing) as well as inference.
    request_timeout_secs: Option<u64>,
}

impl EndpointProvider {
    /// A provider for the server at `base_url`, registered as `name`.
    ///
    /// `api_key` becomes a bearer token when present; `headers` are sent on
    /// every request after it, so a header the config names wins over the one
    /// the key would have set.
    pub fn new(
        client: reqwest::Client,
        name: impl Into<String>,
        base_url: impl Into<String>,
        api_key: Option<String>,
        headers: Vec<(String, String)>,
    ) -> Self {
        let base_url = base_url.into();
        Self {
            client,
            name: name.into(),
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            headers,
            rate_limiter: None,
            capability_overrides: HashMap::new(),
            configured_models: None,
            serves: Vec::new(),
            temperature_unsupported: Default::default(),
            warned_unknown: Default::default(),
            learned: Default::default(),
            request_timeout_secs: None,
        }
    }

    /// Bound the provider's own side calls by the entry's timeout.
    ///
    /// The inference path takes its deadline from each request; the model
    /// listing has no request to take one from, so it used the side-call
    /// default even when the entry set a shorter one. Reqwest's per-request
    /// timeout wins over the client's, so a client built with the entry's
    /// timeout was not enough on its own: the probe route's 10 s waited the
    /// full 30 s.
    pub fn with_request_timeout(mut self, secs: Option<u64>) -> Self {
        self.request_timeout_secs = secs;
        self
    }

    /// The deadline on the model listing: the entry's own, or the side-call
    /// default when the entry set none.
    fn listing_timeout_secs(&self) -> Option<u64> {
        self.request_timeout_secs
            .or(Some(crate::provider::SIDE_CALL_TIMEOUT_SECS))
    }

    /// Merge `[model_capabilities]` entries onto what the server reports.
    pub fn with_overrides(mut self, overrides: HashMap<String, ModelCapabilityOverride>) -> Self {
        self.capability_overrides = overrides;
        self
    }

    /// Throttle requests client-side. `None` sends them as they come.
    pub fn with_rate_limit(
        mut self,
        rate_limit: Option<&crate::provider::RateLimitConfig>,
    ) -> Self {
        self.rate_limiter = rate_limit.map(RateLimiter::new);
        self
    }

    /// The ids to report when the server will not list its own.
    ///
    /// `Some` is taken as the complete catalogue, so a blueprint naming an id
    /// outside it is refused the way it would be against a listing. `None`
    /// leaves the provider unable to say, which refuses nothing.
    pub fn with_models(mut self, models: Option<Vec<String>>) -> Self {
        self.configured_models = models;
        self
    }

    /// Ids a bare model name (no provider prefix) may resolve here on, over
    /// and above whatever the listing or `with_models` named.
    pub fn with_serves(mut self, serves: Vec<String>) -> Self {
        self.serves = serves;
        self
    }

    /// The name this provider is registered under.
    pub fn provider_name(&self) -> &str {
        &self.name
    }

    /// Where requests go.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The headers every request carries: the bearer token when there is one,
    /// then the configured extras, then the content type.
    fn request_headers(&self) -> Vec<(&str, String)> {
        let mut headers: Vec<(&str, String)> = Vec::with_capacity(self.headers.len() + 2);
        if let Some(key) = &self.api_key {
            headers.push(("Authorization", format!("Bearer {key}")));
        }
        for (name, value) in &self.headers {
            headers.push((name.as_str(), value.clone()));
        }
        headers.push(("Content-Type", "application/json".to_string()));
        headers
    }

    /// The request body, with the temperature dropped for a model that has
    /// refused one or that the operator marked as taking none.
    fn build_body(&self, request: &InferenceRequest) -> serde_json::Value {
        let mut body = build_openai_request_body(request);
        if !self.capabilities(&request.model).supports_temperature {
            drop_temperature(&mut body);
        }
        body
    }

    /// POST `/chat/completions`, retrying once without a temperature when the
    /// server refuses the one it was sent.
    async fn post_chat(
        &self,
        request: &InferenceRequest,
        mut body: serde_json::Value,
    ) -> Result<reqwest::Response> {
        let url = format!("{}/chat/completions", self.base_url);
        let headers = self.request_headers();
        let sent = send_chat_request(
            &self.client,
            &self.name,
            &url,
            &headers,
            &body,
            self.rate_limiter.as_ref(),
            request.request_timeout_secs,
        )
        .await;
        match sent {
            Err(ProviderError::ApiError(detail)) if temperature_refused(&detail) => {
                tracing::debug!(
                    provider = %self.name,
                    model = %request.model,
                    "the endpoint refused the temperature we sent; retrying without it"
                );
                self.temperature_unsupported.insert(&request.model);
                drop_temperature(&mut body);
                send_chat_request(
                    &self.client,
                    &self.name,
                    &url,
                    &headers,
                    &body,
                    self.rate_limiter.as_ref(),
                    request.request_timeout_secs,
                )
                .await
            }
            other => other,
        }
    }

    /// GET `/models`, as the server answers it.
    async fn fetch_models_json(&self) -> Result<serde_json::Value> {
        let mut builder = crate::provider::apply_request_timeout(
            self.client.get(format!("{}/models", self.base_url)),
            self.listing_timeout_secs(),
        );
        for (name, value) in self.request_headers() {
            builder = builder.header(name, value);
        }
        let response = builder
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
                "HTTP {status}: {error_body}"
            )));
        }
        crate::provider::decode_json(response).await
    }

    /// Say so, once per model, when a model is running on the assumed window.
    ///
    /// A listed model is not warned about even though the listing did not
    /// size it: the server knows the model, and the warning is for a name
    /// nothing has confirmed.
    fn warn_if_unknown(&self, model: &str, resolved: &ModelCapabilities) {
        if resolved.limits_source != LimitsSource::Builtin
            || self.learned.contains(model)
            || !self.warned_unknown.insert(model)
        {
            return;
        }
        let provider = &self.name;
        tracing::warn!(
            provider = %provider,
            model = %model,
            assumed_context_tokens = FALLBACK_CAPABILITIES.max_context_tokens,
            "the endpoint's listing does not size this model, so a conservative window \
             is assumed; percentage region budgets resolve against it. Set the real \
             window with [model_capabilities.\"{model}\"] max_context_tokens = <n>",
        );
    }

    /// The configured fallback as listing rows, for a server that could not
    /// be asked.
    fn configured_model_infos(&self) -> Option<Vec<ModelInfo>> {
        self.configured_models.as_ref().map(|ids| {
            ids.iter()
                .map(|id| ModelInfo::new(id.clone(), self.name.clone(), self.capabilities(id)))
                .collect()
        })
    }
}

/// Take the `temperature` out of a request body.
///
/// A body that is not an object is left alone: every caller here builds one,
/// and dropping a field is not worth a panic.
fn drop_temperature(body: &mut serde_json::Value) {
    if let Some(fields) = body.as_object_mut() {
        fields.remove("temperature");
    }
}

/// The ids a `GET /models` body names, with no filtering: a compatible server
/// lists what it serves and nothing else, so every id is a chat model.
///
/// Read into [`LearnedModel`] records so the catalogue is the same shape every
/// other provider fills. The listing on these servers says nothing reliable
/// about size or parameters, so every field stays `None` and the table of
/// operator overrides remains the only source for them.
pub(crate) fn parse_listing(body: &serde_json::Value) -> Result<HashMap<String, LearnedModel>> {
    let data = body
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| ProviderError::InvalidResponse("Missing 'data' array".to_string()))?;
    Ok(data
        .iter()
        .filter_map(|item| item.get("id")?.as_str().map(str::to_string))
        .map(|id| (id, LearnedModel::default()))
        .collect())
}

#[async_trait]
impl Provider for EndpointProvider {
    async fn infer(&self, request: &InferenceRequest) -> Result<InferenceResponse> {
        tracing::debug!(provider = %self.name, model = %request.model, "calling endpoint");

        if let Some(limiter) = &self.rate_limiter {
            limiter.acquire().await?;
        }

        let body = self.build_body(request);
        let response = self.post_chat(request, body).await?;
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
        tracing::debug!(provider = %self.name, model = %request.model, "calling endpoint (streaming)");

        if let Some(limiter) = &self.rate_limiter {
            limiter.acquire().await?;
        }

        let mut body = self.build_body(request);
        crate::openai_compat::make_streaming(&mut body);
        let response = self.post_chat(request, body).await?;

        let peer = leviath_net::read_caps::peer_of(&response);
        Ok(crate::rate_limit::meter_stream(
            self.rate_limiter.as_ref(),
            Box::pin(openai_sse_stream(response.bytes_stream()).sent_by(peer)),
        ))
    }

    async fn count_tokens(&self, text: &str, _model: &str) -> usize {
        // No tokenizer is known for an arbitrary server, and most of them
        // expose no count route; the local estimate is the honest answer.
        leviath_core::estimate_tokens(text)
    }

    fn max_context_tokens(&self, model: &str) -> usize {
        self.capabilities(model).max_context_tokens
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self, model: &str) -> ModelCapabilities {
        let base = self.learned.corrected(model, FALLBACK_CAPABILITIES);
        let mut caps = match self.capability_overrides.get(model) {
            Some(o) => o.apply_to(base),
            None => {
                self.warn_if_unknown(model, &base);
                base
            }
        };
        if self.temperature_unsupported.contains(model) {
            caps.supports_temperature = false;
        }
        caps
    }

    fn serves_model(&self, model_key: &str) -> Option<String> {
        // The listing first, then the configured fallbacks. Nothing here can
        // claim a model it has not been told about: an arbitrary server has no
        // compiled table to guess from, and claiming everything would take
        // every bare model name away from the providers that do know.
        self.learned.find_by_key(model_key).or_else(|| {
            let configured = self.configured_models.iter().flatten();
            configured
                .chain(self.serves.iter())
                .any(|id| id == model_key)
                .then(|| model_key.to_string())
        })
    }

    fn served_catalog(&self) -> Option<Vec<String>> {
        self.learned
            .catalog()
            .or_else(|| self.configured_models.clone())
    }

    fn pricing(&self, model: &str) -> Option<crate::ModelPricing> {
        // Only what the operator wrote down. A local server bills nothing and
        // a gateway's rates are its own business; a guessed zero would make a
        // run's total look exact when it is not.
        self.capability_overrides
            .get(model)
            .and_then(|o| o.pricing())
    }

    async fn prime_capabilities(&self) -> Result<()> {
        let body = self.fetch_models_json().await?;
        let learned = parse_listing(&body)?;
        let count = learned.len();
        self.learned.replace(learned);
        tracing::debug!(provider = %self.name, models = count, "learned the endpoint's model ids");
        Ok(())
    }

    /// The listing, primed on first call; the configured ids when the server
    /// cannot be asked and the config named some; the error otherwise, so a
    /// caller can say the endpoint did not answer rather than that it serves
    /// nothing.
    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        if self.learned.is_empty()
            && let Err(e) = self.prime_capabilities().await
        {
            return match self.configured_model_infos() {
                Some(configured) => Ok(configured),
                None => Err(e),
            };
        }
        Ok(self
            .learned
            .to_model_infos(&self.name, |id| self.capabilities(id)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::always_on_tracing_guard;
    use leviath_testkit::{spawn_mock_sequence, spawn_mock_server};
    use std::time::Duration;

    fn client() -> reqwest::Client {
        crate::provider::build_http_client(None).expect("a test client builds")
    }

    fn provider_at(url: &str) -> EndpointProvider {
        EndpointProvider::new(client(), "local", url, None, Vec::new())
    }

    fn request(model: &str) -> InferenceRequest {
        InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "hi".into(),
                cache_breakpoint: false,
                reasoning: None,
            }],
            model: model.to_string(),
            max_tokens: 100,
            temperature: 0.2,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        }
    }

    const OK_BODY: &[u8] = br#"{"choices":[{"message":{"content":"hello"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}"#;
    const LISTING: &[u8] = br#"{"object":"list","data":[{"id":"gpt-mock","object":"model"},{"id":"local/qwen","object":"model"},{"object":"model"}]}"#;

    // ─── construction ───────────────────────────────────────────────────────

    #[test]
    fn the_base_url_loses_its_trailing_slash_and_the_name_is_kept() {
        let provider = EndpointProvider::new(
            client(),
            "llama-cpp",
            "http://localhost:8080/v1/",
            None,
            Vec::new(),
        );
        assert_eq!(provider.base_url(), "http://localhost:8080/v1");
        assert_eq!(provider.provider_name(), "llama-cpp");
        assert_eq!(provider.name(), "llama-cpp");
    }

    #[test]
    fn headers_carry_the_key_then_the_extras_then_the_content_type() {
        let provider = EndpointProvider::new(
            client(),
            "gw",
            "http://h/v1",
            Some("secret".to_string()),
            vec![("X-Org".to_string(), "research".to_string())],
        );
        let headers = provider.request_headers();
        assert_eq!(
            headers,
            vec![
                ("Authorization", "Bearer secret".to_string()),
                ("X-Org", "research".to_string()),
                ("Content-Type", "application/json".to_string()),
            ]
        );

        // No key, no Authorization header at all: a local server that gets
        // one it did not ask for may reject the request.
        let plain = provider_at("http://h/v1");
        let keyless = plain.request_headers();
        assert_eq!(keyless.len(), 1);
        assert_eq!(keyless[0].0, "Content-Type");
    }

    // ─── capabilities ───────────────────────────────────────────────────────

    #[test]
    fn an_unlisted_model_gets_the_assumed_window_and_warns_once() {
        let _guard = always_on_tracing_guard();
        let provider = provider_at("http://h/v1");
        let caps = provider.capabilities("mystery");
        assert_eq!(caps, FALLBACK_CAPABILITIES);
        assert!(caps.supports_tools);
        assert!(caps.supports_streaming);
        assert!(caps.supports_temperature);
        assert_eq!(provider.max_context_tokens("mystery"), 128_000);
        // The second ask is the same answer and no second warning.
        provider.capabilities("mystery");
        assert_eq!(provider.warned_unknown.len(), 1);
    }

    #[test]
    fn an_operator_entry_corrects_the_window_and_is_never_warned_about() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "qwen".to_string(),
            ModelCapabilityOverride {
                max_context_tokens: Some(32_768),
                input_per_mtok: Some(0.5),
                output_per_mtok: Some(1.5),
                ..Default::default()
            },
        );
        let provider = provider_at("http://h/v1").with_overrides(overrides);
        let caps = provider.capabilities("qwen");
        assert_eq!(caps.max_context_tokens, 32_768);
        assert_eq!(caps.limits_source, LimitsSource::Override);
        assert!(provider.warned_unknown.is_empty());
        // Pricing is only what was written down.
        assert_eq!(
            provider.pricing("qwen").expect("configured").input_per_mtok,
            0.5
        );
        assert_eq!(provider.pricing("other"), None);
    }

    #[test]
    fn a_refused_temperature_outranks_everything() {
        let provider = provider_at("http://h/v1");
        provider.temperature_unsupported.insert("m");
        assert!(!provider.capabilities("m").supports_temperature);
        // And the body is built without one.
        let body = provider.build_body(&request("m"));
        assert!(body.get("temperature").is_none());
        // A model with no refusal keeps it.
        let body = provider.build_body(&request("n"));
        assert_eq!(body["temperature"], 0.2);
    }

    #[test]
    fn dropping_the_temperature_leaves_a_non_object_alone() {
        let mut body = serde_json::json!({"temperature": 0.1, "model": "m"});
        drop_temperature(&mut body);
        assert_eq!(body, serde_json::json!({"model": "m"}));
        let mut text = serde_json::json!("not an object");
        drop_temperature(&mut text);
        assert_eq!(text, "not an object");
    }

    #[tokio::test]
    async fn token_counting_is_the_local_estimate() {
        let provider = provider_at("http://h/v1");
        assert_eq!(
            provider.count_tokens("twelve chars", "m").await,
            leviath_core::estimate_tokens("twelve chars")
        );
    }

    // ─── catalogue ──────────────────────────────────────────────────────────

    #[test]
    fn the_listing_is_read_without_filtering_and_skips_rows_with_no_id() {
        let body: serde_json::Value = serde_json::from_slice(LISTING).unwrap();
        let learned = parse_listing(&body).unwrap();
        let mut ids: Vec<&String> = learned.keys().collect();
        ids.sort();
        assert_eq!(ids, ["gpt-mock", "local/qwen"]);
        assert!(learned["gpt-mock"].max_context_tokens.is_none());

        let missing = parse_listing(&serde_json::json!({"object": "list"})).unwrap_err();
        assert!(missing.to_string().contains("Missing 'data'"), "{missing}");
    }

    #[tokio::test]
    async fn priming_fills_the_catalogue_and_routing_answers_from_it() {
        let _guard = always_on_tracing_guard();
        let url = spawn_mock_server(200, "OK", LISTING).await;
        let provider = EndpointProvider::new(
            client(),
            "local",
            url,
            Some("k".to_string()),
            vec![("X-Org".to_string(), "r".to_string())],
        );
        assert_eq!(provider.served_catalog(), None, "not asked yet");
        assert_eq!(provider.serves_model("gpt-mock"), None);

        provider.prime_capabilities().await.expect("primed");
        let mut catalog = provider.served_catalog().expect("listed");
        catalog.sort();
        assert_eq!(catalog, ["gpt-mock", "local/qwen"]);
        assert_eq!(
            provider.serves_model("gpt-mock").as_deref(),
            Some("gpt-mock")
        );
        // A namespaced id answers for its last segment, like a gateway's.
        assert_eq!(provider.serves_model("qwen").as_deref(), Some("local/qwen"));
        assert_eq!(provider.serves_model("nope"), None);
        // A listed model is not warned about: the endpoint knows it, even if
        // it did not size it.
        provider.capabilities("gpt-mock");
        assert!(provider.warned_unknown.is_empty());
    }

    /// The entry's timeout bounds the listing. Stamping the 30 s side-call
    /// default on the request beats any client-level timeout, so a 1 s entry
    /// against a server that never answers would wait 30 s.
    #[tokio::test]
    async fn the_listing_is_bounded_by_the_entry_timeout_not_the_side_call_default() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port is available");
        let addr = listener
            .local_addr()
            .expect("a bound listener has an address");
        // Accept every connection and hold it open without answering.
        let hold = tokio::spawn(async move {
            let mut held = Vec::new();
            loop {
                let (socket, _) = listener.accept().await.expect("accept");
                held.push(socket);
            }
        });
        let provider = provider_at(&format!("http://{addr}/v1")).with_request_timeout(Some(1));
        let started = std::time::Instant::now();
        let result = provider.list_models().await;
        let waited = started.elapsed();
        hold.abort();
        assert!(result.is_err(), "a server that never answers is an error");
        // Well under the 30 s the side-call default would have waited, with
        // room for a slow runner.
        assert!(waited < Duration::from_secs(5));
    }

    #[test]
    fn an_entry_without_a_timeout_keeps_the_side_call_default_on_the_listing() {
        let plain = provider_at("http://h/v1");
        assert_eq!(
            plain.listing_timeout_secs(),
            Some(crate::provider::SIDE_CALL_TIMEOUT_SECS)
        );
        let bounded = provider_at("http://h/v1").with_request_timeout(Some(7));
        assert_eq!(bounded.listing_timeout_secs(), Some(7));
    }

    #[tokio::test]
    async fn list_models_primes_on_first_call_and_reports_the_listing() {
        let url = spawn_mock_server(200, "OK", LISTING).await;
        let provider = provider_at(&url);
        let models = provider.list_models().await.expect("listed");
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["gpt-mock", "local/qwen"]);
        assert!(models.iter().all(|m| m.provider == "local" && m.learned));
        assert_eq!(models[0].capabilities.limits_source, LimitsSource::Builtin);
        // The second call is answered from memory: the one-shot server is gone.
        assert_eq!(provider.list_models().await.expect("cached").len(), 2);
    }

    #[tokio::test]
    async fn a_server_that_refuses_the_listing_falls_back_to_the_configured_ids() {
        let url = spawn_mock_server(404, "Not Found", b"no such route").await;
        let provider = provider_at(&url).with_models(Some(vec!["llama-3".to_string()]));

        let err = provider.prime_capabilities().await.unwrap_err();
        assert!(err.to_string().contains("HTTP 404"), "{err}");
        assert_eq!(provider.served_catalog(), Some(vec!["llama-3".to_string()]));
        assert_eq!(provider.serves_model("llama-3").as_deref(), Some("llama-3"));
        assert_eq!(provider.serves_model("other"), None);

        // Nothing was learned, so listing asks again (the one-shot server is
        // gone, so this is a transport failure) and answers with the config.
        let models = provider.list_models().await.expect("the configured ids");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "llama-3");
        assert!(!models[0].learned);
    }

    #[tokio::test]
    async fn with_neither_a_listing_nor_configured_ids_the_provider_cannot_say() {
        // Nothing listens on this port.
        let provider = provider_at("http://127.0.0.1:1");
        assert_eq!(provider.served_catalog(), None);
        assert!(provider.list_models().await.is_err());
        assert_eq!(provider.serves_model("anything"), None);
    }

    #[tokio::test]
    async fn a_listing_that_is_not_json_or_has_no_data_is_an_error() {
        let url = spawn_mock_server(200, "OK", b"not json").await;
        assert!(provider_at(&url).prime_capabilities().await.is_err());
        let url = spawn_mock_server(200, "OK", br#"{"models":[]}"#).await;
        let err = provider_at(&url).prime_capabilities().await.unwrap_err();
        assert!(err.to_string().contains("Missing 'data'"), "{err}");
    }

    #[tokio::test]
    async fn an_unreadable_error_body_still_reports_the_status() {
        let url = leviath_testkit::spawn_mock_server_truncated_body(500, "Internal").await;
        let err = provider_at(&url).prime_capabilities().await.unwrap_err();
        assert!(err.to_string().contains("unknown error"), "{err}");
    }

    #[test]
    fn serves_routes_a_bare_name_without_widening_the_catalogue() {
        let provider = provider_at("http://h/v1").with_serves(vec!["mine".to_string()]);
        assert_eq!(provider.serves_model("mine").as_deref(), Some("mine"));
        assert_eq!(provider.served_catalog(), None);
    }

    // ─── inference ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn infer_posts_the_openai_shape_and_parses_the_answer() {
        let _guard = always_on_tracing_guard();
        let (url, bodies) = spawn_mock_sequence(vec![(200, "OK", OK_BODY.to_vec())]).await;
        let provider = EndpointProvider::new(
            client(),
            "local",
            url,
            None,
            vec![("X-Org".to_string(), "r".to_string())],
        )
        .with_rate_limit(Some(&crate::provider::RateLimitConfig {
            requests_per_minute: 60,
            tokens_per_minute: 100_000,
        }));
        let response = provider
            .infer(&request("gpt-mock"))
            .await
            .expect("answered");
        assert_eq!(response.content, "hello");
        assert_eq!(response.tokens_used.total_tokens, 5);

        let sent: serde_json::Value =
            serde_json::from_str(&bodies.lock().unwrap()[0]).expect("a JSON body was sent");
        assert_eq!(sent["model"], "gpt-mock");
        assert_eq!(sent["max_tokens"], 100);
        assert_eq!(sent["temperature"], 0.2);
        assert_eq!(sent["messages"][0]["content"], "hi");
    }

    #[tokio::test]
    async fn a_refused_temperature_is_retried_without_one_and_remembered() {
        let _guard = always_on_tracing_guard();
        let refusal = br#"{"error":{"message":"Unsupported value: 'temperature' does not support 0.2 with this model.","type":"invalid_request_error","param":"temperature","code":"unsupported_value"}}"#;
        let (url, bodies) = spawn_mock_sequence(vec![
            (400, "Bad Request", refusal.to_vec()),
            (200, "OK", OK_BODY.to_vec()),
            (200, "OK", OK_BODY.to_vec()),
        ])
        .await;
        let provider = provider_at(&url);
        provider.infer(&request("strict")).await.expect("retried");
        // A second inference for the same model omits it up front.
        provider.infer(&request("strict")).await.expect("answered");

        let sent = bodies.lock().unwrap();
        assert_eq!(sent.len(), 3);
        assert!(sent[0].contains("temperature"));
        assert!(!sent[1].contains("temperature"));
        assert!(!sent[2].contains("temperature"));
        assert!(!provider.capabilities("strict").supports_temperature);
    }

    #[tokio::test]
    async fn an_ordinary_refusal_is_returned_as_it_came() {
        let url = spawn_mock_server(500, "Internal Server Error", b"boom").await;
        let err = provider_at(&url).infer(&request("m")).await.unwrap_err();
        assert!(err.to_string().contains("boom"), "{err}");
    }

    #[tokio::test]
    async fn a_body_that_is_not_json_is_an_invalid_response() {
        let url = spawn_mock_server(200, "OK", b"not json").await;
        let err = provider_at(&url).infer(&request("m")).await.unwrap_err();
        assert!(err.to_string().contains("Invalid response"), "{err}");
    }

    #[tokio::test]
    async fn streaming_frames_the_servers_events() {
        let _guard = always_on_tracing_guard();
        let sse = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
        let url = spawn_mock_server(200, "OK", sse).await;
        let provider = provider_at(&url).with_rate_limit(Some(&crate::provider::RateLimitConfig {
            requests_per_minute: 60,
            tokens_per_minute: 100_000,
        }));
        let mut stream = provider
            .infer_stream(&request("m"))
            .await
            .expect("streaming");
        use tokio_stream::StreamExt;
        let first = stream.next().await.expect("a chunk").expect("ok");
        assert_eq!(first.delta, "hi");
    }

    /// A streamed call spends the token window the way a buffered one does.
    /// The usage arrives on the stream's last frame, so it is only known once
    /// the stream has been folded; before this was recorded the window stayed
    /// empty on the daemon's default path and `tokens_per_minute` never held
    /// anything back.
    #[tokio::test]
    async fn a_streamed_call_fills_the_token_window() {
        let sse = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":50,\"total_tokens\":150}}\n\ndata: [DONE]\n\n";
        let url = spawn_mock_server(200, "OK", sse).await;
        let provider = provider_at(&url).with_rate_limit(Some(&crate::provider::RateLimitConfig {
            requests_per_minute: 60,
            tokens_per_minute: 100,
        }));
        let stream = provider
            .infer_stream(&request("m"))
            .await
            .expect("streaming");
        let response = crate::collect_stream(stream).await.expect("a whole turn");
        assert_eq!(response.tokens_used.total_tokens, 150);
        // 150 tokens against a window of 100: the next request has to wait for
        // the window to turn over, which a bounded acquire reports as elapsed.
        let limiter = provider.rate_limiter.as_ref().expect("a limiter");
        let held =
            tokio::time::timeout(std::time::Duration::from_millis(300), limiter.acquire()).await;
        assert!(held.is_err(), "the streamed tokens were not counted");
    }

    #[tokio::test]
    async fn a_streaming_refusal_is_an_error_before_any_chunk() {
        let url = spawn_mock_server(503, "Service Unavailable", b"down").await;
        assert!(provider_at(&url).infer_stream(&request("m")).await.is_err());
    }
}
