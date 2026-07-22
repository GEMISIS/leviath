//! OAuth 2.1 for MCP servers: browser login, token exchange, and refresh.
//!
//! MCP servers advertise themselves as OAuth *public clients* (no secret), so
//! this implements the authorization-code flow with PKCE (RFC 7636) plus
//! dynamic client registration (RFC 7591) and resource indicators (RFC 8707).
//! A standards-correct implementation needs no per-server code.
//!
//! The interactive flow (browser + loopback redirect) lives in
//! `OAuthClient::login`; `OAuthClient::refresh` is non-interactive so a
//! background process can keep a session alive without ever opening a browser.

mod metadata;
mod pkce;
pub mod store;

use std::collections::HashMap;
use std::time::Duration;

use reqwest::Url;
use serde::Deserialize;
use tokio::net::TcpListener;

use metadata::{AuthServerMetadata, ProtectedResourceMetadata};
use pkce::Pkce;
pub use store::{AuthStore, ServerAuth};

/// How the browser gets opened. Injected so tests never launch one.
///
/// A `fn` pointer, not a closure type: production passes
/// [`leviath_sys::open_url`], tests pass a stub that drives the callback
/// directly. Returns whether the launcher spawned.
pub type BrowserOpener = fn(&str) -> bool;

/// How long to wait for the user to finish authorizing in the browser.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

/// The scopes requested when the server advertises none.
const DEFAULT_SCOPES: &str = "openid profile email";

/// The token endpoint's response.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    scope: Option<String>,
}

/// A dynamic client registration response (only the id is needed).
#[derive(Debug, Deserialize)]
struct RegistrationResponse {
    client_id: String,
}

/// Drives OAuth against one MCP server's authorization server.
pub struct OAuthClient {
    http: reqwest::Client,
}

impl Default for OAuthClient {
    fn default() -> Self {
        Self::new()
    }
}

