//! The provider itself: one request, and what to say when it is refused.

use std::sync::Arc;

use async_trait::async_trait;

use super::token::{Credentials, RefreshError, TokenSource};
use super::{catalog, headers, request as request_body, stream, usage};
use crate::capabilities::{ModelCapabilities, ModelCapabilityOverride};
use crate::provider::{
    InferenceRequest, InferenceResponse, ModelInfo, Provider, ProviderError, RateLimitConfig,
    Result, UnavailableReason,
};
use crate::rate_limit::RateLimiter;

/// Inference billed to a ChatGPT subscription.
pub struct CodexProvider {
    client: reqwest::Client,
    base_url: String,
    tokens: Arc<dyn TokenSource>,
    originator: String,
    user_agent: String,
    reasoning_effort: String,
    verbosity: String,
    replay_reasoning: bool,
    capability_overrides: std::collections::HashMap<String, ModelCapabilityOverride>,
    rate_limiter: Option<RateLimiter>,
    request_timeout_secs: Option<u64>,
    /// Where the subscription's quota is read from. A separate host from
    /// `base_url` in production, so it is its own field rather than a path
    /// under that one.
    usage_url: String,
    /// The catalog this account can reach, learned once at start-up.
    ///
    /// Taken through [`leviath_core::sync::lock`]; the sections clone a `Vec`
    /// and nothing else.
    served: std::sync::Mutex<Option<Vec<String>>>,
}

impl CodexProvider {
    /// Build a provider over `tokens`.
    pub fn new(client: reqwest::Client, tokens: Arc<dyn TokenSource>) -> Self {
        let originator = super::DEFAULT_ORIGINATOR.to_string();
        let user_agent = headers::user_agent_for(&originator, env!("CARGO_PKG_VERSION"));
        Self {
            client,
            base_url: super::DEFAULT_BASE_URL.to_string(),
            tokens,
            originator,
            user_agent,
            reasoning_effort: "medium".to_string(),
            verbosity: "medium".to_string(),
            replay_reasoning: true,
            capability_overrides: std::collections::HashMap::new(),
            rate_limiter: None,
            request_timeout_secs: None,
            usage_url: super::USAGE_URL.to_string(),
            served: std::sync::Mutex::new(None),
        }
    }

    /// Point at a different host. Tests use this; so does anyone proxying.
    #[must_use]
    pub fn with_base_url(mut self, base_url: Option<String>) -> Self {
        if let Some(url) = base_url {
            self.base_url = url.trim_end_matches('/').to_string();
        }
        self
    }

    /// Identify as something else. The `User-Agent` follows unless separately
    /// set, so the two cannot contradict each other by accident.
    #[must_use]
    pub fn with_originator(mut self, originator: Option<String>) -> Self {
        if let Some(originator) = originator.filter(|o| !o.trim().is_empty()) {
            self.user_agent = headers::user_agent_for(&originator, env!("CARGO_PKG_VERSION"));
            self.originator = originator;
        }
        self
    }

    /// Override the `User-Agent` alone.
    #[must_use]
    pub fn with_user_agent(mut self, user_agent: Option<String>) -> Self {
        if let Some(ua) = user_agent.filter(|u| !u.trim().is_empty()) {
            self.user_agent = ua;
        }
        self
    }

    /// Reasoning effort and text verbosity, as the operator set them.
    #[must_use]
    pub fn with_reasoning(mut self, effort: Option<String>, verbosity: Option<String>) -> Self {
        if let Some(effort) = effort.filter(|e| EFFORTS.contains(&e.as_str())) {
            self.reasoning_effort = effort;
        }
        if let Some(verbosity) = verbosity.filter(|v| VERBOSITIES.contains(&v.as_str())) {
            self.verbosity = verbosity;
        }
        self
    }

    /// Turn reasoning replay off, for the day the route stops accepting a
    /// replayed blob.
    #[must_use]
    pub fn with_reasoning_replay(mut self, replay: bool) -> Self {
        self.replay_reasoning = replay;
        self
    }

