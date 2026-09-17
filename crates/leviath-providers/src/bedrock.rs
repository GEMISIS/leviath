//! AWS Bedrock, reached with a Bedrock API key.
//!
//! Bedrock fronts models from a dozen vendors behind one API, Converse, and
//! that is the API this provider speaks: it is the only one that covers the
//! whole catalogue (the OpenAI-shaped route on Bedrock does not serve Claude,
//! Nova or Llama, and the Anthropic-shaped one serves Claude alone). The
//! request and reply shapes live in `convert`, the binary stream in
//! `eventstream` and `stream`, exact counting in `count`, and what this build
//! knows about the models in `catalog` and `pricing`.
//!
//! The key is a Bedrock API key sent as a bearer token, which AWS accepts on
//! the runtime host, the control-plane host that lists models, and the
//! `bedrock-mantle` host that counts tokens for the newest Claude. No AWS
//! credentials, no request signing. Every host is regional, so the region is
//! part of the address and the one setting a user has to get right.

pub(crate) mod catalog;
mod convert;
mod count;
mod eventstream;
pub(crate) mod media;
mod pricing;
mod stream;

use std::collections::HashMap;
use std::pin::Pin;

use async_trait::async_trait;
use futures_core::Stream;

use crate::failure::FailureKind;
use crate::learned::LearnedModels;
use crate::provider::{
    InferenceRequest, InferenceResponse, ModelCapabilities, ModelCapabilityOverride, ModelInfo,
    Provider, ProviderError, Result, StreamChunk, UnavailableReason,
};
use crate::rate_limit::RateLimiter;

/// The name this provider is registered and configured under.
pub const PROVIDER_NAME: &str = "bedrock";

/// The region used when none is configured.
pub const DEFAULT_REGION: &str = "us-east-1";

/// The environment variable AWS's own tooling reads the API key from.
pub const KEY_ENV: &str = "AWS_BEARER_TOKEN_BEDROCK";

/// Re-exported so a listing compiled from the tables can name what AWS's
/// cards say about a model.
pub use catalog::{WindowRow, window_for, windows_read_on};

/// The Bedrock provider.
pub struct BedrockProvider {
    /// HTTP client for inference.
    client: reqwest::Client,
    /// The Bedrock API key, sent as a bearer token.
    api_key: String,
    /// The AWS region every host is derived from.
    region: String,
    /// The runtime origin, when a gateway replaces AWS's.
    runtime_url: Option<String>,
    /// The control-plane origin, when a test or gateway replaces AWS's.
    control_url: Option<String>,
    /// The `bedrock-mantle` origin, when a test or gateway replaces AWS's.
    mantle_url: Option<String>,
    /// The price file's URL, when a test replaces AWS's.
    pricing_url: Option<String>,
    /// Client-side rate limiter.
    rate_limiter: Option<RateLimiter>,
    /// Per-model capability overrides from `[model_capabilities]`.
    capability_overrides: HashMap<String, ModelCapabilityOverride>,
    /// What the listing and the price file said, filled by
    /// [`Provider::prime_capabilities`].
    learned: LearnedModels,
    /// Models Bedrock's own CountTokens refused, so the Anthropic route is
    /// tried first for them next time. See `count`.
    count_route: crate::provider::ModelMemo,
    /// The account's data retention mode (`none`, `default`, `aws_review`,
    /// `provider_data_share` or `inherit`), once read. `None` until
    /// [`Self::account_retention`] has answered, or when a gateway fronts
    /// the control plane and it cannot be asked.
    retention_mode: std::sync::RwLock<Option<String>>,
    /// What the `bedrock-mantle` listing says each model allows, keyed by
    /// the bare id that listing spells (`anthropic.claude-sonnet-5`, never a
    /// profile). Empty until [`Self::read_model_retention`] has answered.
    /// A model absent here is one the listing does not carry, and the
    /// account's mode alone decides for it.
    model_retention: std::sync::RwLock<HashMap<String, ModelRetention>>,
    /// The operator's extra headers, sent after the provider's own on every
    /// inference call to the runtime origin (the one a gateway replaces),
    /// and not on the AWS control-plane, price-file or count routes.
    extra_headers: Vec<(String, String)>,
}