impl OAuthClient {
    /// Build a client with sensible network timeouts.
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(60))
            .build()
            .expect("failed to build reqwest client");
        Self { http }
    }

    /// Run the full interactive login for `mcp_url` and return the tokens.
    ///
    /// `now` (Unix seconds) is passed in rather than read from the clock so the
    /// computed `expires_at` is deterministic under test. `reuse_client_id`
    /// short-circuits dynamic registration when a previous login already
    /// registered this client with the authorization server.
    pub async fn login(
        &self,
        mcp_url: &str,
        headers: &HashMap<String, String>,
        opener: BrowserOpener,
        now: u64,
        reuse_client_id: Option<&str>,
    ) -> anyhow::Result<ServerAuth> {
        let mcp = Url::parse(mcp_url)
            .map_err(|e| anyhow::anyhow!("Invalid MCP server url '{}': {}", mcp_url, e))?;

        let (resource, server_meta) = self.discover(&mcp, headers).await?;

        // Bind the loopback listener first, so its port is known before both
        // registration (which needs the redirect URI) and the authorize URL.
        // Binding an OS-assigned loopback port does not fail in practice; a
        // failure here would mean the machine has no working loopback stack.
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binding an ephemeral loopback port cannot fail");
        let port = listener
            .local_addr()
            .expect("a bound listener always has a local address")
            .port();
        let redirect_uri = format!("http://127.0.0.1:{port}/callback");

        let client_id = match reuse_client_id {
            Some(id) => id.to_string(),
            None => self.register(&server_meta, &redirect_uri).await?,
        };

        let pkce = Pkce::generate();
        let scope = if server_meta.scopes_supported.is_empty() {
            DEFAULT_SCOPES.to_string()
        } else {
            server_meta.scopes_supported.join(" ")
        };
        let authorize_url = build_authorize_url(
            &server_meta.authorization_endpoint,
            &client_id,
            &redirect_uri,
            &pkce,
            &scope,
            &resource,
        )?;

        // Always print the URL: on a headless or SSH session the browser can't
        // open, and the user needs to paste it themselves.
        println!("Opening your browser to authorize:\n  {authorize_url}");
        if !opener(authorize_url.as_str()) {
            println!("(couldn't open a browser automatically — open the link above)");
        }

        let code = wait_for_callback(listener, &pkce.state, CALLBACK_TIMEOUT).await?;

        let token = self
            .exchange_code(
                &server_meta.token_endpoint,
                &client_id,
                &redirect_uri,
                &code,
                &pkce.verifier,
                &resource,
            )
            .await?;

        Ok(build_server_auth(
            resource,
            &server_meta,
            client_id,
            token,
            now,
        ))
    }

    /// Refresh `auth` non-interactively. Never opens a browser.
    pub async fn refresh(&self, auth: &ServerAuth, now: u64) -> anyhow::Result<ServerAuth> {
        let refresh_token = auth
            .refresh_token
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("no refresh token available"))?;

        let params = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", auth.client_id.as_str()),
            ("resource", auth.resource.as_str()),
        ];
        let value = self
            .post_form(&auth.token_endpoint, &params)
            .await
            .map_err(|e| anyhow::anyhow!("token refresh failed: {}", e))?;
        let token: TokenResponse = serde_json::from_value(value)
            .map_err(|e| anyhow::anyhow!("could not parse token response: {}", e))?;

        let mut refreshed = auth.clone();
        refreshed.access_token = token.access_token;
        // A refresh may or may not rotate the refresh token; keep the old one
        // if the server did not send a new one.
        if let Some(new_refresh) = token.refresh_token {
            refreshed.refresh_token = Some(new_refresh);
        }
        refreshed.expires_at = expires_at(token.expires_in, now);
        if let Some(scope) = token.scope {
            refreshed.scope = scope;
        }
        Ok(refreshed)
    }

    /// Resolve the `Authorization` header for a stored server, refreshing the
    /// token first if it is at or near expiry.
    ///
    /// Non-interactive: a dead refresh returns an error naming the login
    /// command rather than opening a browser, so the daemon can call this
    /// safely. A refreshed token is written back to `store_path`. Returns
    /// `None` when the server has no stored auth (e.g. an unauthenticated
    /// server, or one using a static header).
    pub async fn authorization_header(
        &self,
        server_name: &str,
        store_path: &std::path::Path,
        now: u64,
    ) -> anyhow::Result<Option<(String, String)>> {
        let mut store = AuthStore::load(store_path)?;
        let Some(auth) = store.get(server_name) else {
            return Ok(None);
        };

        let token = if auth.is_expired_at(now) {
            let refreshed = self.refresh(auth, now).await.map_err(|e| {
                anyhow::anyhow!(
                    "MCP server '{server_name}' token expired and could not be \
                     refreshed ({e}); re-authenticate with `lev mcp login {server_name}`"
                )
            })?;
            let access = refreshed.access_token.clone();
            store.set(server_name, refreshed);
            store.save(store_path)?;
            access
        } else {
            auth.access_token.clone()
        };

        Ok(Some((
            "Authorization".to_string(),
            format!("Bearer {token}"),
        )))
    }

    /// Discover the resource identifier and authorization-server metadata.
    async fn discover(
        &self,
        mcp: &Url,
        headers: &HashMap<String, String>,
    ) -> anyhow::Result<(String, AuthServerMetadata)> {
        // A probe request surfaces the WWW-Authenticate hint; a server that
        // answers it without auth still yields the well-known document.
        let www_authenticate = self.probe_challenge(mcp, headers).await;
        let resource_meta_url = match metadata::resource_metadata_url(www_authenticate.as_deref()) {
            Some(url) => url,
            None => metadata::well_known_resource_url(mcp).to_string(),
        };

        let value = self
            .get_json(&resource_meta_url)
            .await
            .map_err(|e| anyhow::anyhow!("failed to fetch resource metadata: {}", e))?;
        let resource_meta: ProtectedResourceMetadata = serde_json::from_value(value)
            .map_err(|e| anyhow::anyhow!("failed to parse resource metadata: {}", e))?;

        let issuer = resource_meta
            .authorization_servers
            .first()
            .ok_or_else(|| anyhow::anyhow!("resource metadata names no authorization server"))?;
        // Fall back to the MCP URL itself as the resource identifier if the
        // document omits it (some servers do).
        let resource = if resource_meta.resource.is_empty() {
            mcp.to_string()
        } else {
            resource_meta.resource.clone()
        };

        let server_meta = self.fetch_auth_server_metadata(issuer).await?;
        Ok((resource, server_meta))
    }

    /// Fetch AS metadata, trying RFC 8414 then the OpenID fallback.
    async fn fetch_auth_server_metadata(&self, issuer: &str) -> anyhow::Result<AuthServerMetadata> {
        let mut last_err = None;
        for url in metadata::auth_server_metadata_urls(issuer)? {
            match self.fetch_one_auth_server_metadata(url.as_str()).await {
                Ok(meta) => return Ok(meta),
                Err(e) => last_err = Some(e),
            }
        }
        Err(anyhow::anyhow!(
            "failed to fetch authorization server metadata: {}",
            last_err.expect("at least one candidate URL is always tried")
        ))
    }

    /// Fetch and parse AS metadata from one candidate URL.
    async fn fetch_one_auth_server_metadata(
        &self,
        url: &str,
    ) -> anyhow::Result<AuthServerMetadata> {
        let value = self.get_json(url).await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Probe the MCP endpoint and return its `WWW-Authenticate` header, if any.
    ///
    /// A network failure here is not fatal: discovery falls back to the
    /// well-known path, so a `None` simply means "no hint".
    async fn probe_challenge(
        &self,
        mcp: &Url,
        headers: &HashMap<String, String>,
    ) -> Option<String> {
        let mut request = self.http.post(mcp.clone()).body("{}");
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let response = request.send().await.ok()?;
        response
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    }

    /// Register this client dynamically (RFC 7591), returning its id.
    async fn register(
        &self,
        server_meta: &AuthServerMetadata,
        redirect_uri: &str,
    ) -> anyhow::Result<String> {
        let endpoint = server_meta
            .registration_endpoint
            .as_deref()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "authorization server does not support dynamic client registration; \
                 a client id must be configured manually"
                )
            })?;

        let body = serde_json::json!({
            "client_name": "Leviath",
            "redirect_uris": [redirect_uri],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
        });
        let response = self
            .http
            .post(endpoint)
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("client registration request failed: {}", e))?;
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!(
                "client registration failed with HTTP {}: {}",
                status,
                text.trim()
            ));
        }
        let registration: RegistrationResponse = response
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("failed to parse registration response: {}", e))?;
        Ok(registration.client_id)
    }

    /// Exchange an authorization code for tokens.
    async fn exchange_code(
        &self,
        token_endpoint: &str,
        client_id: &str,
        redirect_uri: &str,
        code: &str,
        verifier: &str,
        resource: &str,
    ) -> anyhow::Result<TokenResponse> {
        let params = [
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", client_id),
            ("code_verifier", verifier),
            ("resource", resource),
        ];
        let value = self
            .post_form(token_endpoint, &params)
            .await
            .map_err(|e| anyhow::anyhow!("token exchange failed: {}", e))?;
        serde_json::from_value(value)
            .map_err(|e| anyhow::anyhow!("could not parse token response: {}", e))
    }

    /// GET a URL and return its JSON body as a value.
    ///
    /// Non-generic on purpose: a `<T>` version generates a separate llvm-cov
    /// instantiation per return type, and the error arms of the unused ones
    /// read as uncovered. Callers deserialize the returned value concretely.
    async fn get_json(&self, url: &str) -> anyhow::Result<serde_json::Value> {
        let response = self.http.get(url).send().await?;
        if !response.status().is_success() {
            anyhow::bail!("HTTP {}", response.status());
        }
        Ok(response.json().await?)
    }

    /// POST a form and return the JSON response as a value, surfacing an OAuth
    /// error body rather than a bare status. Non-generic for the same reason as
    /// [`Self::get_json`].
    async fn post_form(
        &self,
        url: &str,
        params: &[(&str, &str)],
    ) -> anyhow::Result<serde_json::Value> {
        let response = self.http.post(url).form(params).send().await?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("HTTP {}: {}", status, body.trim());
        }
        serde_json::from_str(&body)
            .map_err(|e| anyhow::anyhow!("could not parse token response: {}", e))
    }
}

/// Compose the browser authorization URL.
fn build_authorize_url(
    endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    pkce: &Pkce,
    scope: &str,
    resource: &str,
) -> anyhow::Result<Url> {
    let mut url = Url::parse(endpoint)
        .map_err(|e| anyhow::anyhow!("Invalid authorization endpoint '{}': {}", endpoint, e))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &pkce.state)
        .append_pair("scope", scope)
        // RFC 8707: bind the issued token to this specific MCP server.
        .append_pair("resource", resource);
    Ok(url)
}

