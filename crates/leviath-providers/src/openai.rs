//! OpenAI provider implementation.

use crate::capabilities::{Match, Row};
use crate::learned::{LearnedModel, LearnedModels};
use crate::openai_compat::{
    TokenLimitField, build_openai_request_body_with, openai_sse_stream, parse_openai_response,
    send_chat_request, temperature_refused, tools_refused_over_reasoning_effort,
};
use crate::provider::{
    InferenceRequest, InferenceResponse, ModelCapabilities, ModelCapabilityOverride, ModelInfo,
    Provider, ProviderError, Result, StreamChunk,
};
use crate::rate_limit::RateLimiter;
use async_trait::async_trait;
use futures_core::Stream;
use std::collections::HashMap;
use std::pin::Pin;

/// OpenAI provider.
pub struct OpenAIProvider {
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

    /// Models the API has refused tools for until told `reasoning_effort:
    /// "none"`, learned from its own error rather than declared up front.
    ///
    /// Remembered so the cost is one extra round trip per model per process,
    /// not one per inference: a run makes many calls and they would each pay it.
    /// A `HashSet` behind a lock rather than a field on the request, because the
    /// provider is shared across every agent talking to it.
    reasoning_effort_none: crate::provider::ModelMemo,
    /// Models the API has refused a temperature for, so the next request to one
    /// omits it instead of spending a round trip learning the same thing again.
    temperature_unsupported: crate::provider::ModelMemo,
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
/// do not answer `/chat/completions`: realtime and transcription models speak
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
    let reasoning = model_key.starts_with('o')
        && model_key
            .get(1..2)
            .is_some_and(|c| c.chars().all(|c| c.is_ascii_digit()));
    (model_key.starts_with("gpt") || reasoning) && !NOT_CHAT.iter().any(|s| model_key.contains(s))
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
pub(crate) const MODELS: &[Row] = &[
    Row {
        matches: &[Match::Prefix("gpt-5.5")],
        temperature: false,
        tools: true,
        context: 1_050_000,
        output: 128_000,
    },
    // GPT-5.x family (5.4, 5.4-mini, 5.4-nano, 5-mini).
    Row {
        matches: &[Match::Prefix("gpt-5")],
        temperature: true,
        tools: true,
        context: 400_000,
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
            client,
            api_key,
            base_url: "https://api.openai.com/v1".to_string(),
            rate_limiter: None,
            capability_overrides: HashMap::new(),
            reasoning_effort_none: Default::default(),
            temperature_unsupported: Default::default(),
            learned: Default::default(),
        }
    }

