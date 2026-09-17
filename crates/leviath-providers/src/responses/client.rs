//! The HTTP side every Responses provider shares: who it authenticates as,
//! where it sends, and what it does when a subscription token lapses.

use std::sync::Arc;

use crate::oauth::{RefreshError, TokenSource};
use crate::provider::{ProviderError, RateLimitConfig, Result, UnavailableReason};
use crate::rate_limit::RateLimiter;

/// How requests authenticate.
#[derive(Clone)]
pub enum Auth {
    /// A static API key, sent as a bearer token.
    Key(String),
    /// A rotating subscription token, refreshed before expiry and once on a
    /// 401.
    Signin(Arc<dyn TokenSource>),
}

/// Hand-written so a key cannot be printed.
impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Key(_) => "Key(<redacted>)",
            Self::Signin(_) => "Signin",
        })
    }
}

/// One host a provider sends to, and everything about sending there.
pub struct Endpoint {
    /// The outbound client.
    pub client: reqwest::Client,
    /// The API root, with no trailing slash.
    pub base_url: String,
    /// The credential.
    pub auth: Auth,
    /// The operator's extra headers, sent after the provider's own.
    pub extra_headers: Vec<(String, String)>,
    /// The rate limit, when one is configured.
    pub rate_limiter: Option<RateLimiter>,
    /// The request deadline, when one is configured.
    pub request_timeout_secs: Option<u64>,
    /// The header a static key goes in instead of `Authorization: Bearer`,
    /// for a host that wants it under its own name (Azure's `api-key`).
    pub auth_header: Option<String>,
}

impl Endpoint {
    /// An endpoint at `base_url` authenticating with `auth`.
    pub fn new(client: reqwest::Client, base_url: &str, auth: Auth) -> Self {
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            auth,
            extra_headers: Vec::new(),
            rate_limiter: None,
            request_timeout_secs: None,
            auth_header: None,
        }
    }

    /// Point at another host. `None` keeps the current one.
    pub fn set_base_url(&mut self, base_url: Option<String>) {
        if let Some(url) = base_url {
            self.base_url = url.trim_end_matches('/').to_string();
        }
    }

    /// Apply a rate limit.
    pub fn set_rate_limit(&mut self, config: Option<&RateLimitConfig>) {
        self.rate_limiter = config.map(RateLimiter::new);
    }

    /// Whether this endpoint bills a subscription rather than a key.
    pub fn is_signin(&self) -> bool {
        matches!(self.auth, Auth::Signin(_))
    }

    /// The bearer to send, refreshing a subscription token first when it is
    /// about to lapse.
    pub async fn bearer(&self) -> Result<String> {
        match &self.auth {
            Auth::Key(key) => Ok(key.clone()),
            Auth::Signin(tokens) => tokens
                .credentials()
                .await
                .map(|c| c.access_token)
                .map_err(signed_out),
        }
    }

    /// One request with `bearer`, uninterpreted. The credential goes in
    /// [`Self::auth_header`] when one is named, and as a bearer token
    /// otherwise.
    async fn once(
        &self,
        builder: impl Fn(&reqwest::Client) -> reqwest::RequestBuilder,
        bearer: &str,
    ) -> Result<reqwest::Response> {
        let request = crate::provider::apply_request_timeout(
            builder(&self.client),
            self.request_timeout_secs,
        );
        let request = match &self.auth_header {
            Some(name) => request.header(name.as_str(), bearer),
            None => request.bearer_auth(bearer),
        };
        crate::provider::with_extra_headers(request, &self.extra_headers)
            .send()
            .await
            .map_err(|e| ProviderError::transport("sending the request", &e))
    }

    /// Send the request `builder` makes, waiting on the rate limit, refreshing
    /// a subscription token once on a 401, and hand back the response
    /// whatever its status. `builder` is called again for the retry, since a
    /// request body can be sent only once.
    pub async fn send(
        &self,
        builder: impl Fn(&reqwest::Client) -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response> {
        if let Some(limiter) = &self.rate_limiter {
            // `acquire` waits for capacity; it has no failure to report.
            let _ = limiter.acquire().await;
        }
        let bearer = self.bearer().await?;
        let response = self.once(&builder, &bearer).await?;
        match (&self.auth, response.status().as_u16()) {
            (Auth::Signin(tokens), 401) => {
                tracing::debug!("a subscription token was rejected; refreshing and retrying once");
                let fresh = tokens.refresh_stale(&bearer).await.map_err(signed_out)?;
                self.once(&builder, &fresh.access_token).await
            }
            _ => Ok(response),
        }
    }

    /// POST `body` as JSON to `path` under the base URL.
    pub async fn post_json(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<reqwest::Response> {
        let url = self.url(path);
        self.send(|client| client.post(&url).json(body)).await
    }

    /// GET a JSON listing at `path` under the base URL, with the shared
    /// listing error handling.
    pub async fn listing(&self, path: &str) -> Result<serde_json::Value> {
        self.get_json(&self.url(path)).await
    }

    /// GET JSON from `url`, which may be on another host of the same vendor,
    /// with the bearer and the listing error handling.
    pub async fn get_json(&self, url: &str) -> Result<serde_json::Value> {
        let response = self.send(|client| client.get(url)).await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(ProviderError::ApiError(format!("HTTP {status}: {body}")));
        }
        crate::provider::decode_json(response).await
    }

    /// `path` under the base URL.
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }
}