/// Assemble the stored auth from a token response.
fn build_server_auth(
    resource: String,
    server_meta: &AuthServerMetadata,
    client_id: String,
    token: TokenResponse,
    now: u64,
) -> ServerAuth {
    ServerAuth {
        resource,
        issuer: server_meta.issuer.clone(),
        authorization_endpoint: server_meta.authorization_endpoint.clone(),
        token_endpoint: server_meta.token_endpoint.clone(),
        client_id,
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_at: expires_at(token.expires_in, now),
        scope: token.scope.unwrap_or_default(),
    }
}

/// Absolute expiry from a relative `expires_in`, or `0` (unknown) when the
/// server omits it.
fn expires_at(expires_in: Option<u64>, now: u64) -> u64 {
    match expires_in {
        Some(secs) => now.saturating_add(secs),
        None => 0,
    }
}

/// Accept the browser redirect on the loopback listener and return the code.
///
/// Validates `state` to reject a forged or replayed callback, replies with a
/// human-friendly page, and gives up after [`CALLBACK_TIMEOUT`].
async fn wait_for_callback(
    listener: TcpListener,
    expected_state: &str,
    timeout: Duration,
) -> anyhow::Result<String> {
    let accept = async {
        loop {
            // Accepting on a freshly-bound loopback listener does not fail;
            // connection resets surface later, on read, not here.
            let (stream, _) = listener
                .accept()
                .await
                .expect("accepting on a bound loopback listener cannot fail");
            // A browser may make incidental requests (favicon, etc); only the
            // one carrying our params counts.
            if let Some(result) = handle_callback_connection(stream, expected_state).await? {
                return Ok(result);
            }
        }
    };

    match tokio::time::timeout(timeout, accept).await {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!(
            "timed out waiting for browser authorization"
        )),
    }
}

/// Handle one loopback connection.
///
/// Returns `Ok(Some(code))` for the authorization callback, `Ok(None)` for an
/// unrelated request (so the caller keeps listening), and `Err` for a callback
/// that arrived but was invalid (mismatched state, or an OAuth `error`).
async fn handle_callback_connection(
    mut stream: tokio::net::TcpStream,
    expected_state: &str,
) -> anyhow::Result<Option<String>> {
    use tokio::io::AsyncReadExt;

    let mut buf = vec![0u8; 8192];
    let n = stream.read(&mut buf).await.unwrap_or(0);
    let request = String::from_utf8_lossy(&buf[..n]);
    let Some(target) = request_target(&request) else {
        return Ok(None);
    };
    if !target.starts_with("/callback") {
        write_response(&mut stream, "404 Not Found", "Not found.").await;
        return Ok(None);
    }

    let params = query_params(target);
    if let Some(error) = params.get("error") {
        write_response(&mut stream, "400 Bad Request", "Authorization failed.").await;
        return Err(anyhow::anyhow!("authorization server returned: {}", error));
    }
    match (params.get("code"), params.get("state")) {
        (Some(code), Some(state)) if state == expected_state => {
            write_response(
                &mut stream,
                "200 OK",
                "Authorization complete — you can close this tab and return to Leviath.",
            )
            .await;
            Ok(Some(code.clone()))
        }
        (_, Some(_)) => {
            // A state mismatch means a forged or stale callback.
            write_response(
                &mut stream,
                "400 Bad Request",
                "Invalid authorization state.",
            )
            .await;
            Err(anyhow::anyhow!("OAuth state mismatch — rejecting callback"))
        }
        _ => {
            write_response(
                &mut stream,
                "400 Bad Request",
                "Missing authorization code.",
            )
            .await;
            Err(anyhow::anyhow!("callback missing code or state"))
        }
    }
}

/// The request target (`/callback?…`) from an HTTP request line.
fn request_target(request: &str) -> Option<&str> {
    let line = request.lines().next()?;
    let mut parts = line.split_whitespace();
    let _method = parts.next()?;
    parts.next()
}