    /// Create a new OpenAI provider with per-model capability overrides.
    pub fn with_overrides(
        client: reqwest::Client,
        api_key: String,
        overrides: HashMap<String, ModelCapabilityOverride>,
        rate_limit: Option<&crate::provider::RateLimitConfig>,
    ) -> Self {
        Self {
            client,
            api_key,
            base_url: "https://api.openai.com/v1".to_string(),
            rate_limiter: rate_limit.map(crate::rate_limit::RateLimiter::new),
            capability_overrides: overrides,
            reasoning_effort_none: Default::default(),
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

    /// Return built-in capability defaults for a model.
    fn builtin_capabilities(&self, model: &str) -> ModelCapabilities {
        table_capabilities(model)
    }

    /// POST a chat-completions body, teaching the retry described on
    /// [`tools_refused_over_reasoning_effort`].
    ///
    /// Shared by both entry points so streaming and non-streaming cannot learn
    /// different things about the same model.
    async fn post_chat(
        &self,
        request: &InferenceRequest,
        mut body: serde_json::Value,
    ) -> Result<reqwest::Response> {
        let url = format!("{}/chat/completions", self.base_url);
        let headers = [
            ("Authorization", format!("Bearer {}", self.api_key)),
            ("Content-Type", "application/json".to_string()),
        ];

        // A model that takes no temperature is sent none, rather than being sent
        // zero. `build_openai_request_body_with` always writes the key and the
        // runtime substitutes `0.0` where a model declares no support, but "not
        // supported" is not a value: the o-series accepts only its default and
        // rejects `0.0` exactly as firmly as `0.7`, so the one flag that exists
        // to protect these models was what broke them. Omitting is what the
        // OpenRouter provider has always done for the same models.
        if !self.capabilities(&request.model).supports_temperature {
            body = drop_temperature(body);
        }

        // Already learned for this model: pay nothing and send it up front.
        if self.needs_reasoning_effort_none(&request.model) {
            set_reasoning_effort_none(&mut body);
        }
        if self.temperature_is_unsupported(&request.model) {
            body = drop_temperature(body);
        }
        // A caller who set `reasoning_effort` themselves (via the manifest's
        // `[model.parameters]`) has said what they want. Overriding it, or
        // retrying to override it, would quietly ignore them - so the retry is
        // only for a body that never mentioned the field.
        let ours_to_set = body.get("reasoning_effort").is_none();

        let sent = send_chat_request(
            &self.client,
            "openai",
            &url,
            &headers,
            &body,
            self.rate_limiter.as_ref(),
            request.request_timeout_secs,
        )
        .await;

        match sent {
            Err(ProviderError::ApiError(detail))
                if ours_to_set && tools_refused_over_reasoning_effort(&detail) =>
            {
                tracing::debug!(
                    model = %request.model,
                    "OpenAI refused tools alongside a reasoning effort; retrying with none"
                );
                self.remember_reasoning_effort_none(&request.model);
                set_reasoning_effort_none(&mut body);
                send_chat_request(
                    &self.client,
                    "openai",
                    &url,
                    &headers,
                    &body,
                    self.rate_limiter.as_ref(),
                    request.request_timeout_secs,
                )
                .await
            }
            Err(ProviderError::ApiError(detail)) if temperature_refused(&detail) => {
                tracing::debug!(
                    model = %request.model,
                    "the API refused the temperature we sent; retrying without it"
                );
                self.remember_temperature_unsupported(&request.model);
                body = drop_temperature(body);
                send_chat_request(
                    &self.client,
                    "openai",
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

    /// Whether this model has already refused a temperature.
    fn temperature_is_unsupported(&self, model: &str) -> bool {
        self.temperature_unsupported.contains(model)
    }

    /// Record that it did, for the rest of this process.
    fn remember_temperature_unsupported(&self, model: &str) {
        self.temperature_unsupported.insert(model);
    }

    /// Whether this model has already refused tools over a reasoning effort.
    fn needs_reasoning_effort_none(&self, model: &str) -> bool {
        self.reasoning_effort_none.contains(model)
    }

    /// Record that it did, for the rest of this process.
    fn remember_reasoning_effort_none(&self, model: &str) {
        self.reasoning_effort_none.insert(model);
    }
}

/// Take `temperature` out of a request body.
///
/// "Not supported" is not a value: a model that takes only its default rejects
/// `0.0` exactly as firmly as `0.7`, so the field has to be absent rather than
/// zeroed. One function because three callers want it - the capability says
/// so, the API said so once already, or the API is saying so right now - and a
/// body that is not an object is not a case any of them can produce.
fn drop_temperature(body: serde_json::Value) -> serde_json::Value {
    match body {
        serde_json::Value::Object(mut fields) => {
            fields.remove("temperature");
            serde_json::Value::Object(fields)
        }
        // Not a shape any caller here produces, and returned untouched rather
        // than panicked over: dropping a field is not worth a crash.
        other => other,
    }
}

/// Say "no reasoning" in the field the API rejected the request over.
fn set_reasoning_effort_none(body: &mut serde_json::Value) {
    body["reasoning_effort"] = serde_json::Value::String("none".to_string());
}

#[async_trait]
impl Provider for OpenAIProvider {
    async fn infer(&self, request: &InferenceRequest) -> Result<InferenceResponse> {
        tracing::debug!(model = %request.model, "Calling OpenAI API");

        if let Some(limiter) = &self.rate_limiter {
            limiter.acquire().await?;
        }

        let body = build_openai_request_body_with(request, TokenLimitField::MaxCompletionTokens);
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
        tracing::debug!(model = %request.model, "Calling OpenAI API (streaming)");

        if let Some(limiter) = &self.rate_limiter {
            limiter.acquire().await?;
        }

        let mut body =
            build_openai_request_body_with(request, TokenLimitField::MaxCompletionTokens);
        crate::openai_compat::make_streaming(&mut body);
        let response = self.post_chat(request, body).await?;

        let peer = leviath_net::read_caps::peer_of(&response);
        let byte_stream = response.bytes_stream();
        let stream = openai_sse_stream(byte_stream).sent_by(peer);

        Ok(crate::rate_limit::meter_stream(
            self.rate_limiter.as_ref(),
            Box::pin(stream),
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
        "openai"
    }

    fn serves_model(&self, model_key: &str) -> Option<String> {
        // OpenAI's chat models are `gpt-*`, and its reasoning line is `o1`/`o3`
        // and successors. See the note on the Gemini provider for why the
        // capability table is the wrong thing to ask.
        (is_chat_model_id(model_key) || self.capability_overrides.contains_key(model_key))
            .then(|| model_key.to_string())
    }

    fn pricing(&self, model: &str) -> Option<crate::ModelPricing> {
        // Config first: it is the only source that can know a negotiated rate,
        // and the shipped table is a transcription of a public page that may
        // have moved since this build.
        self.capability_overrides
            .get(model)
            .and_then(|o| o.pricing())
            .or_else(|| crate::pricing::published_rates("openai", model))
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
        if self.temperature_unsupported.contains(model) {
            caps.supports_temperature = false;
        }
        caps
    }

    /// The chat and reasoning models the listing named, once primed.
    ///
    /// Filtered through `is_chat_model_id` because `GET /v1/models` also
    /// carries embeddings, transcription, speech and image models (130 entries
    /// against a few dozen chat models, measured), and a complete catalogue
    /// that named `text-embedding-3-large` would let a blueprint route a stage
    /// to it. The same rule routes a bare name, so nothing routing accepts is
    /// refused here.
    fn served_catalog(&self) -> Option<Vec<String>> {
        self.learned
            .catalog()
            .map(|ids| ids.into_iter().filter(|id| is_chat_model_id(id)).collect())
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
        tracing::debug!(models = count, "learned OpenAI model ids and dates");
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
            .to_model_infos("openai", |id| self.capabilities(id))
            .into_iter()
            .filter(|m| is_chat_model_id(&m.id))
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
            "gpt-5.5".to_string(),
            crate::ModelCapabilityOverride {
                input_per_mtok: Some(1.0),
                output_per_mtok: Some(2.0),
                ..Default::default()
            },
        );
        let provider = OpenAIProvider::with_overrides(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "k".to_string(),
            overrides,
            None,
        );

        // Configured: the operator's number, not the table's.
        let configured = provider.pricing("gpt-5.5").expect("configured");
        assert_eq!(configured.input_per_mtok, 1.0);
        assert_eq!(configured.output_per_mtok, 2.0);

        // Not configured: the published rate.
        let listed = provider.pricing("gpt-5.4").expect("in the table");
        assert_eq!(listed.input_per_mtok, 2.5);

        // Neither: unpriced, so the run reports its cost unavailable.
        assert_eq!(provider.pricing("no-such-model-9"), None);
    }
    use super::*;
    use crate::provider::LimitsSource;
    use crate::test_support::always_on_tracing_guard;
    use leviath_testkit::{spawn_mock_server, spawn_mock_server_truncated_body};

    #[test]
    fn test_provider_creation() {
        let provider = OpenAIProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        assert_eq!(provider.name(), "openai");
    }

    #[test]
    fn test_context_limits() {
        let provider = OpenAIProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        assert_eq!(provider.max_context_tokens("gpt-5.4-mini"), 400_000);
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
            model: "gpt-5.4-mini".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = build_openai_request_body_with(&request, TokenLimitField::MaxCompletionTokens);
        assert_eq!(body["model"], "gpt-5.4-mini");
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

    /// Both arms of the removal, including the shape no caller produces.
    #[test]
    fn dropping_a_temperature_leaves_everything_else_alone() {
        let body = serde_json::json!({"model": "m", "temperature": 0.7, "max_tokens": 8});
        assert_eq!(
            super::drop_temperature(body),
            serde_json::json!({"model": "m", "max_tokens": 8})
        );
        // A body that is not an object comes back untouched.
        assert_eq!(
            super::drop_temperature(serde_json::json!(7)),
            serde_json::json!(7)
        );
    }

    /// The provider recovers from the refusal instead of failing the run, and
    /// the second request is the one that differs.
    ///
    /// The run this comes from died at `analyze` after 37 iterations and 2.4M
    /// tokens because the capability table said `gpt-5.5` takes a temperature
    /// and it does not. The table is corrected too, but a table is the wrong
    /// thing to depend on - the next model to behave this way will be wrong in
    /// it on the day it ships - so the recovery is driven by what the API said.
    #[tokio::test]
    async fn a_refused_temperature_is_retried_without_one() {
        let refusal = br#"{"error":{"message":"Unsupported value: 'temperature' does not support 0.7 with this model. Only the default (1) value is supported.","type":"invalid_request_error","param":"temperature","code":"unsupported_value"}}"#;
        let ok = br#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2}}"#;
        let (url, bodies) = leviath_testkit::spawn_mock_sequence(vec![
            (400, "Bad Request", refusal.to_vec()),
            (200, "OK", ok.to_vec()),
        ])
        .await;

        let provider = OpenAIProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        )
        .with_base_url(Some(url));

        let request = InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "hi".into(),
                cache_breakpoint: false,
            }],
            // A model the table believes takes a temperature, so one is sent
            // and the refusal is what removes it.
            model: "gpt-5.4".to_string(),
            max_tokens: 16,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let out = provider.infer(&request).await;
        assert!(out.is_ok(), "the retry has to rescue the call: {out:?}");
        assert_eq!(out.expect("checked ok above").content, "ok");

        let sent = leviath_core::sync::lock(&bodies).clone();
        assert_eq!(sent.len(), 2, "one refusal, one retry: {sent:?}");
        // As a pair, so the whole claim is in one message and neither
        // argument is an expression that only runs when the test fails.
        let carried: Vec<bool> = sent.iter().map(|b| b.contains("temperature")).collect();
        assert_eq!(
            carried,
            vec![true, false],
            "the first request carries the temperature and the retry drops it: {sent:?}"
        );

        // Learned, so the next call to this model omits it up front rather
        // than spending the refused round trip again. Without this the fix
        // costs an extra request on every single inference.
        assert!(provider.temperature_is_unsupported("gpt-5.4"));
        let (url2, bodies2) =
            leviath_testkit::spawn_mock_sequence(vec![(200, "OK", ok.to_vec())]).await;
        let provider = provider.with_base_url(Some(url2));
        provider
            .infer(&request)
            .await
            .expect("the second call succeeds first time");
        let again = leviath_core::sync::lock(&bodies2).clone();
        assert_eq!(again.len(), 1, "no retry needed: {again:?}");
        let carried_again: Vec<bool> = again.iter().map(|b| b.contains("temperature")).collect();
        assert_eq!(
            carried_again,
            vec![false],
            "and it was omitted up front: {again:?}"
        );
    }

    #[test]
    fn test_builtin_capabilities_gpt55() {
        let provider = OpenAIProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let caps = provider.builtin_capabilities("gpt-5.5");
        // Verified against the API, which answers "Unsupported value:
        // 'temperature' does not support 0.7 with this model. Only the default
        // (1) value is supported." The rest of the gpt-5 family does take one,
        // which is how the generic branch came to cover this model wrongly and
        // killed a research run mid-`analyze`.
        assert!(!caps.supports_temperature);
        assert!(caps.supports_streaming);
        assert_eq!(caps.max_context_tokens, 1_050_000);
        assert_eq!(caps.max_output_tokens, 128_000);
    }

    #[test]
    fn test_builtin_capabilities_gpt54_mini() {
        let provider = OpenAIProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let caps = provider.builtin_capabilities("gpt-5.4-mini");
        assert!(caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 400_000);
        assert_eq!(caps.max_output_tokens, 128_000);
    }

    #[test]
    fn test_builtin_capabilities_gpt54_nano() {
        let provider = OpenAIProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let caps = provider.builtin_capabilities("gpt-5.4-nano");
        assert!(caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 400_000);
        assert_eq!(caps.max_output_tokens, 128_000);
    }

    #[test]
    fn test_builtin_capabilities_gpt41() {
        let provider = OpenAIProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let caps = provider.builtin_capabilities("gpt-4.1");
        assert!(caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 1_047_576);
        assert_eq!(caps.max_output_tokens, 32_768);
    }

    #[test]
    fn test_builtin_capabilities_o4_mini() {
        let provider = OpenAIProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let caps = provider.builtin_capabilities("o4-mini");
        assert!(!caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 200_000);
        assert_eq!(caps.max_output_tokens, 100_000);
    }

    #[test]
    fn test_builtin_capabilities_o3() {
        let provider = OpenAIProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let caps = provider.builtin_capabilities("o3-mini");
        assert!(!caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 200_000);
        assert_eq!(caps.max_output_tokens, 100_000);
    }

    #[test]
    fn test_capabilities_override() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "gpt-5.4-mini".to_string(),
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
        let provider = OpenAIProvider::with_overrides(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
            overrides,
            None,
        );
        let caps = provider.capabilities("gpt-5.4-mini");
        assert!(!caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 1);
    }

    #[test]
    fn test_parse_response_with_cached_tokens() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "Hello!"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 20,
                "total_tokens": 120,
                "prompt_tokens_details": {
                    "cached_tokens": 80
                }
            }
        });

        let response = parse_openai_response(&body).unwrap();
        // The OpenAI shape reports `prompt_tokens` INCLUSIVE of its details, so
        // 80 of those 100 were cache reads and only 20 were fresh. Reporting
        // 100 here (as this once did) bills the 80 twice: once at the full
        // input rate inside `prompt_tokens`, and again at the cache rate.
        assert_eq!(response.tokens_used.prompt_tokens, 20, "fresh input only");
        assert_eq!(response.tokens_used.cached_tokens, 80);
        assert_eq!(response.tokens_used.cache_write_tokens, 0);
        // The three input counts are disjoint, so they add back up.
        assert_eq!(response.tokens_used.input_tokens(), 100);
        assert_eq!(response.tokens_used.total_tokens, 120);
    }