/// What Bedrock's model listing says about one model's data retention: the
/// modes it can be served under, and the mode it is served under now.
///
/// A model that does not list `none` cannot run with zero data retention on
/// Bedrock at all, whatever the account is set to; under account mode `none`
/// Bedrock reports it unavailable. Measured on 2026-09-15: every OpenAI model
/// on Bedrock and Claude Fable 5 are such models, Claude Sonnet 5 and Opus 5
/// are not.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelRetention {
    /// The modes the model can be served under.
    #[serde(default)]
    pub allowed_modes: Vec<String>,
    /// The mode it is served under with the account as it is now: the
    /// account's own mode, or the model's default when the account inherits.
    #[serde(default)]
    pub mode: String,
    /// `available` or `unavailable`, as the listing says of the model under
    /// the account as it is now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Why it is unavailable, in Bedrock's words: the mode it is not served
    /// under, or an access grant the account lacks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<String>,
}

impl ModelRetention {
    /// Whether the model can be served under `mode`.
    pub fn allows(&self, mode: &str) -> bool {
        self.allowed_modes.iter().any(|m| m == mode)
    }

    /// Whether the listing says the model cannot be called as things stand.
    pub fn unavailable(&self) -> bool {
        self.status.as_deref() == Some("unavailable")
    }
}

/// The account's data retention setting as Bedrock reports it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AccountRetention {
    /// `none`, `default`, `aws_review`, `provider_data_share` (legacy) or
    /// `inherit`.
    pub mode: String,
    /// When it was last set, as Bedrock spells it (a timestamp string or
    /// epoch seconds, depending on the plane that answered).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<serde_json::Value>,
}

/// The retention modes Bedrock accepts on `PUT /data-retention`, least
/// permissive first. `inherit` defers to a broader scope.
pub const RETENTION_MODES: &[&str] = &[
    "none",
    "default",
    "aws_review",
    "provider_data_share",
    "inherit",
];

impl BedrockProvider {
    /// A provider for `region` with the key, and nothing else configured.
    pub fn new(client: reqwest::Client, api_key: String) -> Self {
        Self {
            client,
            api_key,
            region: DEFAULT_REGION.to_string(),
            runtime_url: None,
            control_url: None,
            mantle_url: None,
            pricing_url: None,
            rate_limiter: None,
            capability_overrides: HashMap::new(),
            learned: LearnedModels::default(),
            count_route: crate::provider::ModelMemo::default(),
            retention_mode: std::sync::RwLock::new(None),
            model_retention: std::sync::RwLock::new(HashMap::new()),
            extra_headers: Vec::new(),
        }
    }

    /// `GET /v1/models` on the `bedrock-mantle` host: which data retention
    /// modes each model allows. Remembered, so [`Provider::live_retention`]
    /// can refuse a model that cannot run under mode `none` before a request
    /// is sent. Answers how many models were read; `Ok(0)` behind a gateway,
    /// where the host is not reached.
    ///
    /// The listing is tried on the region's host and then on `us-east-1`,
    /// the way a count is: a host that does not carry the listing answers
    /// with an error rather than an empty page.
    pub async fn read_model_retention(&self) -> Result<usize> {
        let Some(hosts) = self.mantle_hosts() else {
            return Ok(0);
        };
        let headers = [("x-api-key", self.api_key.clone())];
        let mut result = Err(ProviderError::Other("no mantle host".to_string()));
        for host in &hosts {
            result = self
                .get_json_with(&format!("{host}/v1/models"), &headers)
                .await;
            if result.is_ok() {
                break;
            }
        }
        let body = result?;
        let rows = body.get("data").and_then(|d| d.as_array()).ok_or_else(|| {
            ProviderError::InvalidResponse("the mantle listing carries no data".to_string())
        })?;
        let read: HashMap<String, ModelRetention> = rows
            .iter()
            .filter_map(|row| {
                let id = row.get("id")?.as_str()?.to_string();
                let mut retention: ModelRetention =
                    serde_json::from_value(row.get("data_retention")?.clone()).ok()?;
                let text = |key: &str| row.get(key)?.as_str().map(str::to_string);
                retention.status = text("status");
                retention.status_reason = text("status_reason");
                Some((id, retention))
            })
            .collect();
        let count = read.len();
        *self
            .model_retention
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = read;
        Ok(count)
    }

    /// What the listing said about `model`'s retention, by its bare id, once
    /// [`Self::read_model_retention`] has answered. `None` for a model the
    /// listing does not carry.
    pub fn model_retention(&self, model: &str) -> Option<ModelRetention> {
        self.model_retention
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(catalog::bare_id(model))
            .cloned()
    }