    /// Per-model corrections from `[model_capabilities]`.
    #[must_use]
    pub fn with_overrides(
        mut self,
        overrides: Option<std::collections::HashMap<String, ModelCapabilityOverride>>,
    ) -> Self {
        self.capability_overrides = overrides.unwrap_or_default();
        self
    }

    /// Apply a rate limit.
    #[must_use]
    pub fn with_rate_limit(mut self, config: Option<&RateLimitConfig>) -> Self {
        self.rate_limiter = config.map(RateLimiter::new);
        self
    }

    /// Bound every request to `secs`.
    #[must_use]
    pub fn with_request_timeout(mut self, secs: Option<u64>) -> Self {
        self.request_timeout_secs = secs;
        self
    }

    /// Point the quota read somewhere else.
    #[must_use]
    pub fn with_usage_url(mut self, url: Option<String>) -> Self {
        if let Some(url) = url.filter(|u| !u.trim().is_empty()) {
            self.usage_url = url;
        }
        self
    }

    /// The plan tier behind the grant, when it is known.
    fn plan(&self) -> Option<String> {
        let grant = self.tokens.grant()?;
        // The stored tier first, then the id token's: a refresh re-reads the
        // claims, so the two agree, and a grant written before the field
        // existed still has the token to read.
        grant.plan_type.clone().or_else(|| grant.claims().plan_type)
    }

    /// Send one request, refreshing once if the token has lapsed.
    ///
    /// The 401 is intercepted here rather than left to `check_http_response`,
    /// which maps it to a provider-fatal `AuthFailed`: on this route an expired
    /// token is routine, and treating it as fatal would trip the circuit
    /// breaker and fail the run over every few hours of uptime.
    async fn send(&self, body: &serde_json::Value, model: &str) -> Result<reqwest::Response> {
        if let Some(limiter) = &self.rate_limiter {
            // Discarded, not propagated. `acquire` waits for capacity and has
            // no failure to report; its `Result` predates the current
            // implementation. Propagating it would add an error path nothing
            // can drive. If it ever gains a real error, propagate it here.
            let _ = limiter.acquire().await;
        }
        let creds = self.credentials().await?;
        let response = self.post(body, &creds).await?;

        let response = match response.status().as_u16() {
            401 => {
                tracing::debug!("the ChatGPT token was rejected; refreshing and retrying once");
                let refreshed = self
                    .tokens
                    .refresh_stale(&creds.access_token)
                    .await
                    .map_err(unavailable)?;
                self.post(body, &refreshed).await?
            }
            _ => response,
        };

        self.classify(response, model).await
    }

    /// One POST, with no interpretation of the answer.
    async fn post(
        &self,
        body: &serde_json::Value,
        creds: &Credentials,
    ) -> Result<reqwest::Response> {
        let url = format!("{}/responses", self.base_url);
        let mut builder = crate::provider::apply_request_timeout(
            self.client.post(&url),
            self.request_timeout_secs,
        );
        for (name, value) in headers::inference(creds, &self.originator, &self.user_agent) {
            builder = builder.header(name, value);
        }
        builder
            .json(body)
            .send()
            .await
            .map_err(|e| ProviderError::transport("sending the request", &e))
    }