    #[test]
    fn test_parse_response_without_cached_tokens() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "Hello!"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 20,
                "total_tokens": 120
            }
        });

        let response = parse_openai_response(&body).unwrap();
        assert_eq!(response.tokens_used.cached_tokens, 0);
    }

    // ── Additional coverage tests ──────────────────────────────────────────

    #[test]
    fn test_name() {
        let provider = OpenAIProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        assert_eq!(provider.name(), "openai");
    }

    #[tokio::test]
    async fn test_count_tokens_uses_tiktoken() {
        let provider = OpenAIProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let tokens = provider.count_tokens("Hello, world!", "gpt-5.4-mini").await;
        assert!(tokens > 0);
        assert!(tokens < 20);
    }

    #[tokio::test]
    async fn test_count_tokens_empty() {
        let provider = OpenAIProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let tokens = provider.count_tokens("", "gpt-5.4-mini").await;
        assert_eq!(tokens, 0);
    }

    /// A prompt above the inline threshold is counted on a blocking thread and
    /// comes back with the same answer the inline path gives: the hop changes
    /// where the CPU is spent, never the count.
    #[tokio::test]
    async fn a_large_prompt_is_counted_off_the_async_threads() {
        let provider = OpenAIProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let text = "word ".repeat(TIKTOKEN_INLINE_BYTES / 5 + 1_000);
        assert!(text.len() > TIKTOKEN_INLINE_BYTES);
        let tokens = provider.count_tokens(&text, "gpt-5.4-mini").await;
        assert_eq!(
            tokens,
            crate::tokenizer::count_tokens(&text, "gpt-5.4-mini")
        );
    }

    #[test]
    fn test_max_context_tokens_delegates_to_capabilities() {
        let provider = OpenAIProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        assert_eq!(provider.max_context_tokens("gpt-5.5"), 1_050_000);
        assert_eq!(provider.max_context_tokens("gpt-4.1"), 1_047_576);
        assert_eq!(provider.max_context_tokens("o3-mini"), 200_000);
    }

    #[test]
    fn test_builtin_capabilities_unknown_model() {
        let provider = OpenAIProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let caps = provider.builtin_capabilities("totally-unknown");
        let default = ModelCapabilities::default();
        assert_eq!(caps.max_context_tokens, default.max_context_tokens);
    }

    #[test]
    fn test_capabilities_uses_override() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "gpt-5.5".to_string(),
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
        let provider = OpenAIProvider::with_overrides(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
            overrides,
            None,
        );
        let caps = provider.capabilities("gpt-5.5");
        assert_eq!(caps.max_context_tokens, 1);
    }

    #[test]
    fn test_parse_response_no_content() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 5,
                "completion_tokens": 0,
                "total_tokens": 5
            }
        });
        let response = parse_openai_response(&body).unwrap();
        assert_eq!(response.content, "");
    }

    #[test]
    fn test_parse_response_finish_reason_length() {
        let body = serde_json::json!({
            "choices": [{
                "message": { "content": "truncated" },
                "finish_reason": "length"
            }],
            "usage": { "prompt_tokens": 5, "completion_tokens": 5, "total_tokens": 10 }
        });
        let response = parse_openai_response(&body).unwrap();
        assert_eq!(response.finish_reason, FinishReason::TokenLimit);
    }

    #[test]
    fn test_gpt5_family_context() {
        let provider = OpenAIProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        // gpt-5.4, gpt-5-mini, etc. should all match gpt-5 pattern
        assert_eq!(provider.max_context_tokens("gpt-5.4"), 400_000);
        assert_eq!(provider.max_context_tokens("gpt-5-mini"), 400_000);
    }

    #[test]
    fn test_o4_mini_capabilities() {
        let provider = OpenAIProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let caps = provider.builtin_capabilities("o4-mini");
        assert!(!caps.supports_temperature);
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 200_000);
        assert_eq!(caps.max_output_tokens, 100_000);
    }

    // ─── HTTP-call-level tests via a raw-TCP mock server ───────────────────

    fn provider_with_url(url: String) -> OpenAIProvider {
        OpenAIProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        )
        .with_base_url(Some(url))
    }

    #[test]
    fn with_base_url_keeps_the_default_when_none_is_given() {
        let provider = OpenAIProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        )
        .with_base_url(None);
        assert_eq!(provider.base_url, "https://api.openai.com/v1");
    }

    #[test]
    fn with_base_url_replaces_the_default() {
        let provider = OpenAIProvider::new(
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
            model: "gpt-5.4".to_string(),
            max_tokens: 100,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        }
    }

    // ─── A model that takes no temperature ──────────────────────────────────

    #[tokio::test]
    async fn a_model_taking_no_temperature_is_sent_none() {
        // o3 declares `supports_temperature: false`, and the runtime turns that
        // into `0.0` - a value it rejects as firmly as any other, since it takes
        // only its own default. Omitting is the only thing that works.
        let (url, bodies) =
            leviath_testkit::spawn_mock_sequence(vec![(200, "OK", OK_BODY.to_vec())]).await;
        let provider = provider_with_url(url);
        let request = InferenceRequest {
            model: "o3".to_string(),
            ..simple_request()
        };
        provider.infer(&request).await.unwrap();

        let sent = bodies.lock().expect("recorder").clone();
        let body = &sent[0];
        assert!(!body.contains("temperature"), "{body}");
    }

    #[tokio::test]
    async fn a_model_taking_a_temperature_still_gets_one() {
        // The other half, so "omit it" cannot quietly become "omit it always".
        let (url, bodies) =
            leviath_testkit::spawn_mock_sequence(vec![(200, "OK", OK_BODY.to_vec())]).await;
        let provider = provider_with_url(url);
        let request = InferenceRequest {
            model: "gpt-4o".to_string(),
            temperature: 0.5,
            ..simple_request()
        };
        provider.infer(&request).await.unwrap();

        let sent = bodies.lock().expect("recorder").clone();
        let body = &sent[0];
        assert!(body.contains(r#""temperature":0.5"#), "{body}");
    }

    #[tokio::test]
    async fn streaming_omits_it_too() {
        let sse = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
        let (url, bodies) =
            leviath_testkit::spawn_mock_sequence(vec![(200, "OK", sse.to_vec())]).await;
        let provider = provider_with_url(url);
        let request = InferenceRequest {
            model: "o4-mini".to_string(),
            ..simple_request()
        };
        assert!(provider.infer_stream(&request).await.is_ok());

        let sent = bodies.lock().expect("recorder").clone();
        let body = &sent[0];
        assert!(!body.contains("temperature"), "{body}");
    }

    // ─── Tools refused over a reasoning effort ──────────────────────────────

    /// Verbatim from `api.openai.com`, captured while reproducing #333.
    const TOOLS_REFUSED: &[u8] = br#"{"error":{"message":"Function tools with reasoning_effort are not supported for gpt-5.6-terra in /v1/chat/completions. To use function tools, use /v1/responses or set reasoning_effort to 'none'.","type":"invalid_request_error","param":"reasoning_effort","code":null}}"#;

    const OK_BODY: &[u8] = br#"{"choices":[{"message":{"content":"hi there"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2}}"#;

    fn request_with_a_tool() -> InferenceRequest {
        InferenceRequest {
            tools: vec![crate::provider::Tool {
                name: "get_time".to_string(),
                description: "Get the time".to_string(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            }],
            ..simple_request()
        }
    }

    #[tokio::test]
    async fn a_refusal_over_reasoning_effort_is_retried_with_none() {
        let _guard = always_on_tracing_guard();
        let (url, bodies) = leviath_testkit::spawn_mock_sequence(vec![
            (400, "Bad Request", TOOLS_REFUSED.to_vec()),
            (200, "OK", OK_BODY.to_vec()),
        ])
        .await;
        let provider = provider_with_url(url);
        let resp = provider.infer(&request_with_a_tool()).await.unwrap();
        assert_eq!(resp.content, "hi there");

        // The assertion that matters. "It eventually succeeded" would also pass
        // against a retry that resent the identical body.
        let sent = bodies.lock().expect("recorder").clone();
        assert_eq!(sent.len(), 2, "expected exactly one retry: {sent:?}");
        // Bound rather than indexed inside the message: a message expression
        // only runs when the assert fails, so `sent[0]` there would be a region
        // no passing run ever reaches.
        let (first, retry) = (&sent[0], &sent[1]);
        assert!(
            !first.contains("reasoning_effort"),
            "the first attempt should not mention the field: {first}"
        );
        assert!(
            retry.contains(r#""reasoning_effort":"none""#),
            "the retry should carry it: {retry}"
        );
    }

    #[tokio::test]
    async fn the_second_call_for_a_learned_model_asks_once() {
        // The point of remembering: a run makes many inferences and only the
        // first should pay for the discovery.
        let (url, bodies) = leviath_testkit::spawn_mock_sequence(vec![
            (400, "Bad Request", TOOLS_REFUSED.to_vec()),
            (200, "OK", OK_BODY.to_vec()),
            (200, "OK", OK_BODY.to_vec()),
        ])
        .await;
        let provider = provider_with_url(url);
        provider.infer(&request_with_a_tool()).await.unwrap();
        provider.infer(&request_with_a_tool()).await.unwrap();

        let sent = bodies.lock().expect("recorder").clone();
        assert_eq!(
            sent.len(),
            3,
            "the second inference should not retry: {sent:?}"
        );
        let third = &sent[2];
        assert!(
            third.contains(r#""reasoning_effort":"none""#),
            "the learned setting should be sent up front: {third}"
        );
    }

    #[tokio::test]
    async fn an_unrelated_bad_request_is_not_retried() {
        // A model that takes a reasoning effort but not the value `none` says so
        // in a message that never mentions tools. Retrying it with `none` would
        // resend the same rejection.
        let other = br#"{"error":{"message":"Unsupported value: 'reasoning_effort' does not support 'none' with this model. Supported values are: 'low', 'medium', 'high', and 'xhigh'.","type":"invalid_request_error","param":"reasoning_effort"}}"#;
        let (url, bodies) = leviath_testkit::spawn_mock_sequence(vec![
            (400, "Bad Request", other.to_vec()),
            (200, "OK", OK_BODY.to_vec()),
        ])
        .await;
        let provider = provider_with_url(url);
        let err = provider.infer(&request_with_a_tool()).await.unwrap_err();
        assert!(err.to_string().contains("API error:"), "{err}");
        assert_eq!(
            bodies.lock().expect("recorder").len(),
            1,
            "should not retry"
        );
    }

    #[tokio::test]
    async fn a_caller_supplied_reasoning_effort_is_left_alone() {
        // `[model.parameters] reasoning_effort = "low"` is the caller saying
        // what they want. Overriding it would ignore them silently.
        let (url, bodies) = leviath_testkit::spawn_mock_sequence(vec![
            (400, "Bad Request", TOOLS_REFUSED.to_vec()),
            (200, "OK", OK_BODY.to_vec()),
        ])
        .await;
        let provider = provider_with_url(url);
        let request = InferenceRequest {
            extra: serde_json::json!({ "reasoning_effort": "low" }),
            ..request_with_a_tool()
        };
        let err = provider.infer(&request).await.unwrap_err();
        assert!(err.to_string().contains("API error:"), "{err}");
        let sent = bodies.lock().expect("recorder").clone();
        assert_eq!(sent.len(), 1, "should not retry over the caller's setting");
        let first = &sent[0];
        assert!(first.contains(r#""reasoning_effort":"low""#), "{first}");
    }

    #[tokio::test]
    async fn streaming_learns_the_same_thing() {
        let _guard = always_on_tracing_guard();
        let sse = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
        let (url, bodies) = leviath_testkit::spawn_mock_sequence(vec![
            (400, "Bad Request", TOOLS_REFUSED.to_vec()),
            (200, "OK", sse.to_vec()),
        ])
        .await;
        let provider = provider_with_url(url);
        assert!(provider.infer_stream(&request_with_a_tool()).await.is_ok());
        let sent = bodies.lock().expect("recorder").clone();
        assert_eq!(sent.len(), 2);
        let retry = &sent[1];
        assert!(retry.contains(r#""reasoning_effort":"none""#), "{retry}");
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
        let body = br#"{"data":[{"id":"gpt-5.4"}]}"#;
        let url = spawn_mock_server(200, "OK", body).await;
        let provider = provider_with_url(url);
        let models = provider.list_models().await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gpt-5.4");
        assert_eq!(models[0].provider, "openai");
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
        let body = br#"{"data":[{"no_id": true}, {"id":"gpt-valid"}]}"#;
        let url = spawn_mock_server(200, "OK", body).await;
        let provider = provider_with_url(url);
        let models = provider.list_models().await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gpt-valid");
    }

    #[tokio::test]
    async fn list_models_skips_entries_with_non_string_id() {
        // covers the `.as_str()?` None branch in the filter_map
        let body = br#"{"data":[{"id": 42}, {"id":"gpt-valid"}]}"#;
        let url = spawn_mock_server(200, "OK", body).await;
        let provider = provider_with_url(url);
        let models = provider.list_models().await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gpt-valid");
    }

    // ─── transport-failure arms (connection refused, no server listening) ──

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

    // ─── "unknown error" fallback when the error body can't be read ────────

    #[tokio::test]
    async fn list_models_non_success_body_read_error_falls_back_to_unknown_error() {
        let url = spawn_mock_server_truncated_body(500, "Internal Server Error").await;
        let provider = provider_with_url(url);
        let err = provider.list_models().await.unwrap_err();
        assert!(err.to_string().contains("unknown error"));
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
        let limited = OpenAIProvider::with_overrides(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "k".to_string(),
            HashMap::new(),
            Some(&cfg),
        );
        assert!(limited.rate_limiter.is_some());
        let unlimited = OpenAIProvider::with_overrides(
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
        let provider = OpenAIProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "k".to_string(),
        );
        assert_eq!(
            provider.serves_model("gpt-5.5"),
            Some("gpt-5.5".to_string()),
            "its own model"
        );
        assert!(
            provider.serves_model("claude-opus-5").is_none(),
            "claude-opus-5 belongs to another vendor"
        );
        assert!(
            provider.serves_model("gemini-3.1-pro-preview").is_none(),
            "gemini-3.1-pro-preview belongs to another vendor"
        );
        assert!(
            provider.serves_model("grok-4.6").is_none(),
            "grok-4.6 belongs to another vendor"
        );
        // The reasoning line is named differently from the chat line.
        assert_eq!(provider.serves_model("o3"), Some("o3".to_string()));
        assert_eq!(
            provider.serves_model("o1-preview"),
            Some("o1-preview".to_string())
        );
        // A name that merely opens with the same letter is not one of them.
        assert!(provider.serves_model("opus-5").is_none(), "not OpenAI's");
        assert!(
            provider.serves_model("not-a-real-model-xyz").is_none(),
            "a model nobody has"
        );
    }
}

#[cfg(test)]
mod learned_tests {
    use super::*;
    use leviath_testkit::{spawn_mock_sequence, spawn_mock_server};

    fn provider_at(url: String) -> OpenAIProvider {
        OpenAIProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        )
        .with_base_url(Some(url))
    }

    /// `GET /v1/models` names every model the account can reach, embeddings
    /// and transcription included, and says nothing about size or shape. So
    /// priming learns ids and dates, publishes only the chat-shaped ids, and
    /// leaves every capability exactly where the table had it.
    #[tokio::test]
    async fn priming_learns_ids_and_dates_but_not_shape() {
        let body = br#"{"object":"list","data":[
            {"id":"gpt-5.5","object":"model","created":1776824847,"owned_by":"system","shutdown_date":null},
            {"id":"text-embedding-3-large","object":"model","created":1,"owned_by":"system"},
            {"id":"whisper-1","object":"model","created":2,"owned_by":"openai-internal"},
            {"id":"gpt-realtime-2.1","object":"model","created":4,"owned_by":"system"},
            {"id":"gpt-transcribe","object":"model","created":5,"owned_by":"system"},
            {"id":"gpt-4o-mini-tts","object":"model","created":6,"owned_by":"system"},
            {"id":"o3","object":"model","created":3,"owned_by":"system","shutdown_date":"2027-01-01"}
        ]}"#;
        // One response only: `list_models` after priming must not fetch again.
        let (url, _bodies) = spawn_mock_sequence(vec![(200, "OK", body.to_vec())]).await;
        let provider = provider_at(url);
        assert_eq!(provider.served_catalog(), None, "unprimed: cannot say");
        let before = provider.capabilities("gpt-5.5");

        provider.prime_capabilities().await.expect("primes");

        let mut catalog = provider.served_catalog().expect("primed");
        catalog.sort();
        assert_eq!(
            catalog,
            ["gpt-5.5", "o3"],
            "chat and reasoning ids only: no embeddings, speech, realtime or transcription"
        );
        assert_eq!(provider.capabilities("gpt-5.5"), before);

        let listed = provider.list_models().await.expect("from the store");
        let ids: Vec<&str> = listed.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["gpt-5.5", "o3"]);
        assert_eq!(listed[0].released, Some(1_776_824_847));
        assert_eq!(listed[0].retires, None);
        assert_eq!(listed[1].retires.as_deref(), Some("2027-01-01"));
        assert!(listed[1].learned);
        assert_eq!(listed[1].display_name, None, "the listing carries none");
    }

    /// The API's refusal is the last word, above even an operator's entry:
    /// the runtime reads this flag to decide whether to resolve a temperature
    /// at all, and resolving one for a model that has already said no spends
    /// a round trip per call learning the same thing.
    #[test]
    fn a_refused_temperature_outranks_the_table_and_the_override() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "gpt-5.5".to_string(),
            ModelCapabilityOverride {
                supports_temperature: Some(true),
                ..Default::default()
            },
        );
        let provider = OpenAIProvider::with_overrides(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "k".to_string(),
            overrides,
            None,
        );
        assert!(provider.capabilities("gpt-5.5").supports_temperature);
        provider.temperature_unsupported.insert("gpt-5.5");
        assert!(!provider.capabilities("gpt-5.5").supports_temperature);
    }

    #[tokio::test]
    async fn a_listing_without_data_is_an_error() {
        let url = spawn_mock_server(200, "OK", br#"{"object":"list"}"#).await;
        let provider = provider_at(url);
        let err = provider.prime_capabilities().await.unwrap_err();
        assert!(err.to_string().contains("data"), "{err}");
    }
}