    /// Everything the listing said, by bare id, sorted: what
    /// `lev providers retention` prints per model.
    pub fn model_retentions(&self) -> Vec<(String, ModelRetention)> {
        let mut rows: Vec<(String, ModelRetention)> = self
            .model_retention
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(id, r)| (id.clone(), r.clone()))
            .collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        rows
    }

    /// `GET /data-retention` on the control plane: the account's data
    /// retention mode. Remembered, so [`Provider::live_retention`] can answer
    /// without a network call. `None` when a gateway fronts the control
    /// plane, which cannot be asked.
    pub async fn account_retention(&self) -> Result<Option<AccountRetention>> {
        let Some(control) = self.control_base() else {
            return Ok(None);
        };
        let url = format!("{control}/data-retention");
        let body = self.get_json(&url, true).await?;
        let read: AccountRetention = serde_json::from_value(body).map_err(|e| {
            ProviderError::ApiError(format!("Bedrock's data retention answer is not one: {e}"))
        })?;
        self.remember_retention(&read.mode);
        Ok(Some(read))
    }

    /// `PUT /data-retention` on the control plane: set the account's data
    /// retention mode. `none` is zero data retention; `aws_review` is what
    /// Claude Fable 5 and Mythos 5 need. Refused here for a word Bedrock does
    /// not take, before any request is made.
    pub async fn set_account_retention(&self, mode: &str) -> Result<AccountRetention> {
        if !RETENTION_MODES.contains(&mode) {
            return Err(ProviderError::ApiError(format!(
                "'{mode}' is not a Bedrock data retention mode: {}",
                RETENTION_MODES.join(", ")
            )));
        }
        let Some(control) = self.control_base() else {
            return Err(ProviderError::ApiError(
                "a gateway fronts Bedrock's control plane, so the account's data retention \
                 mode cannot be set from here"
                    .to_string(),
            ));
        };
        let url = format!("{control}/data-retention");
        let builder = crate::provider::apply_request_timeout(
            crate::provider::side_call_client().put(&url),
            Some(crate::provider::SIDE_CALL_TIMEOUT_SECS),
        )
        .header("authorization", format!("Bearer {}", self.api_key))
        .json(&serde_json::json!({ "mode": mode }));
        let response = builder
            .send()
            .await
            .map_err(|e| ProviderError::transport("setting Bedrock's data retention", &e))?;
        let response = classify_response(response, None).await?;
        let body = crate::provider::decode_json(response).await?;
        let written: AccountRetention = serde_json::from_value(body).map_err(|e| {
            ProviderError::ApiError(format!("Bedrock's data retention answer is not one: {e}"))
        })?;
        self.remember_retention(&written.mode);
        Ok(written)
    }

    /// The mode read or written most recently, if any.
    pub fn retention_mode(&self) -> Option<String> {
        self.retention_mode
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn remember_retention(&self, mode: &str) {
        *self
            .retention_mode
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(mode.to_string());
    }

    /// The constructor the registry calls: with the operator's per-model
    /// corrections and rate limit.
    pub fn with_overrides(
        client: reqwest::Client,
        api_key: String,
        overrides: HashMap<String, ModelCapabilityOverride>,
        rate_limit: Option<&crate::provider::RateLimitConfig>,
    ) -> Self {
        Self {
            rate_limiter: rate_limit.map(RateLimiter::new),
            capability_overrides: overrides,
            ..Self::new(client, api_key)
        }
    }

    /// The region every host is derived from. `None` keeps the default.
    pub fn with_region(mut self, region: Option<String>) -> Self {
        if let Some(region) = region
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty())
        {
            self.region = region;
        }
        self
    }

    /// Replace the runtime origin (`https://bedrock-runtime.<region>.amazonaws.com`)
    /// with a gateway's. `None` keeps the default.
    ///
    /// With a gateway in front, the listing, the price file and the count
    /// routes are not read unless they are given their own origin: a
    /// gateway that fronts inference rarely fronts the control plane, and
    /// asking it would be answered with its own 404 page.
    pub fn with_base_url(mut self, base_url: Option<String>) -> Self {
        if let Some(url) = base_url {
            self.runtime_url = Some(url.trim_end_matches('/').to_string());
        }
        self
    }

    /// Replace the control-plane origin (`https://bedrock.<region>.amazonaws.com`).
    pub fn with_control_url(mut self, url: Option<String>) -> Self {
        if let Some(url) = url {
            self.control_url = Some(url.trim_end_matches('/').to_string());
        }
        self
    }

    /// Replace the `bedrock-mantle` origin the newest Claude is counted on.
    pub fn with_mantle_url(mut self, url: Option<String>) -> Self {
        if let Some(url) = url {
            self.mantle_url = Some(url.trim_end_matches('/').to_string());
        }
        self
    }

    /// Extra headers on every inference call to the runtime origin, after
    /// the provider's own: what a gateway named in `with_base_url` wants of
    /// its own. The AWS-only routes (control plane, price file, counts) are
    /// not sent them; a gateway does not front those.
    pub fn with_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.extra_headers = headers;
        self
    }

    /// Replace the URL the price file is read from.
    pub fn with_pricing_url(mut self, url: Option<String>) -> Self {
        if let Some(url) = url {
            self.pricing_url = Some(url);
        }
        self
    }

    /// The configured region.
    pub fn region(&self) -> &str {
        &self.region
    }

    /// The origin inference goes to.
    pub fn runtime_base(&self) -> String {
        self.runtime_url
            .clone()
            .unwrap_or_else(|| format!("https://bedrock-runtime.{}.amazonaws.com", self.region))
    }

    /// The origin the listing is read from, or `None` when a gateway fronts
    /// inference and nothing was said about the control plane.
    fn control_base(&self) -> Option<String> {
        self.control_url.clone().or_else(|| {
            self.runtime_url
                .is_none()
                .then(|| format!("https://bedrock.{}.amazonaws.com", self.region))
        })
    }

    /// Where Anthropic's count route is served: the configured host alone,
    /// or the region's `bedrock-mantle` host and then `us-east-1`, which
    /// carries every model the route serves. `None` behind a gateway, for
    /// the same reason as the listing.
    fn mantle_hosts(&self) -> Option<Vec<String>> {
        if let Some(url) = &self.mantle_url {
            return Some(vec![url.clone()]);
        }
        if self.runtime_url.is_some() {
            return None;
        }
        let mut hosts = vec![count::mantle_host(&self.region)];
        let fallback = count::mantle_host(count::FALLBACK_MANTLE_REGION);
        if hosts[0] != fallback {
            hosts.push(fallback);
        }
        Some(hosts)
    }

    /// Where the price file is, or `None` for the same reason.
    fn price_url(&self) -> Option<String> {
        self.pricing_url.clone().or_else(|| {
            self.runtime_url
                .is_none()
                .then(|| pricing::price_file_url(&self.region))
        })
    }

    /// The Converse URL for `model`, streaming or not.
    fn converse_url(&self, model: &str, streaming: bool) -> String {
        format!(
            "{}/model/{}/converse{}",
            self.runtime_base(),
            encode_model_id(model),
            match streaming {
                true => "-stream",
                false => "",
            }
        )
    }

    /// The headers every request to an AWS host carries.
    fn header_pairs(&self) -> Vec<(&'static str, String)> {
        vec![
            ("authorization", format!("Bearer {}", self.api_key)),
            ("content-type", "application/json".to_string()),
            ("accept", "application/json".to_string()),
        ]
    }

    /// An inference `POST`, checked.
    async fn post(
        &self,
        url: &str,
        body: &serde_json::Value,
        timeout: Option<u64>,
    ) -> Result<reqwest::Response> {
        let mut builder = crate::provider::apply_request_timeout(self.client.post(url), timeout);
        for (name, value) in self.header_pairs() {
            builder = builder.header(name, value);
        }
        builder = crate::provider::with_extra_headers(builder, &self.extra_headers);
        let response = builder
            .json(body)
            .send()
            .await
            .map_err(|e| ProviderError::transport("sending the request", &e))?;
        let response = classify_response(response, self.rate_limiter.as_ref()).await?;
        if let Some(limiter) = &self.rate_limiter {
            limiter.reset_backoff().await;
        }
        Ok(response)
    }

    /// A `POST` about inference rather than inference itself, on the pooled
    /// side-call client with its short deadline.
    async fn post_side_call(
        &self,
        url: &str,
        headers: &[(&str, String)],
        body: &serde_json::Value,
    ) -> Result<reqwest::Response> {
        let mut builder = crate::provider::apply_request_timeout(
            crate::provider::side_call_client().post(url),
            Some(crate::provider::SIDE_CALL_TIMEOUT_SECS),
        );
        for (name, value) in headers {
            builder = builder.header(*name, value);
        }
        let response = builder
            .json(body)
            .send()
            .await
            .map_err(|e| ProviderError::transport("sending the request", &e))?;
        classify_response(response, self.rate_limiter.as_ref()).await
    }

    /// A `GET` of one JSON document from an AWS host, with the key.
    async fn get_json(&self, url: &str, authed: bool) -> Result<serde_json::Value> {
        let bearer = [("authorization", format!("Bearer {}", self.api_key))];
        let headers: &[(&str, String)] = match authed {
            true => &bearer,
            false => &[],
        };
        self.get_json_with(url, headers).await
    }

    /// A side `GET` with the headers given, the mantle listing's `x-api-key`
    /// included, answered as JSON.
    async fn get_json_with(
        &self,
        url: &str,
        headers: &[(&str, String)],
    ) -> Result<serde_json::Value> {
        let mut builder = crate::provider::apply_request_timeout(
            crate::provider::side_call_client().get(url),
            Some(crate::provider::SIDE_CALL_TIMEOUT_SECS),
        );
        for (name, value) in headers {
            builder = builder.header(*name, value);
        }
        let response = builder
            .send()
            .await
            .map_err(|e| ProviderError::transport("listing models", &e))?;
        let response = classify_response(response, None).await?;
        crate::provider::decode_json_capped(response, pricing::PRICE_FILE_CAP).await
    }

    /// Every text model `ListFoundationModels` names.
    async fn fetch_foundation_models(
        &self,
        control: &str,
    ) -> Result<Vec<catalog::FoundationModel>> {
        let body = self
            .get_json(&format!("{control}/foundation-models"), true)
            .await?;
        let summaries = body
            .get("modelSummaries")
            .and_then(|s| s.as_array())
            .ok_or_else(|| {
                ProviderError::InvalidResponse(
                    "the model listing carries no modelSummaries".to_string(),
                )
            })?;
        Ok(summaries
            .iter()
            .filter_map(catalog::parse_foundation_model)
            .collect())
    }

    /// Every system-defined inference profile, all pages of them.
    async fn fetch_inference_profiles(
        &self,
        control: &str,
    ) -> Result<Vec<catalog::InferenceProfile>> {
        let mut profiles = Vec::new();
        let mut next_token: Option<String> = None;
        loop {
            let mut url =
                format!("{control}/inference-profiles?typeEquals=SYSTEM_DEFINED&maxResults=1000");
            if let Some(token) = &next_token {
                url.push_str("&nextToken=");
                url.push_str(token);
            }
            let body = self.get_json(&url, true).await?;
            profiles.extend(
                body.get("inferenceProfileSummaries")
                    .and_then(|s| s.as_array())
                    .into_iter()
                    .flatten()
                    .filter_map(catalog::parse_inference_profile),
            );
            match body.get("nextToken").and_then(|t| t.as_str()) {
                Some(token) if !token.is_empty() => next_token = Some(token.to_string()),
                _ => return Ok(profiles),
            }
        }
    }

    /// The rate cards in the region's price file, by display name.
    async fn fetch_prices(&self) -> Result<HashMap<String, crate::pricing::ModelPricing>> {
        let url = self.price_url().ok_or_else(|| {
            ProviderError::Other("a gateway is configured; the price file is not read".to_string())
        })?;
        let file = self.get_json(&url, false).await?;
        Ok(pricing::parse_price_file(&file))
    }

    /// The listing this build compiles in, for when the live one cannot be
    /// read.
    fn table_listing(&self) -> Vec<ModelInfo> {
        catalog::CATALOG
            .iter()
            .map(|(id, name)| {
                ModelInfo::new(*id, PROVIDER_NAME, self.capabilities(id))
                    .named(Some((*name).to_string()))
                    .with_mime(self.mime(id))
            })
            .collect()
    }
}