    /// Turn a non-success answer into the error that names what to do.
    async fn classify(
        &self,
        response: reqwest::Response,
        model: &str,
    ) -> Result<reqwest::Response> {
        let status = response.status().as_u16();
        if response.status().is_success() {
            if let Some(limiter) = &self.rate_limiter {
                limiter.reset_backoff().await;
            }
            return Ok(response);
        }

        let retry_after = crate::provider::retry_after_secs(response.headers());
        let body = response.text().await.unwrap_or_default();

        match status {
            // Almost always the client identity, and the stock remedy for a
            // Forbidden sends the reader to check model permissions instead.
            403 => Err(ProviderError::Unavailable {
                reason: UnavailableReason::Forbidden,
                detail: headers::forbidden_remedy(&self.originator, &self.user_agent, &body),
            }),
            429 => {
                let seconds = match retry_after {
                    Some(secs) => Some(secs),
                    // No Retry-After: the quota window's own reset is the real
                    // answer, and guessing with backoff against a limit that
                    // resets on a wall clock is how a run sleeps through it.
                    None => self.quota_reset_secs().await,
                };
                if let Some(limiter) = &self.rate_limiter {
                    limiter.handle_rate_limit(seconds).await;
                }
                Err(ProviderError::RateLimitExceeded {
                    retry_after_secs: seconds,
                })
            }
            400 if plan_gated(&body, model) => Err(ProviderError::Unavailable {
                reason: UnavailableReason::Forbidden,
                detail: catalog::gated_remedy(self.plan().as_deref(), model),
            }),
            400 if body.contains("Unsupported parameter") => Err(ProviderError::ApiError(format!(
                "the Codex route rejected a parameter Leviath sent. This is a bug in \
                     Leviath, not a problem with your account; please report it with this \
                     line. Response: {body}"
            ))),
            _ => match UnavailableReason::classify(status, &body) {
                Some(reason) => Err(ProviderError::Unavailable {
                    reason,
                    detail: body,
                }),
                None => Err(ProviderError::ApiError(format!("HTTP {status}: {body}"))),
            },
        }
    }

    /// Credentials, refreshing first if the token is inside its margin.
    async fn credentials(&self) -> Result<Credentials> {
        self.tokens.credentials().await.map_err(unavailable)
    }

    /// Seconds until the soonest quota window resets.
    async fn quota_reset_secs(&self) -> Option<u64> {
        let quota = self.quota().await.ok()?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        quota.resets_in(now)
    }

    /// Read the subscription's quota windows.
    pub async fn quota(&self) -> Result<usage::Quota> {
        let creds = self.credentials().await?;
        let mut builder = self.client.get(&self.usage_url);
        for (name, value) in headers::inference(&creds, &self.originator, &self.user_agent) {
            // The usage route answers JSON, not a stream.
            if name == "Accept" {
                builder = builder.header(name, "application/json");
                continue;
            }
            builder = builder.header(name, value);
        }
        let response = builder
            .send()
            .await
            .map_err(|e| ProviderError::transport("reading the subscription quota", &e))?;
        let body = response.text().await.unwrap_or_default();
        usage::parse(&body).ok_or_else(|| {
            ProviderError::InvalidResponse(
                "the quota response was not in a known shape".to_string(),
            )
        })
    }
}

/// The reasoning efforts this route accepts.
const EFFORTS: [&str; 6] = ["none", "minimal", "low", "medium", "high", "xhigh"];

/// The text verbosities it accepts.
const VERBOSITIES: [&str; 3] = ["low", "medium", "high"];

/// Whether a 400 body is the route saying this account cannot use the model.
///
/// Matched on the model name as well as the phrase, so a body that happens to
/// quote the sentence about a different model is not mistaken for this one.
fn plan_gated(body: &str, model: &str) -> bool {
    body.contains("is not supported when using Codex with a ChatGPT account")
        && body.contains(model)
}

/// Turn a refresh failure into the provider-level error that says what to do.
fn unavailable(error: RefreshError) -> ProviderError {
    ProviderError::Unavailable {
        reason: UnavailableReason::AuthFailed,
        detail: error.to_string(),
    }
}

#[async_trait]
impl Provider for CodexProvider {
    async fn infer(&self, request: &InferenceRequest) -> Result<InferenceResponse> {
        // The route streams; a buffered call is the stream collected. Sending
        // `stream: false` is not an option worth having when the shared
        // collector already exists and a silent socket gets reaped by a proxy.
        crate::provider::collect_stream(self.infer_stream(request).await?).await
    }