/// A refresh that failed, as the error that says to sign in again.
pub fn signed_out(error: RefreshError) -> ProviderError {
    ProviderError::Unavailable {
        reason: UnavailableReason::AuthFailed,
        detail: error.to_string(),
    }
}

/// A refused request whose body was already read.
pub fn api_error(status: u16, body: &str) -> ProviderError {
    match UnavailableReason::classify(status, body) {
        Some(reason) => ProviderError::Unavailable {
            reason,
            detail: body.to_string(),
        },
        None => ProviderError::ApiError(format!("HTTP {status}: {body}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::{Credentials, ProviderGrant};
    use async_trait::async_trait;
    use leviath_testkit::{spawn_mock_sequence, spawn_mock_server};

    struct Gone;

    #[async_trait]
    impl TokenSource for Gone {
        async fn credentials(&self) -> std::result::Result<Credentials, RefreshError> {
            Ok(Credentials::default())
        }
        async fn refresh_stale(&self, _: &str) -> std::result::Result<Credentials, RefreshError> {
            Err(RefreshError::Terminal("the session was revoked".into()))
        }
        fn grant(&self) -> Option<ProviderGrant> {
            None
        }
    }

    #[test]
    fn a_key_never_prints_and_a_base_url_loses_its_slash() {
        assert_eq!(
            format!("{:?}", Auth::Key("secret".into())),
            "Key(<redacted>)"
        );
        assert_eq!(format!("{:?}", Auth::Signin(Arc::new(Gone))), "Signin");
        let mut endpoint = Endpoint::new(
            reqwest::Client::new(),
            "https://a/v1/",
            Auth::Key("k".into()),
        );
        assert_eq!(endpoint.url("/models"), "https://a/v1/models");
        endpoint.set_base_url(None);
        assert_eq!(endpoint.base_url, "https://a/v1");
        endpoint.set_base_url(Some("http://b/".into()));
        assert_eq!(endpoint.base_url, "http://b");
        assert!(!endpoint.is_signin());
    }

    #[tokio::test]
    async fn a_listing_that_is_refused_carries_its_status() {
        let url = spawn_mock_server(404, "Not Found", b"nope".to_vec()).await;
        let err = Endpoint::new(reqwest::Client::new(), &url, Auth::Key("k".into()))
            .listing("/models")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"), "{err}");
    }

    #[tokio::test]
    async fn a_401_the_refresh_cannot_fix_says_so() {
        let (url, _) = spawn_mock_sequence(vec![(401, "Unauthorized", vec![])]).await;
        let mut endpoint =
            Endpoint::new(reqwest::Client::new(), &url, Auth::Signin(Arc::new(Gone)));
        endpoint.set_rate_limit(Some(&RateLimitConfig {
            requests_per_minute: 60,
            tokens_per_minute: 1000,
        }));
        assert!(endpoint.is_signin());
        let err = endpoint
            .post_json("/responses", &serde_json::json!({}))
            .await
            .unwrap_err();
        assert_eq!(
            err.unavailable_reason(),
            Some(UnavailableReason::AuthFailed)
        );
        assert!(err.to_string().contains("revoked"), "{err}");
    }

    #[test]
    fn a_read_refusal_is_classified_like_a_live_one() {
        assert!(
            api_error(400, "prompt too long")
                .to_string()
                .starts_with("API error")
        );
        assert!(Gone.grant().is_none());
        assert_eq!(
            api_error(402, "out of credits").unavailable_reason(),
            Some(UnavailableReason::CreditsExhausted)
        );
    }
}