/// `model` as a path segment: every byte outside RFC 3986's unreserved set
/// percent-encoded, which turns the `:` in `-v1:0` into `%3A` and the `/` in
/// an ARN into `%2F`.
pub(crate) fn encode_model_id(model: &str) -> String {
    let mut out = String::with_capacity(model.len());
    for byte in model.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// The `x-amzn-ErrorType` header, without the URI AWS sometimes appends.
fn aws_error_type(headers: &reqwest::header::HeaderMap) -> Option<String> {
    headers
        .get("x-amzn-errortype")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(':').next().unwrap_or(v).trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The message in an AWS error body (`{"message"}`) or an Anthropic one
/// (`{"error": {"message"}}`).
fn error_message(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    value
        .get("message")
        .or_else(|| value.get("Message"))
        .or_else(|| value.pointer("/error/message"))
        .and_then(|m| m.as_str())
        .map(str::to_string)
}

/// Whether an AWS 403 means the key itself was rejected, as opposed to a
/// key that works but may not do this.
///
/// The type header says so for a mistyped or expired key. A freshly minted
/// or deactivated Bedrock API key is refused differently, measured live: an
/// `AccessDeniedException` whose message is "Authentication failed: Please
/// make sure your API Key is valid", so the message is read too.
fn is_bad_key(error_type: Option<&str>, message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    matches!(
        error_type,
        Some("UnrecognizedClientException" | "ExpiredTokenException" | "InvalidSignatureException")
    ) || lower.contains("authentication failed")
        || lower.contains("security token")
        || lower.contains("api key is valid")
}

/// Check an AWS response for errors and return it on success.
///
/// The shared `check_http_response` cannot be used: AWS reports a revoked
/// or mistyped key as a 403, which the shared classifier reads as "the key
/// works but may not do this" and tells the operator to check the model's
/// permissions. The `x-amzn-ErrorType` header tells the two apart, and it is
/// gone once the body has been read, so this reads it first.
pub(crate) async fn classify_response(
    response: reqwest::Response,
    limiter: Option<&RateLimiter>,
) -> Result<reqwest::Response> {
    let status = response.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        let retry_after = crate::provider::retry_after_secs(response.headers());
        if let Some(l) = limiter {
            l.handle_rate_limit(retry_after).await;
        }
        return Err(ProviderError::RateLimitExceeded {
            retry_after_secs: retry_after,
        });
    }
    if status.is_success() {
        return Ok(response);
    }
    let error_type = aws_error_type(response.headers());
    let body =
        leviath_net::read_caps::read_text_capped(response, leviath_net::read_caps::JSON_BODY_CAP)
            .await
            .unwrap_or_else(|e| e.to_string());
    let message = error_message(&body).unwrap_or_else(|| body.clone());
    let named = match &error_type {
        Some(kind) => format!("{kind}: {message}"),
        None => message,
    };
    let kind = FailureKind::from_status(status.as_u16());
    let detail = format!(
        "[{}] HTTP {}: {} - {}",
        kind.label(),
        status,
        named,
        kind.remedy()
    );
    let reason = match status.as_u16() {
        401 => Some(UnavailableReason::AuthFailed),
        403 if is_bad_key(error_type.as_deref(), &body) => Some(UnavailableReason::AuthFailed),
        403 => Some(UnavailableReason::Forbidden),
        code => UnavailableReason::classify(code, &body),
    };
    Err(match reason {
        Some(reason) => ProviderError::Unavailable { reason, detail },
        None => ProviderError::ApiError(detail),
    })
}

#[async_trait]
impl Provider for BedrockProvider {
    async fn infer(&self, request: &InferenceRequest) -> Result<InferenceResponse> {
        tracing::debug!(model = %request.model, "Calling Bedrock Converse");
        if let Some(limiter) = &self.rate_limiter {
            limiter.acquire().await?;
        }
        if media::is_image_model(&request.model) {
            return self.run_image(request).await;
        }
        let caps = self.capabilities(&request.model);
        let body = convert::converse_body(request, &caps, catalog::vendor_of(&request.model));
        let response = self
            .post(
                &self.converse_url(&request.model, false),
                &body,
                request.request_timeout_secs,
            )
            .await?;
        let value: serde_json::Value = crate::provider::decode_json(response).await?;
        let result = convert::parse_response(&value)?;
        if let Some(limiter) = &self.rate_limiter {
            limiter.record_tokens(result.tokens_used.total_tokens);
        }
        Ok(result)
    }

    async fn infer_stream(
        &self,
        request: &InferenceRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>> {
        tracing::debug!(model = %request.model, "Calling Bedrock ConverseStream");
        if let Some(limiter) = &self.rate_limiter {
            limiter.acquire().await?;
        }
        if media::is_image_model(&request.model) {
            return Ok(crate::media::one_chunk(self.run_image(request).await?));
        }
        let caps = self.capabilities(&request.model);
        let body = convert::converse_body(request, &caps, catalog::vendor_of(&request.model));
        let response = self
            .post(
                &self.converse_url(&request.model, true),
                &body,
                request.request_timeout_secs,
            )
            .await?;
        let peer = leviath_net::read_caps::peer_of(&response);
        let stream = stream::ConverseStream::new(response.bytes_stream()).sent_by(peer);
        Ok(crate::rate_limit::meter_stream(
            self.rate_limiter.as_ref(),
            Box::pin(stream),
        ))
    }

    /// The exact count from Bedrock, or the local heuristic when neither of
    /// its routes counts this model. Through the limiter like `infer`, as
    /// the count spends the same request quota.
    async fn count_tokens(&self, text: &str, model: &str) -> usize {
        if let Some(limiter) = &self.rate_limiter {
            limiter
                .acquire()
                .await
                .expect("the rate limiter only waits for capacity; it does not fail");
        }
        match self.count_remote(text, model).await {
            Ok(n) => n,
            Err(e) => {
                tracing::debug!(error = %e, "Bedrock could not count the tokens; using heuristic");
                crate::tokenizer::count_tokens(text, catalog::vendor_model(model))
            }
        }
    }

    fn max_context_tokens(&self, model: &str) -> usize {
        self.capabilities(model).max_context_tokens
    }

    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    /// The account's mode, once read, decides, and the listing's word on the
    /// model narrows it. A model the listing never offers under `none`
    /// cannot run with zero retention here at all. Otherwise a model that
    /// allows mode `none` keeps nothing under any mode; Claude Fable 5 and
    /// Mythos 5 keep 30 days under `aws_review` and are unavailable below
    /// it; `default` leaves each model to its own policy, under which AWS may
    /// keep flagged content for abuse detection. An account that inherits
    /// its mode serves each model under that model's default.
    fn live_retention(&self, model: &str) -> Option<crate::retention::RetentionPolicy> {
        use crate::retention::{Control, Retention, RetentionPolicy, Source};
        let read = self.retention_mode()?;
        let covered = crate::retention::is_covered_claude(model);
        let listed = self.model_retention(model);
        // The mode the model is served under: the account's own, or the
        // model's default when the account defers.
        let mode = match (read.as_str(), &listed) {
            ("inherit", Some(m)) if !m.mode.is_empty() => m.mode.clone(),
            ("inherit", _) => "default".to_string(),
            (own, _) => own.to_string(),
        };
        let kept_regardless = match covered {
            true => Retention::Days(30),
            false => Retention::Unknown,
        };
        let (retention, note) = match (&listed, mode.as_str(), covered) {
            (Some(m), _, _) if !m.allows("none") => (
                kept_regardless,
                format!(
                    "Bedrock serves this model under data retention mode {} only, never \
                     none, so it cannot run with zero data retention here{}",
                    m.allowed_modes.join(" or "),
                    match m.allows(&mode) {
                        true => String::new(),
                        false => format!("; under the account's mode {mode} it is unavailable"),
                    }
                ),
            ),
            (Some(m), _, _) if !m.allows(&mode) => (
                kept_regardless,
                format!(
                    "the account's data retention mode is {mode}, which Bedrock does not \
                     serve this model under (it allows {}), so the model is unavailable \
                     until the mode changes",
                    m.allowed_modes.join(" or ")
                ),
            ),
            (_, _, true) => (
                Retention::Days(30),
                format!(
                    "the account's data retention mode is {mode}; this model needs \
                     aws_review and keeps prompts and outputs up to 30 days inside AWS \
                     for the human review Anthropic requires"
                ),
            ),
            (_, "none", false) => (
                Retention::Zero,
                "the account's data retention mode is none: nothing is written to \
                 durable storage or shared with the model provider"
                    .to_string(),
            ),
            (_, "default", false) => (
                Retention::Unknown,
                match read.as_str() {
                    "inherit" => {
                        "the account inherits its data retention mode, so this model runs \
                         under its own default, and AWS may keep content flagged for \
                         abuse detection; set the mode to none for a guarantee"
                    }
                    _ => {
                        "the account's data retention mode is default: the model's own \
                         policy applies, and AWS may keep content flagged for abuse \
                         detection; set the mode to none for a guarantee"
                    }
                }
                .to_string(),
            ),
            (_, _, false) => (
                Retention::Zero,
                format!(
                    "the account's data retention mode is {mode}; a model that allows \
                     mode none keeps nothing whatever the account mode"
                ),
            ),
        };
        Some(RetentionPolicy {
            retention,
            control: Control::Account,
            source: Source::Live,
            note,
        })
    }

    /// The account's mode again, and the listing if it was never read: what
    /// `lev providers retention set zero` changed on the account reaches a
    /// daemon that primed before it.
    async fn refresh_retention(&self) {
        if let Err(e) = self.account_retention().await {
            tracing::debug!(error = %e, "Bedrock's data retention mode could not be re-read");
        }
        let unread = self
            .model_retention
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty();
        if unread && let Err(e) = self.read_model_retention().await {
            tracing::debug!(error = %e, "Bedrock's per-model data retention could not be read");
        }
    }

    fn capabilities(&self, model: &str) -> ModelCapabilities {
        // Three answers, narrowest first: what the user wrote, what the
        // listing and the card said, what this build was compiled with.
        let mut base = self
            .learned
            .corrected(model, catalog::table_capabilities(model));
        // An image model is sent its prompt alone and answers with images.
        if media::is_image_model(model) {
            base.supports_tools = false;
            base.supports_temperature = false;
            base.max_output_tokens = base.max_output_tokens.min(4_096);
            base.max_context_tokens = base.max_context_tokens.max(32_000);
        }
        match self.capability_overrides.get(model) {
            Some(o) => o.apply_to(base),
            None => base,
        }
    }

    fn mime(&self, model: &str) -> crate::capabilities::ModelMime {
        let base = self.learned.mime_corrected(model, catalog::mime_for(model));
        let mime = match self.capability_overrides.get(model) {
            Some(o) => o.apply_mime(base),
            None => base,
        };
        match media::is_image_model(model) {
            true => mime,
            false => crate::mime::WireShape::Bedrock.carried(mime),
        }
    }

    /// Read the listing, the profiles and the price file into `Self::learned`.
    ///
    /// The listing carries names, modalities and which ids can be called;
    /// the price file carries rates for every vendor but Anthropic. Neither
    /// carries a token limit, so those stay with the card table. A price
    /// file that cannot be read leaves the rates unset rather than failing
    /// the prime: cost is reported as unknown, which is the honest figure.
    async fn prime_capabilities(&self) -> Result<()> {
        let Some(control) = self.control_base() else {
            tracing::debug!("a gateway fronts Bedrock; the listing is not read");
            return Ok(());
        };
        let models = self.fetch_foundation_models(&control).await?;
        let profiles = self.fetch_inference_profiles(&control).await?;
        let mut learned = catalog::merge_listing(&models, &profiles);
        match self.fetch_prices().await {
            Ok(prices) => pricing::attach_prices(&mut learned, &prices),
            Err(e) => tracing::debug!(error = %e, "Bedrock's price file could not be read"),
        }
        let count = learned.len();
        self.learned.replace(learned);
        tracing::debug!(models = count, "learned Bedrock model capabilities");
        // Best effort, like the price file: an account whose key cannot read
        // the setting (an SCP, an older key) still runs, with the table's
        // answer standing in until `lev providers retention` asks again.
        match self.account_retention().await {
            Ok(Some(read)) => {
                tracing::debug!(mode = %read.mode, "read Bedrock's data retention mode")
            }
            Ok(None) => {}
            Err(e) => {
                tracing::debug!(error = %e, "Bedrock's data retention mode could not be read")
            }
        }
        // The same again for what each model allows: a listing that cannot
        // be read leaves the account's mode to decide alone.
        match self.read_model_retention().await {
            Ok(n) => tracing::debug!(models = n, "read Bedrock's per-model data retention"),
            Err(e) => {
                tracing::debug!(error = %e, "Bedrock's per-model data retention could not be read")
            }
        }
        Ok(())
    }

    /// The listing, from `Self::learned` once primed, else from the cards.
    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        if self.learned.is_empty() {
            self.prime_capabilities().await?;
        }
        if self.learned.is_empty() {
            return Ok(self.table_listing());
        }
        Ok(self
            .learned
            .to_model_infos(PROVIDER_NAME, |id| self.capabilities(id))
            .into_iter()
            .map(|mut info| {
                // The same answer accounting gets: a Claude carries no rate
                // on the listing record and is priced from Anthropic's rows.
                info.pricing = info.pricing.or_else(|| self.pricing(&info.id));
                let mime = self.mime(&info.id);
                info.with_mime(mime)
            })
            .collect())
    }

    /// Always re-reads the listing: a primed store would answer a revoked
    /// key from memory.
    async fn check_credential(&self) -> Result<Vec<ModelInfo>> {
        self.prime_capabilities().await?;
        self.list_models().await
    }

    fn serves_model(&self, model_key: &str) -> Option<String> {
        // Bedrock's ids are unmistakable, and a bare vendor name such as
        // `claude-sonnet-5` is deliberately not claimed: the same model here
        // bills a different account than it does at Anthropic.
        (catalog::is_bedrock_id(model_key)
            || self.learned.contains(model_key)
            || self.capability_overrides.contains_key(model_key))
        .then(|| model_key.to_string())
    }

    fn served_catalog(&self) -> Option<Vec<String>> {
        self.learned.catalog()
    }

    fn learned_models(&self) -> Option<&LearnedModels> {
        Some(&self.learned)
    }

    /// Config first, then what the price file said, then Anthropic's list
    /// price for a Claude: AWS bills Claude through Marketplace at the same
    /// rates Anthropic publishes, and leaves it out of the price file.
    fn pricing(&self, model: &str) -> Option<crate::ModelPricing> {
        self.capability_overrides
            .get(model)
            .and_then(|o| o.pricing())
            .or_else(|| self.learned.get(model).and_then(|m| m.pricing))
            .or_else(|| match catalog::vendor_of(model) {
                catalog::Vendor::Anthropic => {
                    crate::pricing::published_rates("anthropic", catalog::vendor_model(model))
                }
                // An image model's price per image, from the shipped table.
                _ => crate::pricing::published_rates(PROVIDER_NAME, catalog::bare_id(model)),
            })
    }
}

#[cfg(test)]
mod tests;