    async fn infer_stream(
        &self,
        request: &InferenceRequest,
    ) -> Result<
        std::pin::Pin<
            Box<dyn futures_core::Stream<Item = Result<crate::provider::StreamChunk>> + Send>,
        >,
    > {
        let body = request_body::build(
            request,
            &self.reasoning_effort,
            &self.verbosity,
            self.replay_reasoning,
        );
        let response = self.send(&body, &request.model).await?;
        Ok(Box::pin(stream::codex_sse_stream(response.bytes_stream())))
    }

    async fn count_tokens(&self, text: &str, model: &str) -> usize {
        // The same tokenizer the OpenAI provider uses: these are OpenAI models,
        // and the alternative is the byte heuristic, which would leave the
        // pre-flight window guard estimating.
        crate::tokenizer::count_tokens(text, model)
    }

    fn max_context_tokens(&self, model: &str) -> usize {
        self.capabilities(model).max_context_tokens
    }

    fn name(&self) -> &str {
        super::PROVIDER_NAME
    }

    fn capabilities(&self, model: &str) -> ModelCapabilities {
        let base = catalog::capabilities(model);
        match self.capability_overrides.get(model) {
            // Merged, not swapped: an override names only what it corrects.
            Some(over) => over.apply_to(base),
            None => base,
        }
    }

    async fn prime_capabilities(&self) -> Result<()> {
        // The plan tier is already in the stored id token, so this costs no
        // network at all in the common case. Only an unreadable token sends us
        // to the usage route, and a failure there is a warning: a provider that
        // cannot reach its own API degrades to its table rather than stopping
        // the daemon.
        let plan = match self.plan() {
            Some(plan) => Some(plan),
            None => self.quota().await.ok().and_then(|q| q.plan_type),
        };
        *leviath_core::sync::lock(&self.served) = Some(catalog::served(plan.as_deref()));
        Ok(())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let allowed = leviath_core::sync::lock(&self.served).clone();
        Ok(catalog::CATALOG
            .iter()
            .filter(|(id, _)| {
                allowed
                    .as_ref()
                    .is_none_or(|list| list.iter().any(|a| a == id))
            })
            .map(|(id, display)| {
                // `learned` stays false: this is a table compiled into the
                // build, not a listing the route offered.
                ModelInfo::new(*id, super::PROVIDER_NAME, self.capabilities(id))
                    .named(Some((*display).to_string()))
            })
            .collect())
    }

    fn serves_model(&self, model_key: &str) -> Option<String> {
        let known = catalog::CATALOG.iter().any(|(id, _)| *id == model_key)
            || self.capability_overrides.contains_key(model_key);
        // Plan gating is applied here so a `pro`-only model is not claimed on a
        // `plus` account, which would resolve a stage onto a model that 400s.
        (known && catalog::plan_allows(self.plan().as_deref(), model_key))
            .then(|| model_key.to_string())
    }

    fn served_catalog(&self) -> Option<Vec<String>> {
        // `Some` is a promise of completeness, and the lint turns a name
        // outside it into a hard error. Only answered once the plan tier is
        // known; before that "cannot say" is the honest answer.
        leviath_core::sync::lock(&self.served).clone()
    }

    fn explicit_route_only(&self) -> bool {
        // A bare `gpt-5.6-sol` in a blueprint must not silently start spending
        // a subscription because this provider happens to be configured. It is
        // reachable by an explicit `codex/` prefix, an explicit fallback entry,
        // or by being the configured default.
        true
    }

    fn pricing(&self, _model: &str) -> Option<crate::ModelPricing> {
        // A subscription is a flat monthly fee, so a call's marginal cost is a
        // known zero rather than an unknown. Saying so keeps a subscription run
        // out of the "cost unavailable" bucket it does not belong in; the
        // number that matters here is the quota window, not a dollar figure.
        Some(crate::ModelPricing::flat(0.0, 0.0))
    }
}

#[cfg(test)]
mod tests;