/// Parse the query string of a request target into a map.
fn query_params(target: &str) -> HashMap<String, String> {
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
    form_urlencoded::parse(query.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

/// Write a minimal HTML response and close the connection.
async fn write_response(stream: &mut tokio::net::TcpStream, status: &str, message: &str) {
    use tokio::io::AsyncWriteExt;
    let body = format!("<!doctype html><meta charset=utf-8><p>{message}</p>");
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── build_authorize_url ──────────────────────────────────────────────

    fn fixed_pkce() -> Pkce {
        Pkce {
            verifier: "verifier".to_string(),
            challenge: "challenge".to_string(),
            state: "state123".to_string(),
        }
    }

    #[test]
    fn authorize_url_carries_every_required_parameter() {
        let url = build_authorize_url(
            "https://auth.example.com/authorize",
            "client-1",
            "http://127.0.0.1:5000/callback",
            &fixed_pkce(),
            "openid profile",
            "https://mcp.example.com/mcp",
        )
        .unwrap();
        let params: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(params["response_type"], "code");
        assert_eq!(params["client_id"], "client-1");
        assert_eq!(params["redirect_uri"], "http://127.0.0.1:5000/callback");
        assert_eq!(params["code_challenge"], "challenge");
        assert_eq!(params["code_challenge_method"], "S256");
        assert_eq!(params["state"], "state123");
        assert_eq!(params["scope"], "openid profile");
        // RFC 8707 resource binding is mandatory since MCP 2025-06-18.
        assert_eq!(params["resource"], "https://mcp.example.com/mcp");
    }

    #[test]
    fn authorize_url_rejects_a_bad_endpoint() {
        assert!(
            build_authorize_url(
                "not a url",
                "c",
                "http://127.0.0.1/callback",
                &fixed_pkce(),
                "openid",
                "https://mcp",
            )
            .is_err()
        );
    }

    // ─── expires_at ───────────────────────────────────────────────────────

    #[test]
    fn expires_at_adds_the_relative_lifetime() {
        assert_eq!(expires_at(Some(3600), 1_000), 4_600);
    }

    #[test]
    fn expires_at_is_zero_when_unknown() {
        assert_eq!(expires_at(None, 1_000), 0);
    }

    // ─── request parsing ──────────────────────────────────────────────────

    #[test]
    fn request_target_reads_the_path() {
        assert_eq!(
            request_target("GET /callback?code=abc HTTP/1.1\r\nHost: x\r\n\r\n"),
            Some("/callback?code=abc")
        );
    }

    #[test]
    fn request_target_of_garbage_is_none() {
        assert_eq!(request_target(""), None);
        // A method with no target (the `?` on the second token).
        assert_eq!(request_target("GET"), None);
        // A whitespace-only line: a non-empty first line that yields no tokens.
        assert_eq!(request_target("   \r\n"), None);
    }

    #[test]
    fn query_params_parses_pairs() {
        let params = query_params("/callback?code=abc&state=xyz");
        assert_eq!(params["code"], "abc");
        assert_eq!(params["state"], "xyz");
    }

    #[test]
    fn query_params_of_a_bare_path_is_empty() {
        assert!(query_params("/callback").is_empty());
    }

    // ─── build_server_auth ────────────────────────────────────────────────

    fn server_meta() -> AuthServerMetadata {
        serde_json::from_value(serde_json::json!({
            "issuer": "https://auth.example.com",
            "authorization_endpoint": "https://auth.example.com/authorize",
            "token_endpoint": "https://auth.example.com/token",
        }))
        .unwrap()
    }

    #[test]
    fn build_server_auth_populates_every_field() {
        let token = TokenResponse {
            access_token: "at".to_string(),
            refresh_token: Some("rt".to_string()),
            expires_in: Some(3600),
            scope: Some("openid".to_string()),
        };
        let auth = build_server_auth(
            "https://mcp.example.com/mcp".to_string(),
            &server_meta(),
            "client-1".to_string(),
            token,
            1_000,
        );
        assert_eq!(auth.resource, "https://mcp.example.com/mcp");
        assert_eq!(auth.issuer, "https://auth.example.com");
        assert_eq!(auth.client_id, "client-1");
        assert_eq!(auth.access_token, "at");
        assert_eq!(auth.refresh_token.as_deref(), Some("rt"));
        assert_eq!(auth.expires_at, 4_600);
        assert_eq!(auth.scope, "openid");
    }

    #[test]
    fn build_server_auth_defaults_a_missing_scope() {
        let token = TokenResponse {
            access_token: "at".to_string(),
            refresh_token: None,
            expires_in: None,
            scope: None,
        };
        let auth = build_server_auth(
            "https://mcp".to_string(),
            &server_meta(),
            "c".to_string(),
            token,
            0,
        );
        assert_eq!(auth.scope, "");
        assert_eq!(auth.expires_at, 0);
        assert!(auth.refresh_token.is_none());
    }

    // ─── loopback callback handling ───────────────────────────────────────
    //
    // wait_for_callback binds a real listener; these drive it with a real TCP
    // client, exactly as a browser redirect would, so the accept loop, state
    // check, and response writing are all exercised without a browser.

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    /// Send one raw HTTP request line to `addr` and return the response text.
    async fn hit(addr: std::net::SocketAddr, target: &str) -> String {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let request = format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();
        let mut buf = Vec::new();
        let _ = stream.read_to_end(&mut buf).await;
        String::from_utf8_lossy(&buf).into_owned()
    }

    #[tokio::test]
    async fn callback_returns_the_code_on_a_matching_state() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            wait_for_callback(listener, "st8", Duration::from_secs(5)).await
        });

        let response = hit(addr, "/callback?code=the-code&state=st8").await;
        assert!(response.contains("200 OK"), "got: {response}");
        assert!(
            response.contains("Authorization complete"),
            "got: {response}"
        );
        assert_eq!(server.await.unwrap().unwrap(), "the-code");
    }

    #[tokio::test]
    async fn callback_skips_unrelated_requests_then_accepts_the_real_one() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            wait_for_callback(listener, "st8", Duration::from_secs(5)).await
        });

        // A browser often fetches /favicon.ico first; it must not end the wait.
        let favicon = hit(addr, "/favicon.ico").await;
        assert!(favicon.contains("404"), "got: {favicon}");
        let ok = hit(addr, "/callback?code=c&state=st8").await;
        assert!(ok.contains("200 OK"));
        assert_eq!(server.await.unwrap().unwrap(), "c");
    }

    #[tokio::test]
    async fn callback_rejects_a_mismatched_state() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            wait_for_callback(listener, "expected", Duration::from_secs(5)).await
        });

        let response = hit(addr, "/callback?code=c&state=forged").await;
        assert!(response.contains("400"), "got: {response}");
        let err = server.await.unwrap().expect_err("mismatch must fail");
        assert!(err.to_string().contains("state mismatch"), "got: {err}");
    }

    #[tokio::test]
    async fn callback_surfaces_an_oauth_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server =
            tokio::spawn(
                async move { wait_for_callback(listener, "s", Duration::from_secs(5)).await },
            );

        let response = hit(addr, "/callback?error=access_denied").await;
        assert!(response.contains("400"), "got: {response}");
        let err = server.await.unwrap().expect_err("error param must fail");
        assert!(err.to_string().contains("access_denied"), "got: {err}");
    }

    #[tokio::test]
    async fn callback_rejects_a_request_missing_code_and_state() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server =
            tokio::spawn(
                async move { wait_for_callback(listener, "s", Duration::from_secs(5)).await },
            );

        let response = hit(addr, "/callback?nothing=here").await;
        assert!(response.contains("400"), "got: {response}");
        assert!(server.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn handle_callback_ignores_an_empty_connection() {
        // A connection that sends nothing yields no request line, so it is
        // neither the callback nor an error — just skipped.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accept = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle_callback_connection(stream, "s").await
        });
        // Connect and immediately close without writing.
        let stream = TcpStream::connect(addr).await.unwrap();
        drop(stream);
        let outcome = accept
            .await
            .unwrap()
            .expect("empty connection is not an error");
        assert!(outcome.is_none(), "an empty connection yields no code");
    }

    // ─── full OAuth flows against a mock authorization server ─────────────

    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone)]
    struct MockAs {
        base: String,
        registrations: Arc<AtomicUsize>,
    }

    /// A standards-correct mock: protected-resource + AS metadata, dynamic
    /// registration, and a token endpoint. `variant` toggles which discovery
    /// quirks to exercise.
    async fn mock_auth_server(variant: &'static str) -> MockAs {
        let registrations = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let state = MockAs {
            base: base.clone(),
            registrations: registrations.clone(),
        };

        let app_state = (base.clone(), variant, registrations.clone());
        let app = Router::new()
            .route(
                "/mcp",
                post(|State((base, _, _)): State<(String, &'static str, Arc<AtomicUsize>)>| async move {
                    // Unauthenticated probe → 401 with the resource hint
                    // pointing at this server's own well-known document.
                    let hint = format!(
                        "Bearer resource_metadata=\"{base}/.well-known/oauth-protected-resource\""
                    );
                    (
                        StatusCode::UNAUTHORIZED,
                        [(reqwest::header::WWW_AUTHENTICATE, hint)],
                    )
                }),
            )
            .route(
                "/.well-known/oauth-protected-resource",
                get(|State((base, variant, _)): State<(String, &'static str, Arc<AtomicUsize>)>| async move {
                    if variant == "discover_fails" {
                        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                    }
                    if variant == "resource_not_object" {
                        // Valid JSON, but not a ProtectedResourceMetadata object.
                        return Json(serde_json::json!("just a string")).into_response();
                    }
                    let resource = if variant == "no_resource" {
                        serde_json::Value::String(String::new())
                    } else {
                        serde_json::json!(format!("{base}/mcp"))
                    };
                    let servers = match variant {
                        "no_auth_server" => serde_json::json!([]),
                        "bad_issuer" => serde_json::json!(["not a url"]),
                        _ => serde_json::json!([base]),
                    };
                    Json(serde_json::json!({
                        "resource": resource,
                        "authorization_servers": servers,
                    }))
                    .into_response()
                }),
            )
            .route(
                "/.well-known/oauth-authorization-server",
                get(|State((base, variant, _)): State<(String, &'static str, Arc<AtomicUsize>)>| async move {
                    if variant == "no_rfc8414" || variant == "no_metadata" {
                        return StatusCode::NOT_FOUND.into_response();
                    }
                    if variant == "as_bad_rfc8414" {
                        // Valid JSON, but missing the required AS metadata
                        // fields, so parsing fails and discovery tries OpenID.
                        return Json(serde_json::json!({ "not": "metadata" })).into_response();
                    }
                    as_metadata(&base, variant).into_response()
                }),
            )
            .route(
                "/.well-known/openid-configuration",
                get(|State((base, variant, _)): State<(String, &'static str, Arc<AtomicUsize>)>| async move {
                    if variant == "no_metadata" {
                        return StatusCode::NOT_FOUND.into_response();
                    }
                    as_metadata(&base, variant).into_response()
                }),
            )
            .route(
                "/register",
                post(|State((base, variant, regs)): State<(String, &'static str, Arc<AtomicUsize>)>, _body: String| async move {
                    regs.fetch_add(1, Ordering::SeqCst);
                    let _ = base;
                    if variant == "register_fails" {
                        return (StatusCode::BAD_REQUEST, "invalid_redirect_uri").into_response();
                    }
                    if variant == "register_bad_json" {
                        return (StatusCode::OK, "not json").into_response();
                    }
                    Json(serde_json::json!({ "client_id": "registered-client" })).into_response()
                }),
            )
            .route(
                "/token",
                post(|State((_, variant, _)): State<(String, &'static str, Arc<AtomicUsize>)>, body: String| async move {
                    // A refresh with a bad token is the one failure we model.
                    if body.contains("refresh_token=bad") {
                        return (StatusCode::BAD_REQUEST, "invalid_grant").into_response();
                    }
                    if variant == "bad_token_json" {
                        return (StatusCode::OK, "not json").into_response();
                    }
                    if variant == "exchange_fails" && body.contains("authorization_code") {
                        return (StatusCode::BAD_REQUEST, "invalid_grant").into_response();
                    }
                    if variant == "minimal_token" {
                        return Json(serde_json::json!({ "access_token": "minimal" }))
                            .into_response();
                    }
                    if variant == "token_no_access" {
                        // Valid JSON, but not a TokenResponse (no access_token).
                        return Json(serde_json::json!({ "wat": true })).into_response();
                    }
                    Json(serde_json::json!({
                        "access_token": "new-access",
                        "refresh_token": "new-refresh",
                        "expires_in": 3600,
                        "scope": "openid",
                    }))
                    .into_response()
                }),
            )
            .with_state(app_state);

        tokio::spawn(std::future::IntoFuture::into_future(axum::serve(
            listener, app,
        )));
        state
    }

    fn as_metadata(base: &str, variant: &'static str) -> Json<serde_json::Value> {
        let scopes: Vec<&str> = if variant == "no_scopes" {
            vec![]
        } else {
            vec!["openid", "profile"]
        };
        let authorize = if variant == "bad_authorize" {
            "not a url".to_string()
        } else {
            format!("{base}/authorize")
        };
        let mut meta = serde_json::json!({
            "issuer": base,
            "authorization_endpoint": authorize,
            "token_endpoint": format!("{base}/token"),
            "scopes_supported": scopes,
        });
        if variant != "no_registration" {
            meta["registration_endpoint"] = serde_json::json!(format!("{base}/register"));
        }
        Json(meta)
    }

    /// Drive the loopback redirect exactly as a browser would after consent.
    ///
    /// `state_override` forges the CSRF state (for the mismatch test);
    /// otherwise the real state from the authorize URL is echoed back. One
    /// spawn site shared by both consent stubs.
    fn drive_callback(authorize_url: &str, state_override: Option<&str>) {
        let url = Url::parse(authorize_url).unwrap();
        let params: HashMap<_, _> = url.query_pairs().into_owned().collect();
        let redirect = params["redirect_uri"].clone();
        let state = state_override
            .map(String::from)
            .unwrap_or_else(|| params["state"].clone());
        // Spawned onto the same runtime; login is concurrently awaiting accept.
        tokio::spawn(async move {
            let callback = format!("{redirect}?code=auth-code&state={state}");
            let _ = reqwest::Client::new().get(&callback).send().await;
        });
    }

    /// A fake browser that consents successfully.
    fn auto_consent(authorize_url: &str) -> bool {
        drive_callback(authorize_url, None);
        true
    }

    #[tokio::test]
    async fn full_login_round_trip() {
        let server = mock_auth_server("default").await;
        let auth = OAuthClient::new()
            .login(
                &format!("{}/mcp", server.base),
                &HashMap::new(),
                auto_consent,
                1_000,
                None,
            )
            .await
            .expect("login should complete");

        assert_eq!(auth.access_token, "new-access");
        assert_eq!(auth.refresh_token.as_deref(), Some("new-refresh"));
        assert_eq!(auth.expires_at, 4_600);
        assert_eq!(auth.client_id, "registered-client");
        assert_eq!(server.registrations.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn login_reuses_a_known_client_id_and_skips_registration() {
        let server = mock_auth_server("default").await;
        OAuthClient::new()
            .login(
                &format!("{}/mcp", server.base),
                &HashMap::new(),
                auto_consent,
                0,
                Some("existing-client"),
            )
            .await
            .expect("login should complete");
        assert_eq!(
            server.registrations.load(Ordering::SeqCst),
            0,
            "a known client id must not re-register"
        );
    }

    #[tokio::test]
    async fn login_falls_back_when_rfc8414_metadata_is_malformed() {
        // RFC 8414 returns unparseable metadata; discovery must recover via the
        // OpenID document rather than giving up.
        let server = mock_auth_server("as_bad_rfc8414").await;
        let auth = OAuthClient::new()
            .login(
                &format!("{}/mcp", server.base),
                &HashMap::new(),
                auto_consent,
                0,
                None,
            )
            .await
            .expect("openid recovery should work");
        assert_eq!(auth.access_token, "new-access");
    }

    #[tokio::test]
    async fn login_falls_back_to_openid_configuration() {
        // RFC 8414 404s, so discovery must try the OpenID document.
        let server = mock_auth_server("no_rfc8414").await;
        let auth = OAuthClient::new()
            .login(
                &format!("{}/mcp", server.base),
                &HashMap::new(),
                auto_consent,
                0,
                None,
            )
            .await
            .expect("openid fallback should work");
        assert_eq!(auth.access_token, "new-access");
    }

    #[tokio::test]
    async fn login_fails_when_registration_is_unsupported() {
        let server = mock_auth_server("no_registration").await;
        let err = OAuthClient::new()
            .login(
                &format!("{}/mcp", server.base),
                &HashMap::new(),
                auto_consent,
                0,
                None,
            )
            .await
            .expect_err("no registration endpoint and no client id must fail");
        assert!(
            err.to_string().contains("dynamic client registration"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn refresh_rotates_the_tokens() {
        let server = mock_auth_server("default").await;
        let auth = ServerAuth {
            resource: format!("{}/mcp", server.base),
            issuer: server.base.clone(),
            authorization_endpoint: format!("{}/authorize", server.base),
            token_endpoint: format!("{}/token", server.base),
            client_id: "c".to_string(),
            access_token: "old".to_string(),
            refresh_token: Some("good".to_string()),
            expires_at: 500,
            scope: String::new(),
        };
        let refreshed = OAuthClient::new().refresh(&auth, 2_000).await.unwrap();
        assert_eq!(refreshed.access_token, "new-access");
        assert_eq!(refreshed.refresh_token.as_deref(), Some("new-refresh"));
        assert_eq!(refreshed.expires_at, 5_600);
    }

    #[tokio::test]
    async fn refresh_keeps_the_old_token_when_none_is_returned() {
        // A server that returns only an access_token must not wipe the refresh
        // token or scope we already hold.
        let server = mock_auth_server("minimal_token").await;
        let auth = ServerAuth {
            token_endpoint: format!("{}/token", server.base),
            refresh_token: Some("keep-me".to_string()),
            scope: "openid".to_string(),
            ..Default::default()
        };
        let refreshed = OAuthClient::new().refresh(&auth, 0).await.unwrap();
        assert_eq!(refreshed.access_token, "minimal");
        assert_eq!(refreshed.refresh_token.as_deref(), Some("keep-me"));
        assert_eq!(refreshed.scope, "openid");
        assert_eq!(refreshed.expires_at, 0);
    }

    #[tokio::test]
    async fn authorization_header_is_none_without_stored_auth() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("mcp-auth.json");
        let header = OAuthClient::new()
            .authorization_header("unknown", &store, 0)
            .await
            .unwrap();
        assert!(header.is_none());
    }

    #[tokio::test]
    async fn authorization_header_returns_a_fresh_token_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("mcp-auth.json");
        let mut store = AuthStore::default();
        store.set(
            "srv",
            ServerAuth {
                access_token: "still-good".to_string(),
                expires_at: 10_000,
                ..Default::default()
            },
        );
        store.save(&store_path).unwrap();

        let header = OAuthClient::new()
            .authorization_header("srv", &store_path, 1_000)
            .await
            .unwrap()
            .expect("a stored token yields a header");
        assert_eq!(
            header,
            ("Authorization".to_string(), "Bearer still-good".to_string())
        );
    }

    #[tokio::test]
    async fn authorization_header_refreshes_an_expired_token_and_persists_it() {
        let server = mock_auth_server("default").await;
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("mcp-auth.json");
        let mut store = AuthStore::default();
        store.set(
            "srv",
            ServerAuth {
                token_endpoint: format!("{}/token", server.base),
                access_token: "expired".to_string(),
                refresh_token: Some("good".to_string()),
                expires_at: 100,
                ..Default::default()
            },
        );
        store.save(&store_path).unwrap();

        let header = OAuthClient::new()
            .authorization_header("srv", &store_path, 1_000)
            .await
            .unwrap()
            .expect("an expired token is refreshed");
        assert_eq!(header.1, "Bearer new-access");
        // The rotated token is written back for next time.
        let reloaded = AuthStore::load(&store_path).unwrap();
        assert_eq!(reloaded.get("srv").unwrap().access_token, "new-access");
    }

    #[tokio::test]
    async fn authorization_header_names_the_login_command_when_refresh_fails() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("mcp-auth.json");
        let mut store = AuthStore::default();
        store.set(
            "srv",
            ServerAuth {
                token_endpoint: "http://127.0.0.1:1/token".to_string(),
                access_token: "expired".to_string(),
                refresh_token: Some("good".to_string()),
                expires_at: 100,
                ..Default::default()
            },
        );
        store.save(&store_path).unwrap();

        let err = OAuthClient::new()
            .authorization_header("srv", &store_path, 1_000)
            .await
            .expect_err("a dead refresh must fail");
        assert!(err.to_string().contains("lev mcp login srv"), "got: {err}");
    }

    #[tokio::test]
    async fn authorization_header_surfaces_an_unreadable_store() {
        // The store path is a directory, so loading it fails.
        let dir = tempfile::tempdir().unwrap();
        assert!(
            OAuthClient::new()
                .authorization_header("srv", dir.path(), 0)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn authorization_header_surfaces_an_unwritable_store_after_refresh() {
        // Refresh succeeds, but the read-only store can't persist the rotated
        // token.
        let server = mock_auth_server("default").await;
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("mcp-auth.json");
        let mut store = AuthStore::default();
        store.set(
            "srv",
            ServerAuth {
                token_endpoint: format!("{}/token", server.base),
                refresh_token: Some("good".to_string()),
                expires_at: 100,
                ..Default::default()
            },
        );
        store.save(&store_path).unwrap();
        let mut perms = std::fs::metadata(&store_path).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&store_path, perms).unwrap();

        assert!(
            OAuthClient::new()
                .authorization_header("srv", &store_path, 1_000)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn refresh_without_a_token_is_an_error() {
        let mut auth = ServerAuth {
            token_endpoint: "http://127.0.0.1:1/token".to_string(),
            ..Default::default()
        };
        auth.refresh_token = None;
        let err = OAuthClient::new()
            .refresh(&auth, 0)
            .await
            .expect_err("no refresh token must fail");
        assert!(err.to_string().contains("no refresh token"), "got: {err}");
    }

    #[tokio::test]
    async fn refresh_surfaces_a_rejected_grant() {
        let server = mock_auth_server("default").await;
        let auth = ServerAuth {
            token_endpoint: format!("{}/token", server.base),
            refresh_token: Some("bad".to_string()),
            ..Default::default()
        };
        let err = OAuthClient::new()
            .refresh(&auth, 0)
            .await
            .expect_err("a rejected grant must fail");
        assert!(err.to_string().contains("refresh failed"), "got: {err}");
    }

    #[tokio::test]
    async fn discovery_without_a_www_authenticate_uses_the_well_known_path() {
        // A server that answers the probe *without* a challenge still resolves
        // via the well-known document.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/mcp", post(|| async { StatusCode::OK }))
            .route(
                "/.well-known/oauth-protected-resource",
                get({
                    let base = base.clone();
                    move || {
                        let base = base.clone();
                        async move {
                            Json(serde_json::json!({
                                "resource": format!("{base}/mcp"),
                                "authorization_servers": [base],
                            }))
                        }
                    }
                }),
            )
            .route(
                "/.well-known/oauth-authorization-server",
                get({
                    let base = base.clone();
                    move || {
                        let base = base.clone();
                        async move { as_metadata(&base, "default") }
                    }
                }),
            )
            .route(
                "/register",
                post(|| async { Json(serde_json::json!({ "client_id": "c" })) }),
            )
            .route(
                "/token",
                post(|| async {
                    Json(serde_json::json!({"access_token": "at", "expires_in": 60}))
                }),
            );
        tokio::spawn(std::future::IntoFuture::into_future(axum::serve(
            listener, app,
        )));

        let auth = OAuthClient::new()
            .login(
                &format!("{base}/mcp"),
                &HashMap::new(),
                auto_consent,
                0,
                None,
            )
            .await
            .expect("well-known discovery should work");
        assert_eq!(auth.access_token, "at");
    }

    #[test]
    fn oauth_client_default_matches_new() {
        // Both build a usable client; `default` just delegates.
        let _ = OAuthClient::default();
    }

    #[tokio::test]
    async fn callback_times_out_when_no_redirect_arrives() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        // Nobody connects, so the tiny timeout must fire.
        let err = wait_for_callback(listener, "s", Duration::from_millis(100))
            .await
            .expect_err("must time out");
        assert!(err.to_string().contains("timed out"), "got: {err}");
    }

    #[tokio::test]
    async fn login_sends_configured_probe_headers() {
        // A non-empty header map exercises the probe header loop; the server
        // does not require it, so login still completes.
        let server = mock_auth_server("default").await;
        let headers = HashMap::from([("X-Probe".to_string(), "1".to_string())]);
        OAuthClient::new()
            .login(
                &format!("{}/mcp", server.base),
                &headers,
                auto_consent,
                0,
                None,
            )
            .await
            .expect("login with probe headers should complete");
    }

    #[tokio::test]
    async fn login_still_completes_when_the_browser_cannot_open() {
        // The opener reports failure (headless/SSH), but the user "pastes" the
        // link: the callback is still driven, so login succeeds via the
        // print-the-URL path.
        fn failing_opener(authorize_url: &str) -> bool {
            auto_consent(authorize_url);
            false
        }
        let server = mock_auth_server("default").await;
        OAuthClient::new()
            .login(
                &format!("{}/mcp", server.base),
                &HashMap::new(),
                failing_opener,
                0,
                None,
            )
            .await
            .expect("login should complete even without a browser");
    }

    #[tokio::test]
    async fn login_uses_default_scopes_when_the_server_advertises_none() {
        let server = mock_auth_server("no_scopes").await;
        OAuthClient::new()
            .login(
                &format!("{}/mcp", server.base),
                &HashMap::new(),
                auto_consent,
                0,
                None,
            )
            .await
            .expect("login should complete with default scopes");
    }

    #[tokio::test]
    async fn login_falls_back_to_the_mcp_url_when_resource_is_omitted() {
        let server = mock_auth_server("no_resource").await;
        let auth = OAuthClient::new()
            .login(
                &format!("{}/mcp", server.base),
                &HashMap::new(),
                auto_consent,
                0,
                None,
            )
            .await
            .expect("login should complete");
        // The resource identifier defaulted to the MCP URL itself.
        assert_eq!(auth.resource, format!("{}/mcp", server.base));
    }

    #[tokio::test]
    async fn login_fails_when_registration_is_rejected() {
        let server = mock_auth_server("register_fails").await;
        let err = OAuthClient::new()
            .login(
                &format!("{}/mcp", server.base),
                &HashMap::new(),
                auto_consent,
                0,
                None,
            )
            .await
            .expect_err("a rejected registration must fail");
        assert!(
            err.to_string().contains("registration failed"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn discovery_fails_when_no_metadata_document_is_reachable() {
        // Both the RFC 8414 and OpenID endpoints 404.
        let server = mock_auth_server("no_metadata").await;
        let err = OAuthClient::new()
            .login(
                &format!("{}/mcp", server.base),
                &HashMap::new(),
                auto_consent,
                0,
                None,
            )
            .await
            .expect_err("no reachable metadata must fail");
        assert!(
            err.to_string().contains("authorization server metadata"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn discovery_fails_when_resource_metadata_is_unavailable() {
        let server = mock_auth_server("discover_fails").await;
        let err = OAuthClient::new()
            .login(
                &format!("{}/mcp", server.base),
                &HashMap::new(),
                auto_consent,
                0,
                None,
            )
            .await
            .expect_err("a 500 on resource metadata must fail");
        assert!(err.to_string().contains("resource metadata"), "got: {err}");
    }

    #[tokio::test]
    async fn login_fails_when_registration_returns_bad_json() {
        let server = mock_auth_server("register_bad_json").await;
        let err = OAuthClient::new()
            .login(
                &format!("{}/mcp", server.base),
                &HashMap::new(),
                auto_consent,
                0,
                None,
            )
            .await
            .expect_err("unparseable registration must fail");
        assert!(
            err.to_string().contains("registration response"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn login_fails_when_the_token_exchange_is_rejected() {
        let server = mock_auth_server("exchange_fails").await;
        let err = OAuthClient::new()
            .login(
                &format!("{}/mcp", server.base),
                &HashMap::new(),
                auto_consent,
                0,
                None,
            )
            .await
            .expect_err("a rejected code exchange must fail");
        assert!(
            err.to_string().contains("token exchange failed"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn login_fails_when_the_token_response_is_not_json() {
        let server = mock_auth_server("bad_token_json").await;
        let err = OAuthClient::new()
            .login(
                &format!("{}/mcp", server.base),
                &HashMap::new(),
                auto_consent,
                0,
                None,
            )
            .await
            .expect_err("an unparseable token response must fail");
        assert!(
            err.to_string().contains("parse token response"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn login_fails_when_the_authorize_endpoint_is_malformed() {
        let server = mock_auth_server("bad_authorize").await;
        let err = OAuthClient::new()
            .login(
                &format!("{}/mcp", server.base),
                &HashMap::new(),
                auto_consent,
                0,
                None,
            )
            .await
            .expect_err("a bad authorize endpoint must fail");
        assert!(
            err.to_string().contains("authorization endpoint"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn login_fails_when_no_authorization_server_is_named() {
        let server = mock_auth_server("no_auth_server").await;
        let err = OAuthClient::new()
            .login(
                &format!("{}/mcp", server.base),
                &HashMap::new(),
                auto_consent,
                0,
                None,
            )
            .await
            .expect_err("empty authorization_servers must fail");
        assert!(
            err.to_string().contains("no authorization server"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn login_fails_when_the_issuer_is_malformed() {
        let server = mock_auth_server("bad_issuer").await;
        let err = OAuthClient::new()
            .login(
                &format!("{}/mcp", server.base),
                &HashMap::new(),
                auto_consent,
                0,
                None,
            )
            .await
            .expect_err("a bad issuer must fail");
        assert!(err.to_string().contains("issuer"), "got: {err}");
    }

    #[tokio::test]
    async fn login_fails_when_the_callback_is_forged() {
        // The "browser" returns a mismatched state, so wait_for_callback
        // rejects it and login propagates the failure.
        fn forge(authorize_url: &str) -> bool {
            drive_callback(authorize_url, Some("WRONG"));
            true
        }
        let server = mock_auth_server("default").await;
        let err = OAuthClient::new()
            .login(
                &format!("{}/mcp", server.base),
                &HashMap::new(),
                forge,
                0,
                None,
            )
            .await
            .expect_err("a forged callback must fail login");
        assert!(err.to_string().contains("state mismatch"), "got: {err}");
    }

    // ─── private HTTP helpers, driven directly ────────────────────────────
    //
    // Their network-error arms only fire on a failed request, which the
    // happy-path flow never produces. Calling them against a dead port or a
    // bad-body server exercises those arms deterministically.

    #[tokio::test]
    async fn get_json_errors_on_a_dead_connection() {
        let err = OAuthClient::new()
            .get_json("http://127.0.0.1:1/x")
            .await
            .expect_err("a refused connection must fail");
        let _ = err;
    }

    #[tokio::test]
    async fn get_json_errors_on_a_non_success_status() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route("/x", get(|| async { StatusCode::NOT_FOUND }));
        tokio::spawn(std::future::IntoFuture::into_future(axum::serve(
            listener, app,
        )));
        let err = OAuthClient::new()
            .get_json(&format!("{base}/x"))
            .await
            .expect_err("404 must fail");
        assert!(err.to_string().contains("404"), "got: {err}");
    }

    #[tokio::test]
    async fn get_json_errors_on_an_unparseable_body() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route("/x", get(|| async { "not json" }));
        tokio::spawn(std::future::IntoFuture::into_future(axum::serve(
            listener, app,
        )));
        assert!(
            OAuthClient::new()
                .get_json(&format!("{base}/x"))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn post_form_errors_on_a_dead_connection() {
        assert!(
            OAuthClient::new()
                .post_form("http://127.0.0.1:1/token", &[("a", "b")])
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn register_errors_on_a_dead_connection() {
        let meta: AuthServerMetadata = serde_json::from_value(serde_json::json!({
            "issuer": "https://x",
            "authorization_endpoint": "https://x/a",
            "token_endpoint": "https://x/t",
            "registration_endpoint": "http://127.0.0.1:1/register",
        }))
        .unwrap();
        let err = OAuthClient::new()
            .register(&meta, "http://127.0.0.1:5000/callback")
            .await
            .expect_err("a dead registration endpoint must fail");
        assert!(
            err.to_string().contains("registration request failed"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn probe_challenge_of_a_dead_server_is_none() {
        let mcp = Url::parse("http://127.0.0.1:1/mcp").unwrap();
        assert!(
            OAuthClient::new()
                .probe_challenge(&mcp, &HashMap::new())
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn login_fails_when_resource_metadata_is_not_an_object() {
        let server = mock_auth_server("resource_not_object").await;
        let err = OAuthClient::new()
            .login(
                &format!("{}/mcp", server.base),
                &HashMap::new(),
                auto_consent,
                0,
                None,
            )
            .await
            .expect_err("malformed resource metadata must fail");
        assert!(
            err.to_string().contains("parse resource metadata"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn login_fails_when_the_token_lacks_an_access_token() {
        let server = mock_auth_server("token_no_access").await;
        let err = OAuthClient::new()
            .login(
                &format!("{}/mcp", server.base),
                &HashMap::new(),
                auto_consent,
                0,
                None,
            )
            .await
            .expect_err("a token without access_token must fail");
        assert!(
            err.to_string().contains("parse token response"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn refresh_fails_when_the_token_lacks_an_access_token() {
        let server = mock_auth_server("token_no_access").await;
        let auth = ServerAuth {
            token_endpoint: format!("{}/token", server.base),
            refresh_token: Some("good".to_string()),
            ..Default::default()
        };
        let err = OAuthClient::new()
            .refresh(&auth, 0)
            .await
            .expect_err("a malformed refresh token response must fail");
        assert!(
            err.to_string().contains("parse token response"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn login_rejects_a_bad_mcp_url() {
        let err = OAuthClient::new()
            .login("not a url", &HashMap::new(), auto_consent, 0, None)
            .await
            .expect_err("bad url must fail");
        assert!(
            err.to_string().contains("Invalid MCP server url"),
            "got: {err}"
        );
    }
}
