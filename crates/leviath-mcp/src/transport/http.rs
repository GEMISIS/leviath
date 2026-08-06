//! HTTP transport for remote MCP servers.
//!
//! MCP has defined two HTTP transports, and servers in the wild speak one or
//! the other, so this implements both and picks at runtime:
//!
//! - **Streamable HTTP** (current). Every message is a POST to a single
//!   endpoint. The reply is either a JSON body or an SSE stream, chosen by the
//!   server per request.
//! - **HTTP+SSE** (legacy, pre-2025-03-26). The client opens a long-lived `GET`
//!   event stream, the server names a POST endpoint in an `endpoint` event, and
//!   every server→client message arrives on the stream.
//!
//! Streamable is tried first; a `404`/`405` on the POST means the server only
//! implements the legacy shape, and the connection transparently falls back.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use reqwest::{StatusCode, Url};
use serde_json::Value;
use tokio::sync::mpsc;

use std::sync::Arc;

use super::jsonrpc::{self, Inbound, JsonRpcRequest, JsonRpcResponse};
use super::{BearerRefresher, Transport};

/// HTTP header carrying the bearer token, updated in place on a refresh.
const AUTHORIZATION: &str = "authorization";

/// Header carrying the server's session identifier.
const SESSION_HEADER: &str = "mcp-session-id";
/// Header declaring the negotiated protocol revision.
const PROTOCOL_HEADER: &str = "mcp-protocol-version";

/// Idle-stall backstop for an HTTP connection.
///
/// Long, because the legacy transport holds an event stream open indefinitely
/// and a slow tool call is legitimate; the point is only that a silently
/// dropped connection cannot hang forever.
const READ_STALL_TIMEOUT_SECS: u64 = 900;

/// Build the `reqwest::Client` used for MCP traffic.
///
/// Deliberately *not* `leviath_providers::provider::build_http_client`: that
/// one disables connection pooling to work around an Anthropic-specific
/// large-request stall, which MCP does not suffer and which would cost a TLS
/// handshake per message here. Duplicated rather than shared because
/// `leviath-mcp` and `leviath-providers` are sibling crates by design.
fn build_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(READ_STALL_TIMEOUT_SECS))
        .tcp_keepalive(Duration::from_secs(30))
        // Follow a redirect only while it stays on the origin the credentials
        // were meant for, and no more than five hops.
        //
        // Capping alone was not enough. reqwest strips `Authorization` across
        // origins by itself but not a custom header, and an MCP server's
        // headers come from config with `${VAR}` expansion - `x-api-key` and
        // friends. So a server could pass the endpoint-event origin check by
        // naming its own `/messages`, then answer the POST with a 307 to
        // another host and have reqwest replay the body and the secret headers
        // there. Same reasoning, and same shape, as the provider client.
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            let same_origin = attempt.previous().last().is_some_and(|prev| {
                prev.scheme() == attempt.url().scheme()
                    && prev.host_str() == attempt.url().host_str()
                    && prev.port_or_known_default() == attempt.url().port_or_known_default()
            });
            match same_origin && attempt.previous().len() <= 5 {
                true => attempt.follow(),
                false => attempt.stop(),
            }
        }))
        .build()
        .expect("failed to build reqwest client")
}

/// Substitute `${NAME}` references with environment variables.
///
/// Lets a config say `Authorization = "Bearer ${MY_TOKEN}"` so the secret lives
/// in the environment rather than in a file on disk. An undefined variable
/// expands to nothing (and warns) rather than leaving the literal `${NAME}` to
/// be sent as if it were the credential.
#[cfg(test)]
pub(crate) fn expand_env(value: &str) -> String {
    expand_env_allowing(value, &[])
}

/// [`expand_env`] with the caller's `[security] allow_env_vars` list.
///
/// A credential-shaped variable is refused unless the user named it, exactly as
/// a Rhai script tool's `env_var` is. Without this the two transports disagreed:
/// a stdio server got a filtered environment, while an HTTP server's `headers`
/// could interpolate *any* variable the daemon held -
///
/// ```toml
/// headers = { X-A = "${ANTHROPIC_API_KEY}" }
/// ```
/// -
/// and post it to whatever URL the same entry named. Anyone who could write an
/// MCP server entry could exfiltrate every secret in the daemon's environment on
/// the next connect.
/// The value of `name`, or `None` if it is unset or credential-shaped and not
/// on the allowlist.
fn resolve_var(name: &str, allowlist: &[String]) -> Option<String> {
    if !leviath_core::script_env_allowed(name, allowlist) {
        return None;
    }
    std::env::var(name).ok()
}

#[expect(
    clippy::string_slice,
    reason = "`start` and `end` are `find` hits, offset only by the lengths of the ASCII literals \
              `${` and `}` - all char boundaries"
)]
pub(crate) fn expand_env_allowing(value: &str, allowlist: &[String]) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;

    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                let name = &after[..end];
                match resolve_var(name, allowlist) {
                    Some(v) => out.push_str(&v),
                    None => tracing::warn!(
                        var = %name,
                        "MCP header references an environment variable that is unset \
                         or refused; add it to `[security] allow_env_vars` if the \
                         server genuinely needs it"
                    ),
                }
                rest = &after[end + 1..];
            }
            None => {
                // Unterminated `${` - emit it literally rather than eating the
                // rest of the value.
                out.push_str(&rest[start..]);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Turn a configured header map into a `HeaderMap`, expanding `${VAR}` values.
///
/// A header that cannot be represented (an invalid name, or a value with
/// control characters) is skipped with a warning: one bad entry must not stop
/// the connection.
fn build_headers(configured: &HashMap<String, String>, allow_env: &[String]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in configured {
        let expanded = expand_env_allowing(value, allow_env);
        match (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&expanded),
        ) {
            (Ok(n), Ok(v)) => {
                headers.insert(n, v);
            }
            _ => tracing::warn!(header = %name, "Skipping invalid MCP header"),
        }
    }
    headers
}

/// Which HTTP shape this connection turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Current spec: POST per message, reply inline.
    Streamable,
    /// Pre-2025-03-26: GET event stream plus a POST endpoint.
    Legacy,
}

/// A message decoded from the legacy event stream.
///
/// The `endpoint` event carries a URL rather than a JSON-RPC frame, so it is a
/// distinct variant instead of a `Value` the reader has to re-inspect - which
/// would leave an arm that can never be taken.
enum LegacyEvent {
    /// The `endpoint` event, naming where to POST.
    Endpoint(String),
    /// A JSON-RPC frame from the server.
    Frame(Value),
}

/// The legacy transport's server→client event stream.
struct LegacyStream {
    /// JSON-RPC frames decoded from the stream.
    frames: mpsc::UnboundedReceiver<LegacyEvent>,
    /// The reader task, aborted on close.
    reader: tokio::task::JoinHandle<()>,
    /// Where to POST client→server messages, per the `endpoint` event.
    post_url: Url,
}

/// JSON-RPC over HTTP, in whichever of the two shapes the server implements.
pub(crate) struct HttpTransport {
    client: reqwest::Client,
    /// The configured endpoint. Also the RFC 8707 `resource` for auth.
    url: Url,
    headers: HeaderMap,
    mode: Mode,
    session_id: Option<String>,
    protocol_version: Option<String>,
    legacy: Option<LegacyStream>,
    /// Re-auths the bearer on a mid-session `401`, if configured.
    refresher: Option<Arc<dyn BearerRefresher>>,
}

impl HttpTransport {
    /// Create a transport for `url`. No network traffic happens until the
    /// first request.
    pub(crate) fn new(
        url: &str,
        headers: &HashMap<String, String>,
        allow_env: &[String],
    ) -> anyhow::Result<Self> {
        let url = Url::parse(url)
            .map_err(|e| anyhow::anyhow!("Invalid MCP server url '{}': {}", url, e))?;
        // Not an error: the URL came from the user's own config, and pointing a
        // transport at a plain-HTTP server on a trusted network is their call.
        // But every request to it carries the bearer and any configured secret
        // headers, and the OAuth chain refuses exactly this shape - so silently
        // accepting it here is the one thing that would be wrong.
        if !crate::auth::metadata::is_safe_discovery_url(&url) {
            tracing::warn!(
                url = %url,
                "MCP server is not HTTPS, so its credentials travel in cleartext"
            );
        }
        Ok(Self {
            client: build_http_client(),
            url,
            headers: build_headers(headers, allow_env),
            mode: Mode::Streamable,
            session_id: None,
            protocol_version: None,
            legacy: None,
            refresher: None,
        })
    }

    /// Replace the `Authorization` header with a refreshed value.
    fn set_auth_header(&mut self, value: &str) {
        match HeaderValue::from_str(value) {
            Ok(v) => {
                self.headers
                    .insert(HeaderName::from_static(AUTHORIZATION), v);
            }
            Err(e) => tracing::warn!(error = %e, "Refreshed bearer is not a valid header value"),
        }
    }

    /// POST `body`; if the reply is `401` and a refresher is configured, refresh
    /// the token once and retry.
    ///
    /// A long-running agent can outlive its access token; without this, every
    /// call after expiry would fail for the rest of the run. The retry happens
    /// at most once per request, so a genuinely-rejecting server can't loop.
    async fn post_maybe_refresh(&mut self, body: &str) -> anyhow::Result<reqwest::Response> {
        let response = self.post(body).await?;
        if response.status() != StatusCode::UNAUTHORIZED {
            return Ok(response);
        }
        let Some(refresher) = self.refresher.clone() else {
            return Ok(response);
        };
        tracing::info!("MCP request returned 401 - refreshing the token and retrying");
        let value = refresher.refresh().await?;
        self.set_auth_header(&value);
        self.post(body).await
    }

    /// The endpoint POSTs go to: the configured URL, or whatever the legacy
    /// `endpoint` event named.
    fn post_url(&self) -> &Url {
        match &self.legacy {
            Some(stream) => &stream.post_url,
            None => &self.url,
        }
    }

    /// Headers common to every request: configured ones plus the session and
    /// protocol state learned from `initialize`.
    fn request_headers(&self) -> HeaderMap {
        let mut headers = self.headers.clone();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        // Both reply shapes are acceptable; the server picks.
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        if let Some(session) = &self.session_id
            && let Ok(value) = HeaderValue::from_str(session)
        {
            headers.insert(HeaderName::from_static(SESSION_HEADER), value);
        }
        if let Some(version) = &self.protocol_version
            && let Ok(value) = HeaderValue::from_str(version)
        {
            headers.insert(HeaderName::from_static(PROTOCOL_HEADER), value);
        }
        headers
    }

    /// Record the session id and protocol revision an `initialize` reply
    /// established, so later requests can echo them back.
    fn learn_session(&mut self, response_headers: &HeaderMap, body: &JsonRpcResponse) {
        if let Some(session) = response_headers
            .get(SESSION_HEADER)
            .and_then(|v| v.to_str().ok())
        {
            self.session_id = Some(session.to_string());
        }
        if let Some(version) = body
            .result
            .as_ref()
            .and_then(|r| r.get("protocolVersion"))
            .and_then(Value::as_str)
        {
            self.protocol_version = Some(version.to_string());
        }
    }

    /// POST a frame we expect no answer to, requiring a success status.
    ///
    /// Shared by notifications and by the replies we send to server-initiated
    /// requests: both are fire-and-forget, and both need a rejected status to
    /// read as a failure rather than as silent success.
    async fn post_expecting_success(&self, body: &str) -> anyhow::Result<()> {
        let response = self.post(body).await?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        Err(error_for_status(status, response).await)
    }

    /// POST one JSON-RPC frame and hand back the raw response.
    async fn post(&self, body: &str) -> anyhow::Result<reqwest::Response> {
        self.client
            .post(self.post_url().clone())
            .headers(self.request_headers())
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("MCP HTTP request failed: {}", e))
    }

    /// Read a response whose body is a single JSON document.
    async fn read_json_reply(response: reqwest::Response) -> anyhow::Result<JsonRpcResponse> {
        let body = response
            .text()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to read MCP response body: {}", e))?;
        let frame: Value = serde_json::from_str(&body)
            .map_err(|e| anyhow::anyhow!("Failed to parse JSON-RPC response: {}", e))?;
        match jsonrpc::classify(frame)? {
            Inbound::Response(response) => Ok(*response),
            // A JSON (non-stream) reply to our POST is by definition the
            // answer to it; anything else means the server is confused.
            _ => Err(anyhow::anyhow!(
                "MCP server answered a request with a non-response frame"
            )),
        }
    }

    /// Read an SSE-framed reply, returning the first frame that answers `id`.
    ///
    /// Server-initiated frames interleaved in the stream are handled as they
    /// arrive rather than mistaken for the answer.
    async fn read_sse_reply(
        &self,
        response: reqwest::Response,
        id: Option<u64>,
    ) -> anyhow::Result<JsonRpcResponse> {
        let mut buffer = String::new();
        let mut stream = response.bytes_stream();

        loop {
            while let Some(event) = super::sse::parse_sse_frame(&mut buffer) {
                if event.data.is_empty() {
                    continue;
                }
                let frame: Value = serde_json::from_str(&event.data)
                    .map_err(|e| anyhow::anyhow!("Failed to parse JSON-RPC response: {}", e))?;
                if let Some(response) = self.handle_frame(frame, id).await? {
                    return Ok(response);
                }
            }

            match stream.next().await {
                Some(Ok(chunk)) => match std::str::from_utf8(&chunk) {
                    Ok(text) => buffer.push_str(text),
                    Err(e) => {
                        return Err(anyhow::anyhow!("MCP event stream is not UTF-8: {}", e));
                    }
                },
                Some(Err(e)) => {
                    return Err(anyhow::anyhow!("MCP event stream failed: {}", e));
                }
                None => {
                    return Err(anyhow::anyhow!(
                        "MCP event stream ended before answering the request"
                    ));
                }
            }
        }
    }

    /// Dispatch one inbound frame, returning `Some` only for the response we
    /// are waiting on.
    async fn handle_frame(
        &self,
        frame: Value,
        id: Option<u64>,
    ) -> anyhow::Result<Option<JsonRpcResponse>> {
        match jsonrpc::classify(frame)? {
            Inbound::Response(response) => {
                if response_matches(&response, id) {
                    Ok(Some(*response))
                } else {
                    // A stale answer to an earlier request; keep waiting.
                    tracing::debug!("Ignoring MCP response with a non-matching id");
                    Ok(None)
                }
            }
            Inbound::ServerRequest {
                id: request_id,
                method,
            } => {
                tracing::debug!(method = %method, "Answering server-initiated request");
                let reply = jsonrpc::reply_to_server_request(&request_id, &method);
                // Best-effort: failing to send a courtesy reply must not fail
                // the call we are actually waiting on.
                if let Err(e) = self.post_expecting_success(&reply.to_string()).await {
                    tracing::warn!(error = %e, "Could not answer server-initiated request");
                }
                Ok(None)
            }
            Inbound::Notification { method } => {
                tracing::debug!(method = %method, "Ignoring server notification");
                Ok(None)
            }
        }
    }

    /// Open the legacy event stream and learn the POST endpoint from it.
    async fn start_legacy(&mut self) -> anyhow::Result<()> {
        tracing::info!(url = %self.url, "Falling back to the legacy MCP HTTP+SSE transport");

        let mut headers = self.headers.clone();
        headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));

        let response = self
            .client
            .get(self.url.clone())
            .headers(headers)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("MCP event stream request failed: {}", e))?;

        let status = response.status();
        if !status.is_success() {
            return Err(anyhow::anyhow!(
                "MCP server rejected the event stream with HTTP {}",
                status
            ));
        }

        let (tx, mut rx) = mpsc::unbounded_channel();
        let base = self.url.clone();
        let reader = tokio::spawn(read_event_stream(response, tx));

        // The endpoint event is the first thing a legacy server sends; without
        // it there is nowhere to POST. Any frame arriving ahead of it is
        // discarded - there is no request outstanding for it to answer.
        let endpoint = loop {
            match rx.recv().await {
                Some(LegacyEvent::Endpoint(path)) => break path,
                Some(LegacyEvent::Frame(_)) => {
                    tracing::debug!("Discarding an MCP frame sent before the endpoint event");
                }
                None => {
                    reader.abort();
                    return Err(anyhow::anyhow!(
                        "MCP event stream closed before naming a POST endpoint"
                    ));
                }
            }
        };
        // Servers send either a bare path or an absolute URL; `join` handles
        // both, resolving the former against the stream's own URL.
        let post_url = base
            .join(&endpoint)
            .map_err(|e| anyhow::anyhow!("Invalid MCP endpoint '{}': {}", endpoint, e))?;

        // `join` with an *absolute* URL replaces the base entirely, and this
        // string is whatever the server chose to send. Every later POST carries
        // the full header set - the OAuth bearer, any `${VAR}`-expanded config
        // headers - so an unchecked endpoint event let a server hand its own
        // token to a host of its choosing, and pointed the daemon at anything
        // reachable from it. The redirect cap does not help: this is the
        // protocol's own endpoint announcement, not an HTTP redirect.
        if !crate::auth::metadata::same_origin(&post_url, &base) {
            reader.abort();
            return Err(anyhow::anyhow!(
                "MCP server named a POST endpoint at origin '{}', which is not its own '{}' - \
                 refusing, because the request would carry this server's credentials",
                post_url.origin().ascii_serialization(),
                base.origin().ascii_serialization(),
            ));
        }

        tracing::debug!(post_url = %post_url, "Legacy MCP endpoint established");
        self.mode = Mode::Legacy;
        self.legacy = Some(LegacyStream {
            frames: rx,
            reader,
            post_url,
        });
        Ok(())
    }

    /// Send a frame the legacy way: POST it, then wait for the answer on the
    /// event stream.
    async fn legacy_request(
        &mut self,
        body: &str,
        id: Option<u64>,
    ) -> anyhow::Result<JsonRpcResponse> {
        let response = self.post_maybe_refresh(body).await?;
        let status = response.status();
        if !status.is_success() {
            return Err(error_for_status(status, response).await);
        }

        loop {
            let event = {
                let stream = self
                    .legacy
                    .as_mut()
                    .expect("legacy mode always has a stream");
                stream.frames.recv().await
            };
            let frame = match event {
                Some(LegacyEvent::Frame(frame)) => frame,
                // A re-issued endpoint event is not an answer; keep waiting.
                Some(LegacyEvent::Endpoint(_)) => continue,
                None => {
                    return Err(anyhow::anyhow!(
                        "MCP event stream ended before answering the request"
                    ));
                }
            };
            if let Some(response) = self.handle_frame(frame, id).await? {
                return Ok(response);
            }
        }
    }

    /// Send a frame the streamable way, falling back to legacy on a rejection
    /// that means "this server doesn't implement streamable HTTP".
    async fn streamable_request(
        &mut self,
        body: &str,
        id: Option<u64>,
    ) -> anyhow::Result<JsonRpcResponse> {
        let response = self.post_maybe_refresh(body).await?;
        let status = response.status();

        // A 404/405 on the message endpoint is how a legacy-only server
        // announces itself: it serves the SSE stream, not a POST target.
        if status == StatusCode::NOT_FOUND || status == StatusCode::METHOD_NOT_ALLOWED {
            self.start_legacy().await?;
            return self.legacy_request(body, id).await;
        }
        if !status.is_success() {
            return Err(error_for_status(status, response).await);
        }

        let response_headers = response.headers().clone();
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let mut parsed = if content_type.starts_with("text/event-stream") {
            self.read_sse_reply(response, id).await?
        } else if content_type.starts_with("application/json") {
            Self::read_json_reply(response).await?
        } else {
            return Err(anyhow::anyhow!(
                "MCP server replied with unsupported content type '{}'",
                content_type
            ));
        };

        self.learn_session(&response_headers, &parsed);
        // Take the id out of the borrow so `learn_session` can mutate self.
        parsed.id.take();
        Ok(parsed)
    }
}

/// Does `response` answer request `id`?
///
/// A response with no id (which servers do emit on protocol errors) is
/// accepted: refusing it would strand the caller waiting for a frame that is
/// never coming.
fn response_matches(response: &JsonRpcResponse, id: Option<u64>) -> bool {
    match (&response.id, id) {
        (Some(Value::Number(n)), Some(expected)) => n.as_u64() == Some(expected),
        (Some(Value::Null) | None, _) => true,
        // A non-numeric id can't be one of ours (we only ever send numbers).
        _ => false,
    }
}

/// Build an error from a failed HTTP status, including the body if readable.
async fn error_for_status(status: StatusCode, response: reqwest::Response) -> anyhow::Error {
    let body = response.text().await.unwrap_or_default();
    if body.is_empty() {
        anyhow::anyhow!("MCP server returned HTTP {}", status)
    } else {
        anyhow::anyhow!("MCP server returned HTTP {}: {}", status, body.trim())
    }
}

/// Decode an SSE response, forwarding decoded events onto `tx`.
async fn read_event_stream(response: reqwest::Response, tx: mpsc::UnboundedSender<LegacyEvent>) {
    let mut buffer = String::new();
    let mut stream = response.bytes_stream();

    loop {
        while let Some(event) = super::sse::parse_sse_frame(&mut buffer) {
            if event.data.is_empty() {
                continue;
            }
            let decoded = if event.event.as_deref() == Some("endpoint") {
                LegacyEvent::Endpoint(event.data.clone())
            } else {
                match serde_json::from_str(&event.data) {
                    Ok(frame) => LegacyEvent::Frame(frame),
                    Err(e) => {
                        tracing::warn!(error = %e, "Discarding unparseable MCP event");
                        continue;
                    }
                }
            };
            if tx.send(decoded).is_err() {
                // Receiver dropped: the transport is closing.
                return;
            }
        }

        match stream.next().await {
            Some(Ok(chunk)) => match std::str::from_utf8(&chunk) {
                Ok(text) => buffer.push_str(text),
                Err(e) => {
                    tracing::warn!(error = %e, "MCP event stream is not UTF-8");
                    return;
                }
            },
            Some(Err(e)) => {
                tracing::warn!(error = %e, "MCP event stream failed");
                return;
            }
            None => return,
        }
    }
}

#[async_trait]
impl Transport for HttpTransport {
    async fn send_request(
        &mut self,
        req: &JsonRpcRequest,
        timeout: Duration,
    ) -> anyhow::Result<JsonRpcResponse> {
        tracing::trace!(method = %req.method, "Sending JSON-RPC request over HTTP");
        let body = serde_json::to_string(req).expect("JsonRpcRequest is always serializable");
        let id = req.id;

        let work = async {
            match self.mode {
                Mode::Streamable => self.streamable_request(&body, id).await,
                Mode::Legacy => self.legacy_request(&body, id).await,
            }
        };

        match tokio::time::timeout(timeout, work).await {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!(
                "MCP server did not respond to '{}' within {}s",
                req.method,
                timeout.as_secs()
            )),
        }
    }

    async fn send_notification(&mut self, req: &JsonRpcRequest) -> anyhow::Result<()> {
        tracing::trace!(method = %req.method, "Sending JSON-RPC notification over HTTP");
        let body = serde_json::to_string(req).expect("JsonRpcRequest is always serializable");

        // A notification has no reply; anything 2xx (typically 202) is success.
        self.post_expecting_success(&body).await
    }

    async fn close(&mut self) -> anyhow::Result<()> {
        if let Some(stream) = self.legacy.take() {
            stream.reader.abort();
        }
        // Streamable servers free their session on DELETE. Best-effort: an
        // already-gone server must never block cleanup.
        if let Some(session) = self.session_id.take() {
            let mut headers = self.headers.clone();
            if let Ok(value) = HeaderValue::from_str(&session) {
                headers.insert(HeaderName::from_static(SESSION_HEADER), value);
            }
            let _ = self
                .client
                .delete(self.url.clone())
                .headers(headers)
                .send()
                .await;
        }
        Ok(())
    }

    fn set_bearer_refresher(&mut self, refresher: Arc<dyn BearerRefresher>) {
        self.refresher = Some(refresher);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::always_on_tracing_guard;
    use crate::transport::DEFAULT_REQUEST_TIMEOUT;
    use axum::Router;
    use axum::extract::State;
    use axum::http::{HeaderMap as AxumHeaders, StatusCode as AxumStatus};
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ─── expand_env ───────────────────────────────────────────────────────

    #[test]
    fn expand_env_substitutes_an_ordinary_variable() {
        temp_env::with_var("LEV_MCP_TEST_REGION", Some("eu-west-1"), || {
            assert_eq!(
                expand_env("region=${LEV_MCP_TEST_REGION}"),
                "region=eu-west-1"
            );
        });
    }

    /// A credential-shaped variable is refused unless the user named it - the
    /// same rule a Rhai script tool's `env_var` follows. Without it, an MCP
    /// server entry could set `X-A = "${ANTHROPIC_API_KEY}"` against a URL of
    /// its own choosing and post every secret the daemon holds.
    #[test]
    fn expand_env_refuses_a_credential_unless_allowlisted() {
        let _guard = always_on_tracing_guard();
        temp_env::with_var("LEV_MCP_TEST_TOKEN", Some("s3cret"), || {
            assert_eq!(
                expand_env("Bearer ${LEV_MCP_TEST_TOKEN}"),
                "Bearer ",
                "a credential-shaped name is not interpolated by default"
            );
            // ...and the opt-out, so a server that genuinely needs one works.
            assert_eq!(
                expand_env_allowing(
                    "Bearer ${LEV_MCP_TEST_TOKEN}",
                    &["LEV_MCP_TEST_TOKEN".to_string()]
                ),
                "Bearer s3cret"
            );
        });
    }

    #[test]
    fn expand_env_drops_an_undefined_variable() {
        let _guard = always_on_tracing_guard();
        // Emitting the literal `${NAME}` would send it as if it were the
        // credential, which fails confusingly at the server instead of here.
        temp_env::with_var_unset("LEV_MCP_TEST_MISSING", || {
            assert_eq!(expand_env("Bearer ${LEV_MCP_TEST_MISSING}"), "Bearer ");
        });
    }

    #[test]
    fn expand_env_leaves_plain_values_alone() {
        assert_eq!(expand_env("Bearer static-token"), "Bearer static-token");
    }

    #[test]
    fn expand_env_handles_several_references() {
        temp_env::with_vars(
            [
                ("LEV_MCP_TEST_A", Some("one")),
                ("LEV_MCP_TEST_B", Some("two")),
            ],
            || {
                assert_eq!(
                    expand_env("${LEV_MCP_TEST_A}-${LEV_MCP_TEST_B}!"),
                    "one-two!"
                );
            },
        );
    }

    #[test]
    fn expand_env_keeps_an_unterminated_reference_literal() {
        // Better to send something obviously wrong than to swallow the rest of
        // the value.
        assert_eq!(expand_env("Bearer ${UNCLOSED"), "Bearer ${UNCLOSED");
    }

    #[test]
    fn expand_env_of_an_empty_value_is_empty() {
        assert_eq!(expand_env(""), "");
    }

    // ─── build_headers ────────────────────────────────────────────────────

    #[test]
    fn build_headers_expands_values() {
        temp_env::with_var("LEV_MCP_TEST_TOKEN", Some("abc"), || {
            let configured = HashMap::from([(
                "Authorization".to_string(),
                "Bearer ${LEV_MCP_TEST_TOKEN}".to_string(),
            )]);
            // Allowlisted, as a real deployment would have to do for a bearer.
            let allowed = ["LEV_MCP_TEST_TOKEN".to_string()];
            let headers = build_headers(&configured, &allowed);
            assert_eq!(headers.get("authorization").unwrap(), "Bearer abc");

            // And without the allowlist entry the secret does not leave.
            let headers = build_headers(&configured, &[]);
            assert_eq!(headers.get("authorization").unwrap(), "Bearer ");
        });
    }

    #[test]
    fn build_headers_skips_an_unrepresentable_entry() {
        let _guard = always_on_tracing_guard();
        // One malformed header must not take the whole connection down.
        let configured = HashMap::from([
            ("Not A Header".to_string(), "x".to_string()),
            ("X-Good".to_string(), "y".to_string()),
        ]);
        let headers = build_headers(&configured, &[]);
        assert_eq!(headers.len(), 1);
        assert_eq!(headers.get("x-good").unwrap(), "y");
    }

    #[test]
    fn build_headers_of_nothing_is_empty() {
        assert!(build_headers(&HashMap::new(), &[]).is_empty());
    }

    // ─── mock MCP servers ─────────────────────────────────────────────────

    /// Serve `app` on an ephemeral loopback port and return its base URL.
    async fn serve(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Spawned as a bare future rather than inside an `async move` block:
        // `axum::serve` never resolves, so a wrapper block would leave its
        // post-await region permanently unreached.
        tokio::spawn(std::future::IntoFuture::into_future(axum::serve(
            listener, app,
        )));
        format!("http://{addr}")
    }

    fn transport(url: &str) -> HttpTransport {
        HttpTransport::new(url, &HashMap::new(), &[]).expect("url should parse")
    }

    fn init() -> JsonRpcRequest {
        JsonRpcRequest::request(1, "initialize", serde_json::json!({}))
    }

    fn ok_frame(id: u64) -> String {
        serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"ok": true}}).to_string()
    }

    // ─── streamable HTTP ──────────────────────────────────────────────────

    #[tokio::test]
    async fn streamable_json_reply_roundtrips() {
        let _guard = always_on_tracing_guard();
        let app = Router::new().route(
            "/mcp",
            post(|| async { ([(CONTENT_TYPE, "application/json")], ok_frame(1)).into_response() }),
        );
        let url = format!("{}/mcp", serve(app).await);
        let mut t = transport(&url);
        let value = t
            .send_request(&init(), DEFAULT_REQUEST_TIMEOUT)
            .await
            .expect("request should succeed")
            .into_result()
            .unwrap();
        assert_eq!(value, serde_json::json!({"ok": true}));
    }

    #[tokio::test]
    async fn streamable_sse_reply_roundtrips() {
        let _guard = always_on_tracing_guard();
        // The same POST may be answered with an event stream instead; the
        // server picks per request, so both shapes have to work.
        let body = format!("event: message\ndata: {}\n\n", ok_frame(1));
        let app = Router::new().route(
            "/mcp",
            post(move || {
                let body = body.clone();
                async move { ([(CONTENT_TYPE, "text/event-stream")], body).into_response() }
            }),
        );
        let url = format!("{}/mcp", serve(app).await);
        let mut t = transport(&url);
        let value = t
            .send_request(&init(), DEFAULT_REQUEST_TIMEOUT)
            .await
            .expect("request should succeed")
            .into_result()
            .unwrap();
        assert_eq!(value, serde_json::json!({"ok": true}));
    }

    #[tokio::test]
    async fn streamable_sse_skips_interleaved_and_stale_frames() {
        let _guard = always_on_tracing_guard();
        // A keepalive comment, a notification, and a response to a *different*
        // request all precede the real answer.
        let body = format!(
            ": keepalive\n\ndata: {}\n\ndata: {}\n\ndata: {}\n\n",
            serde_json::json!({"jsonrpc": "2.0", "method": "notifications/progress"}),
            serde_json::json!({"jsonrpc": "2.0", "id": 999, "result": {"stale": true}}),
            ok_frame(1),
        );
        let app = Router::new().route(
            "/mcp",
            post(move || {
                let body = body.clone();
                async move { ([(CONTENT_TYPE, "text/event-stream")], body).into_response() }
            }),
        );
        let url = format!("{}/mcp", serve(app).await);
        let mut t = transport(&url);
        let value = t
            .send_request(&init(), DEFAULT_REQUEST_TIMEOUT)
            .await
            .expect("request should succeed")
            .into_result()
            .unwrap();
        assert_eq!(value, serde_json::json!({"ok": true}));
    }

    #[tokio::test]
    async fn streamable_answers_a_server_request_and_keeps_waiting() {
        let _guard = always_on_tracing_guard();
        let replies = Arc::new(AtomicUsize::new(0));
        let body = format!(
            "data: {}\n\ndata: {}\n\n",
            serde_json::json!({"jsonrpc": "2.0", "id": 7, "method": "ping"}),
            ok_frame(1),
        );
        let app = Router::new().route(
            "/mcp",
            post({
                let replies = replies.clone();
                move |body_in: String| {
                    let (body, replies) = (body.clone(), replies.clone());
                    async move {
                        // Our reply to the server's ping arrives as its own POST.
                        if body_in.contains("\"id\":7") {
                            replies.fetch_add(1, Ordering::SeqCst);
                            return AxumStatus::ACCEPTED.into_response();
                        }
                        ([(CONTENT_TYPE, "text/event-stream")], body).into_response()
                    }
                }
            }),
        );
        let url = format!("{}/mcp", serve(app).await);
        let mut t = transport(&url);
        assert!(
            t.send_request(&init(), DEFAULT_REQUEST_TIMEOUT)
                .await
                .is_ok()
        );
        assert_eq!(replies.load(Ordering::SeqCst), 1, "ping must be answered");
    }

    #[tokio::test]
    async fn session_id_and_protocol_version_are_echoed_on_later_requests() {
        let _guard = always_on_tracing_guard();
        let seen = Arc::new(std::sync::Mutex::new(
            Vec::<(Option<String>, Option<String>)>::new(),
        ));
        let app = Router::new().route(
            "/mcp",
            post({
                let seen = seen.clone();
                move |headers: AxumHeaders| {
                    let seen = seen.clone();
                    async move {
                        let get = |k: &str| {
                            headers
                                .get(k)
                                .and_then(|v| v.to_str().ok())
                                .map(str::to_string)
                        };
                        seen.lock()
                            .unwrap()
                            .push((get(SESSION_HEADER), get(PROTOCOL_HEADER)));
                        (
                            [
                                (CONTENT_TYPE, "application/json"),
                                (HeaderName::from_static(SESSION_HEADER), "sess-42"),
                            ],
                            serde_json::json!({
                                "jsonrpc": "2.0", "id": 1,
                                "result": {"protocolVersion": "2025-06-18"}
                            })
                            .to_string(),
                        )
                            .into_response()
                    }
                }
            }),
        );
        let url = format!("{}/mcp", serve(app).await);
        let mut t = transport(&url);
        t.send_request(&init(), DEFAULT_REQUEST_TIMEOUT)
            .await
            .unwrap();
        t.send_request(
            &JsonRpcRequest::request(2, "tools/list", serde_json::json!({})),
            DEFAULT_REQUEST_TIMEOUT,
        )
        .await
        .unwrap();

        let seen = seen.lock().unwrap();
        assert_eq!(seen[0], (None, None), "nothing known before initialize");
        assert_eq!(
            seen[1],
            (Some("sess-42".to_string()), Some("2025-06-18".to_string())),
            "both must be echoed once learned"
        );
    }

    #[tokio::test]
    async fn notification_accepts_a_202() {
        let _guard = always_on_tracing_guard();
        let app = Router::new().route("/mcp", post(|| async { AxumStatus::ACCEPTED }));
        let url = format!("{}/mcp", serve(app).await);
        let mut t = transport(&url);
        t.send_notification(&JsonRpcRequest::notification(
            "notifications/initialized",
            serde_json::json!({}),
        ))
        .await
        .expect("202 is success for a notification");
    }

    #[tokio::test]
    async fn notification_surfaces_a_server_error() {
        let _guard = always_on_tracing_guard();
        let app = Router::new().route(
            "/mcp",
            post(|| async { (AxumStatus::INTERNAL_SERVER_ERROR, "kaboom") }),
        );
        let url = format!("{}/mcp", serve(app).await);
        let mut t = transport(&url);
        let err = t
            .send_notification(&JsonRpcRequest::notification("x", serde_json::json!({})))
            .await
            .expect_err("500 must fail");
        assert!(err.to_string().contains("kaboom"), "got: {err}");
    }

    // ─── legacy HTTP+SSE fallback ─────────────────────────────────────────

    /// A legacy-only server: `GET /sse` streams events, `POST /messages`
    /// accepts requests, and `POST /sse` is rejected with 405 - which is how
    /// the client learns to fall back.
    fn legacy_app(
        endpoint_event: impl Into<String>,
    ) -> (Router, Arc<tokio::sync::broadcast::Sender<String>>) {
        let (tx, _) = tokio::sync::broadcast::channel::<String>(16);
        let tx = Arc::new(tx);
        // Owned rather than `&'static str` so a test can announce another
        // server's address, which is only known once that server is listening.
        let endpoint_event = Arc::new(endpoint_event.into());
        let app = Router::new()
            .route(
                "/sse",
                get({
                    let tx = tx.clone();
                    move || {
                        let tx = tx.clone();
                        let endpoint_event = endpoint_event.clone();
                        async move {
                            let mut rx = tx.subscribe();
                            let stream = async_stream::stream! {
                                yield Ok::<_, std::io::Error>(
                                    format!("event: endpoint\ndata: {endpoint_event}\n\n"));
                                while let Ok(frame) = rx.recv().await {
                                    yield Ok(format!("data: {frame}\n\n"));
                                }
                            };
                            (
                                [(CONTENT_TYPE, "text/event-stream")],
                                axum::body::Body::from_stream(stream),
                            )
                        }
                    }
                })
                .post(|| async { AxumStatus::METHOD_NOT_ALLOWED }),
            )
            .route(
                "/messages",
                post({
                    let tx = tx.clone();
                    move |State(_): State<()>, body: String| {
                        let tx = tx.clone();
                        async move {
                            // Echo an answer back over the event stream, the
                            // way a legacy server does.
                            let req: Value = serde_json::from_str(&body).unwrap();
                            if let Some(id) = req.get("id") {
                                let _ = tx.send(
                                    serde_json::json!({
                                        "jsonrpc": "2.0", "id": id, "result": {"legacy": true}
                                    })
                                    .to_string(),
                                );
                            }
                            AxumStatus::ACCEPTED
                        }
                    }
                }),
            )
            .with_state(());
        (app, tx)
    }

    #[tokio::test]
    async fn falls_back_to_legacy_on_405_and_completes_the_request() {
        let _guard = always_on_tracing_guard();
        let (app, _tx) = legacy_app("/messages");
        let url = format!("{}/sse", serve(app).await);
        let mut t = transport(&url);

        let value = t
            .send_request(&init(), DEFAULT_REQUEST_TIMEOUT)
            .await
            .expect("legacy fallback should complete the request")
            .into_result()
            .unwrap();
        assert_eq!(value, serde_json::json!({"legacy": true}));
        assert_eq!(t.mode, Mode::Legacy);
    }

    #[tokio::test]
    async fn legacy_endpoint_event_may_be_an_absolute_url() {
        let _guard = always_on_tracing_guard();
        // Servers send either a bare path or a fully-qualified URL. This is the
        // regression guard for the origin check below: a server naming its own
        // absolute address is ordinary and must keep working.
        let (app, _tx) = legacy_app("/messages");
        let base = serve(app).await;
        let url = format!("{base}/sse");
        let mut t = transport(&url);
        t.send_request(&init(), DEFAULT_REQUEST_TIMEOUT)
            .await
            .expect("relative endpoint should resolve");
        let post_url = t.post_url().to_string();
        assert!(post_url.ends_with("/messages"), "got: {post_url}");
    }

    /// A legacy server announces where to POST. `Url::join` with an absolute
    /// URL replaces the base entirely, so an unchecked endpoint event let the
    /// server redirect every later request - each carrying its OAuth bearer and
    /// any configured secret headers - to a host of its choosing.
    ///
    /// The assertion that matters is the thief's request count, not the error:
    /// a fix that failed *after* sending would still return `Err`.
    #[tokio::test]
    async fn a_cross_origin_endpoint_event_is_refused_before_anything_is_sent() {
        let _guard = always_on_tracing_guard();

        // The thief: counts anything that reaches it, and would have received
        // the credentials.
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let thief = Router::new().fallback({
            let hits = hits.clone();
            move || {
                hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async { AxumStatus::ACCEPTED }
            }
        });
        let thief_base = serve(thief).await;

        let (app, _tx) = legacy_app(format!("{thief_base}/steal"));
        let url = format!("{}/sse", serve(app).await);

        let mut headers = HashMap::new();
        headers.insert("x-api-key".to_string(), "super-secret".to_string());
        let mut t = HttpTransport::new(&url, &headers, &[]).expect("url should parse");

        let err = t
            .send_request(&init(), DEFAULT_REQUEST_TIMEOUT)
            .await
            .err()
            .expect("a cross-origin endpoint must be refused");
        // Asserted before the message, because this is the property. Without
        // the check the request reaches the thief and the call still ends in
        // `Err` - a timeout waiting for an answer that never comes - so a test
        // that only inspected the error would pass while the credentials had
        // already left.
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no request carrying this server's credentials may reach another origin"
        );
        assert!(
            err.to_string().contains("not its own"),
            "the error should name both origins: {err}"
        );

        // Positive control, so the zero above cannot be an artefact of a
        // counter that never increments or a server that never started.
        reqwest::Client::new()
            .post(format!("{thief_base}/steal"))
            .send()
            .await
            .expect("the thief is listening");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// A redirect that stays on the configured server is ordinary and must keep
    /// working.
    #[tokio::test]
    async fn a_same_origin_redirect_is_followed() {
        let app = Router::new()
            .route(
                "/first",
                get(|| async {
                    (
                        AxumStatus::TEMPORARY_REDIRECT,
                        [(axum::http::header::LOCATION, "/second")],
                    )
                        .into_response()
                }),
            )
            .route("/second", get(|| async { "ok" }));
        let base = serve(app).await;
        let response = build_http_client()
            .get(format!("{base}/first"))
            .send()
            .await
            .expect("a same-origin redirect should be followed");
        assert_eq!(response.status(), 200);
    }

    /// The endpoint-event origin check is not enough on its own: a server can
    /// name its own `/messages`, pass that check, and then answer the POST with
    /// a redirect elsewhere. reqwest strips `Authorization` across origins but
    /// not the `${VAR}`-expanded custom headers an MCP server config carries.
    #[tokio::test]
    async fn a_cross_origin_redirect_does_not_carry_the_headers() {
        let hits = Arc::new(AtomicUsize::new(0));
        let thief = Router::new().fallback({
            let hits = hits.clone();
            move || {
                hits.fetch_add(1, Ordering::SeqCst);
                async { "stolen" }
            }
        });
        let thief_base = serve(thief).await;

        let target = format!("{thief_base}/steal");
        let app = Router::new().route(
            "/first",
            get(move || {
                let target = target.clone();
                async move {
                    (
                        AxumStatus::TEMPORARY_REDIRECT,
                        [(axum::http::header::LOCATION, target)],
                    )
                        .into_response()
                }
            }),
        );
        let base = serve(app).await;

        let response = build_http_client()
            .get(format!("{base}/first"))
            .header("x-api-key", "super-secret")
            .send()
            .await
            .expect("stopping surfaces the 3xx rather than erroring");
        assert_eq!(response.status(), 307, "the redirect is not followed");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "no request carrying the configured headers may reach another origin"
        );

        // Positive control, so the zero above cannot be an artefact of a
        // counter that never increments or a server that never started.
        reqwest::Client::new()
            .get(format!("{thief_base}/steal"))
            .send()
            .await
            .expect("the thief is listening");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    /// A plain-HTTP server is the user's own decision, so it is a warning
    /// rather than a refusal - but a silent one would be wrong, since every
    /// request to it carries the bearer in cleartext.
    #[tokio::test]
    async fn a_cleartext_remote_server_warns() {
        let _guard = always_on_tracing_guard();
        transport("http://mcp.example.com/mcp");
        // Loopback is exempt: there is no network to intercept, and it is how
        // people develop against a local server.
        transport("http://127.0.0.1:9999/mcp");
        transport("https://mcp.example.com/mcp");
    }

    // ─── error paths ──────────────────────────────────────────────────────

    #[test]
    fn an_unparseable_url_is_rejected_up_front() {
        let err = HttpTransport::new("not a url", &HashMap::new(), &[])
            .err()
            .expect("garbage url must not build");
        assert!(
            err.to_string().contains("Invalid MCP server url"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn a_connection_refusal_is_an_error() {
        let _guard = always_on_tracing_guard();
        // Bind then drop, so the port is almost certainly closed.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let mut t = transport(&format!("http://{addr}/mcp"));
        assert!(
            t.send_request(&init(), DEFAULT_REQUEST_TIMEOUT)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_server_error_status_includes_the_body() {
        let _guard = always_on_tracing_guard();
        let app = Router::new().route(
            "/mcp",
            post(|| async { (AxumStatus::INTERNAL_SERVER_ERROR, "upstream exploded") }),
        );
        let url = format!("{}/mcp", serve(app).await);
        let mut t = transport(&url);
        let err = t
            .send_request(&init(), DEFAULT_REQUEST_TIMEOUT)
            .await
            .err()
            .expect("500 must fail");
        assert!(err.to_string().contains("upstream exploded"), "got: {err}");
    }

    #[tokio::test]
    async fn an_empty_error_body_still_reports_the_status() {
        let _guard = always_on_tracing_guard();
        let app = Router::new().route("/mcp", post(|| async { AxumStatus::BAD_GATEWAY }));
        let url = format!("{}/mcp", serve(app).await);
        let mut t = transport(&url);
        let err = t
            .send_request(&init(), DEFAULT_REQUEST_TIMEOUT)
            .await
            .err()
            .expect("502 must fail");
        assert!(err.to_string().contains("502"), "got: {err}");
    }

    #[tokio::test]
    async fn an_unsupported_content_type_is_rejected() {
        let _guard = always_on_tracing_guard();
        let app = Router::new().route(
            "/mcp",
            post(|| async { ([(CONTENT_TYPE, "text/html")], "<html/>").into_response() }),
        );
        let url = format!("{}/mcp", serve(app).await);
        let mut t = transport(&url);
        let err = t
            .send_request(&init(), DEFAULT_REQUEST_TIMEOUT)
            .await
            .err()
            .expect("html must fail");
        assert!(
            err.to_string().contains("unsupported content type"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn a_malformed_json_body_is_a_parse_error() {
        let _guard = always_on_tracing_guard();
        let app = Router::new().route(
            "/mcp",
            post(|| async { ([(CONTENT_TYPE, "application/json")], "not json").into_response() }),
        );
        let url = format!("{}/mcp", serve(app).await);
        let mut t = transport(&url);
        let err = t
            .send_request(&init(), DEFAULT_REQUEST_TIMEOUT)
            .await
            .err()
            .expect("garbage must fail");
        assert!(err.to_string().contains("parse"), "got: {err}");
    }

    #[tokio::test]
    async fn a_json_reply_that_is_not_a_response_is_rejected() {
        let _guard = always_on_tracing_guard();
        // A plain JSON body answering our POST can only be the response to it.
        let app = Router::new().route(
            "/mcp",
            post(|| async {
                (
                    [(CONTENT_TYPE, "application/json")],
                    serde_json::json!({"jsonrpc": "2.0", "method": "notifications/x"}).to_string(),
                )
                    .into_response()
            }),
        );
        let url = format!("{}/mcp", serve(app).await);
        let mut t = transport(&url);
        let err = t
            .send_request(&init(), DEFAULT_REQUEST_TIMEOUT)
            .await
            .err()
            .expect("non-response frame must fail");
        assert!(err.to_string().contains("non-response frame"), "got: {err}");
    }

    #[tokio::test]
    async fn an_sse_stream_that_ends_early_is_an_error() {
        let _guard = always_on_tracing_guard();
        let app = Router::new().route(
            "/mcp",
            post(|| async { ([(CONTENT_TYPE, "text/event-stream")], ": bye\n\n").into_response() }),
        );
        let url = format!("{}/mcp", serve(app).await);
        let mut t = transport(&url);
        let err = t
            .send_request(&init(), DEFAULT_REQUEST_TIMEOUT)
            .await
            .err()
            .expect("truncated stream must fail");
        assert!(
            err.to_string().contains("ended before answering"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn an_unparseable_sse_frame_is_an_error() {
        let _guard = always_on_tracing_guard();
        let app = Router::new().route(
            "/mcp",
            post(|| async {
                ([(CONTENT_TYPE, "text/event-stream")], "data: nonsense\n\n").into_response()
            }),
        );
        let url = format!("{}/mcp", serve(app).await);
        let mut t = transport(&url);
        assert!(
            t.send_request(&init(), DEFAULT_REQUEST_TIMEOUT)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_slow_server_times_out() {
        let _guard = always_on_tracing_guard();
        let app = Router::new().route(
            "/mcp",
            // A handler that never resolves, so the request times out. Written
            // as a function item rather than a closure with a body: a closure
            // block would leave its post-await region permanently unreached.
            post(std::future::pending::<()>),
        );
        let url = format!("{}/mcp", serve(app).await);
        let mut t = transport(&url);
        let err = t
            .send_request(&init(), Duration::from_millis(200))
            .await
            .err()
            .expect("must time out");
        assert!(err.to_string().contains("did not respond"), "got: {err}");
    }

    // ─── response id matching ─────────────────────────────────────────────

    fn resp(id: Value) -> JsonRpcResponse {
        serde_json::from_value(serde_json::json!({
            "jsonrpc": "2.0", "id": id, "result": {}
        }))
        .unwrap()
    }

    #[test]
    fn response_matching_accepts_the_expected_id() {
        assert!(response_matches(&resp(serde_json::json!(3)), Some(3)));
    }

    #[test]
    fn response_matching_rejects_a_different_id() {
        assert!(!response_matches(&resp(serde_json::json!(4)), Some(3)));
    }

    #[test]
    fn response_matching_accepts_a_null_id() {
        // Servers emit a null id on protocol errors; refusing it would strand
        // the caller waiting for a frame that will never come.
        assert!(response_matches(&resp(Value::Null), Some(3)));
    }

    #[test]
    fn response_matching_rejects_a_non_numeric_id() {
        // We only ever send numeric ids, so a string id is not ours.
        assert!(!response_matches(&resp(serde_json::json!("abc")), Some(3)));
    }

    // ─── close ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn close_deletes_a_streamable_session() {
        let _guard = always_on_tracing_guard();
        let deleted = Arc::new(AtomicUsize::new(0));
        let app = Router::new().route(
            "/mcp",
            post(|| async {
                (
                    [
                        (CONTENT_TYPE, "application/json"),
                        (HeaderName::from_static(SESSION_HEADER), "sess-1"),
                    ],
                    ok_frame(1),
                )
                    .into_response()
            })
            .delete({
                let deleted = deleted.clone();
                move || {
                    let deleted = deleted.clone();
                    async move {
                        deleted.fetch_add(1, Ordering::SeqCst);
                        AxumStatus::NO_CONTENT
                    }
                }
            }),
        );
        let url = format!("{}/mcp", serve(app).await);
        let mut t = transport(&url);
        t.send_request(&init(), DEFAULT_REQUEST_TIMEOUT)
            .await
            .unwrap();
        t.close().await.unwrap();
        assert_eq!(deleted.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn close_without_a_session_is_a_no_op() {
        let _guard = always_on_tracing_guard();
        let mut t = transport("http://127.0.0.1:1/mcp");
        t.close().await.expect("close must always succeed");
    }

    #[tokio::test]
    async fn close_aborts_the_legacy_stream() {
        let _guard = always_on_tracing_guard();
        let (app, _tx) = legacy_app("/messages");
        let url = format!("{}/sse", serve(app).await);
        let mut t = transport(&url);
        t.send_request(&init(), DEFAULT_REQUEST_TIMEOUT)
            .await
            .unwrap();
        t.close().await.expect("close must always succeed");
        assert!(t.legacy.is_none());
    }

    // ─── raw-TCP mocks for responses axum cannot produce ──────────────────
    //
    // A declared Content-Length far larger than the bytes actually sent, then
    // a close: reading the body then fails with a genuine I/O error rather
    // than returning short. That is the only way to reach the body-read and
    // stream-error arms.

    async fn serve_raw(response: &'static [u8]) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Exactly one connection: every caller makes a single request, and an
        // accept loop would never terminate, leaving its exit unreachable.
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = [0u8; 8192];
            let _ = socket.read(&mut buf).await;
            let _ = socket.write_all(response).await;
            let _ = socket.flush().await;
            let _ = socket.shutdown().await;
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn a_truncated_json_body_is_a_read_error() {
        let _guard = always_on_tracing_guard();
        let mut t = transport(
            &serve_raw(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
              Content-Length: 9000\r\nConnection: close\r\n\r\n{\"jsonrpc\"",
            )
            .await,
        );
        let err = t
            .send_request(&init(), DEFAULT_REQUEST_TIMEOUT)
            .await
            .err()
            .expect("truncated body must fail");
        assert!(err.to_string().contains("Failed to read"), "got: {err}");
    }

    #[tokio::test]
    async fn a_truncated_event_stream_is_a_stream_error() {
        let _guard = always_on_tracing_guard();
        let mut t = transport(
            &serve_raw(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
             Content-Length: 9000\r\nConnection: close\r\n\r\ndata: partial",
            )
            .await,
        );
        let err = t
            .send_request(&init(), DEFAULT_REQUEST_TIMEOUT)
            .await
            .err()
            .expect("truncated stream must fail");
        assert!(
            err.to_string().contains("event stream failed"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn a_non_utf8_event_stream_is_an_error() {
        let _guard = always_on_tracing_guard();
        // A server writing raw bytes into the stream really does happen.
        let mut t = transport(
            &serve_raw(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
             Connection: close\r\n\r\ndata: \xff\xfe not utf8\n\n",
            )
            .await,
        );
        assert!(
            t.send_request(&init(), DEFAULT_REQUEST_TIMEOUT)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn an_error_body_that_cannot_be_read_still_reports_the_status() {
        let _guard = always_on_tracing_guard();
        let mut t = transport(
            &serve_raw(
                b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 9000\r\n\
             Connection: close\r\n\r\nshort",
            )
            .await,
        );
        let err = t
            .send_request(&init(), DEFAULT_REQUEST_TIMEOUT)
            .await
            .err()
            .expect("503 must fail");
        assert!(err.to_string().contains("503"), "got: {err}");
    }

    // ─── legacy fallback failure modes ────────────────────────────────────

    /// A server that rejects the streamable POST (so the client falls back),
    /// then handles `GET /sse` however `sse` says.
    fn legacy_router(sse: axum::routing::MethodRouter) -> Router {
        Router::new().route(
            "/sse",
            sse.post(|| async { AxumStatus::METHOD_NOT_ALLOWED }),
        )
    }

    #[tokio::test]
    async fn a_rejected_event_stream_fails_the_fallback() {
        let _guard = always_on_tracing_guard();
        let app = legacy_router(get(|| async { AxumStatus::FORBIDDEN }));
        let url = format!("{}/sse", serve(app).await);
        let mut t = transport(&url);
        let err = t
            .send_request(&init(), DEFAULT_REQUEST_TIMEOUT)
            .await
            .err()
            .expect("403 on the stream must fail");
        assert!(
            err.to_string().contains("rejected the event stream"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn an_event_stream_that_never_names_an_endpoint_fails() {
        let _guard = always_on_tracing_guard();
        let app = legacy_router(get(|| async {
            ([(CONTENT_TYPE, "text/event-stream")], ": hello\n\n").into_response()
        }));
        let url = format!("{}/sse", serve(app).await);
        let mut t = transport(&url);
        let err = t
            .send_request(&init(), DEFAULT_REQUEST_TIMEOUT)
            .await
            .err()
            .expect("no endpoint means nowhere to POST");
        assert!(
            err.to_string().contains("before naming a POST endpoint"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn frames_before_the_endpoint_event_are_discarded() {
        let _guard = always_on_tracing_guard();
        // Nothing is outstanding yet, so a frame arriving first answers
        // nothing; it must not be mistaken for the endpoint.
        let app = legacy_router(get(|| async {
            (
                [(CONTENT_TYPE, "text/event-stream")],
                concat!(
                    "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/x\"}\n\n",
                    "data: not json at all\n\n",
                    "event: endpoint\ndata: /messages\n\n",
                ),
            )
                .into_response()
        }));
        let url = format!("{}/sse", serve(app).await);
        let mut t = transport(&url);
        // The stream then ends, so the request itself fails - but only after
        // the endpoint was found, which is what this pins.
        let err = t
            .send_request(&init(), DEFAULT_REQUEST_TIMEOUT)
            .await
            .err()
            .expect("stream ends with no answer");
        assert!(
            !err.to_string().contains("before naming a POST endpoint"),
            "the endpoint should have been found: {err}"
        );
    }

    #[tokio::test]
    async fn an_unusable_endpoint_url_is_rejected() {
        let _guard = always_on_tracing_guard();
        // `http://` has no host, so it cannot be joined onto the base.
        let app = legacy_router(get(|| async {
            (
                [(CONTENT_TYPE, "text/event-stream")],
                "event: endpoint\ndata: http://\n\n",
            )
                .into_response()
        }));
        let url = format!("{}/sse", serve(app).await);
        let mut t = transport(&url);
        let err = t
            .send_request(&init(), DEFAULT_REQUEST_TIMEOUT)
            .await
            .err()
            .expect("unusable endpoint must fail");
        assert!(
            err.to_string().contains("Invalid MCP endpoint"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn a_rejected_legacy_post_fails_the_request() {
        let _guard = always_on_tracing_guard();
        let app = Router::new()
            .route(
                "/sse",
                get(|| async {
                    (
                        [(CONTENT_TYPE, "text/event-stream")],
                        axum::body::Body::from_stream(async_stream::stream! {
                            yield Ok::<_, std::io::Error>(
                                "event: endpoint\ndata: /messages\n\n".to_string());
                            // Hold the stream open so the POST failure, not an
                            // early EOF, is what surfaces.
                            tokio::time::sleep(Duration::from_secs(5)).await;
                        }),
                    )
                        .into_response()
                })
                .post(|| async { AxumStatus::METHOD_NOT_ALLOWED }),
            )
            .route(
                "/messages",
                post(|| async { (AxumStatus::UNAUTHORIZED, "nope") }),
            );
        let url = format!("{}/sse", serve(app).await);
        let mut t = transport(&url);
        let err = t
            .send_request(&init(), DEFAULT_REQUEST_TIMEOUT)
            .await
            .err()
            .expect("401 on the message endpoint must fail");
        assert!(err.to_string().contains("nope"), "got: {err}");
    }

    #[tokio::test]
    async fn a_legacy_stream_that_ends_leaves_the_request_unanswered() {
        let _guard = always_on_tracing_guard();
        let app = Router::new()
            .route(
                "/sse",
                get(|| async {
                    (
                        [(CONTENT_TYPE, "text/event-stream")],
                        "event: endpoint\ndata: /messages\n\n",
                    )
                        .into_response()
                })
                .post(|| async { AxumStatus::METHOD_NOT_ALLOWED }),
            )
            .route("/messages", post(|| async { AxumStatus::ACCEPTED }));
        let url = format!("{}/sse", serve(app).await);
        let mut t = transport(&url);
        let err = t
            .send_request(&init(), DEFAULT_REQUEST_TIMEOUT)
            .await
            .err()
            .expect("no answer can arrive");
        assert!(
            err.to_string().contains("ended before answering"),
            "got: {err}"
        );
    }

    // ─── client construction over HTTP ────────────────────────────────────

    #[tokio::test]
    async fn a_second_request_after_fallback_stays_on_the_legacy_path() {
        let _guard = always_on_tracing_guard();
        // The first request falls back mid-flight; the second must enter
        // already in legacy mode rather than re-probing streamable.
        let (app, _tx) = legacy_app("/messages");
        let url = format!("{}/sse", serve(app).await);
        let mut t = transport(&url);
        t.send_request(&init(), DEFAULT_REQUEST_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(t.mode, Mode::Legacy);

        let value = t
            .send_request(
                &JsonRpcRequest::request(2, "tools/list", serde_json::json!({})),
                DEFAULT_REQUEST_TIMEOUT,
            )
            .await
            .expect("second request should succeed")
            .into_result()
            .unwrap();
        assert_eq!(value, serde_json::json!({"legacy": true}));
    }

    #[tokio::test]
    async fn a_failed_reply_to_a_server_request_does_not_fail_the_call() {
        let _guard = always_on_tracing_guard();
        // Answering a ping is a courtesy; if it fails, the answer we are
        // actually waiting for must still be delivered.
        let calls = Arc::new(AtomicUsize::new(0));
        let body = format!(
            "data: {}\n\ndata: {}\n\n",
            serde_json::json!({"jsonrpc": "2.0", "id": 7, "method": "ping"}),
            ok_frame(1),
        );
        let app = Router::new().route(
            "/mcp",
            post({
                let calls = calls.clone();
                move || {
                    let (body, calls) = (body.clone(), calls.clone());
                    async move {
                        // First POST is the request; every later one (our ping
                        // reply) is rejected.
                        if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                            ([(CONTENT_TYPE, "text/event-stream")], body).into_response()
                        } else {
                            AxumStatus::INTERNAL_SERVER_ERROR.into_response()
                        }
                    }
                }
            }),
        );
        let url = format!("{}/mcp", serve(app).await);
        let mut t = transport(&url);
        let value = t
            .send_request(&init(), DEFAULT_REQUEST_TIMEOUT)
            .await
            .expect("a rejected courtesy reply must not fail the request")
            .into_result()
            .unwrap();
        assert_eq!(value, serde_json::json!({"ok": true}));
    }

    // ─── read_event_stream, driven directly ───────────────────────────────
    //
    // Its failure arms are reached by the state of the *connection*, which no
    // request-level test can arrange deterministically. Calling it with a
    // hand-built response does.

    async fn stream_response(url: &str) -> reqwest::Response {
        build_http_client()
            .get(url)
            .send()
            .await
            .expect("GET should connect")
    }

    #[tokio::test]
    async fn read_event_stream_stops_when_the_receiver_is_gone() {
        let _guard = always_on_tracing_guard();
        // Guards a leak: a transport dropped without `close()` must not leave
        // its reader task spinning on a channel nobody is listening to.
        let url = serve_raw(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
              Connection: close\r\n\r\ndata: {\"jsonrpc\":\"2.0\",\"id\":1}\n\n",
        )
        .await;
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        read_event_stream(stream_response(&url).await, tx).await;
    }

    #[tokio::test]
    async fn read_event_stream_stops_on_invalid_utf8() {
        let _guard = always_on_tracing_guard();
        let url = serve_raw(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
              Connection: close\r\n\r\ndata: \xff\xfe\n\n",
        )
        .await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        read_event_stream(stream_response(&url).await, tx).await;
        assert!(rx.recv().await.is_none(), "nothing decodable was sent");
    }

    #[tokio::test]
    async fn read_event_stream_stops_on_a_stream_error() {
        let _guard = always_on_tracing_guard();
        // Declares far more body than it sends, then closes.
        let url = serve_raw(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
              Content-Length: 9000\r\nConnection: close\r\n\r\ndata: partial",
        )
        .await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        read_event_stream(stream_response(&url).await, tx).await;
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn read_event_stream_ends_cleanly_at_eof() {
        let _guard = always_on_tracing_guard();
        let url = serve_raw(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
              Connection: close\r\n\r\nevent: endpoint\ndata: /messages\n\n",
        )
        .await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        read_event_stream(stream_response(&url).await, tx).await;
        assert!(matches!(
            rx.recv().await,
            Some(LegacyEvent::Endpoint(path)) if path == "/messages"
        ));
        assert!(rx.recv().await.is_none(), "stream is finished");
    }

    #[tokio::test]
    async fn legacy_skips_non_answers_while_waiting() {
        let _guard = always_on_tracing_guard();
        // Between the POST and the answer the server re-issues its endpoint
        // event and sends a notification. Neither answers anything, so both
        // must be stepped over rather than mistaken for the reply.
        let app = Router::new()
            .route(
                "/sse",
                get(|| async {
                    (
                        [(CONTENT_TYPE, "text/event-stream")],
                        axum::body::Body::from_stream(async_stream::stream! {
                            yield Ok::<_, std::io::Error>(
                                "event: endpoint\ndata: /messages\n\n".to_string());
                            // Give the client time to POST before the noise.
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            yield Ok("event: endpoint\ndata: /messages\n\n".to_string());
                            yield Ok(format!(
                                "data: {}\n\n",
                                serde_json::json!({
                                    "jsonrpc": "2.0", "method": "notifications/progress"
                                })));
                            yield Ok(format!("data: {}\n\n", ok_frame(1)));
                        }),
                    )
                        .into_response()
                })
                .post(|| async { AxumStatus::METHOD_NOT_ALLOWED }),
            )
            .route("/messages", post(|| async { AxumStatus::ACCEPTED }));
        let url = format!("{}/sse", serve(app).await);
        let mut t = transport(&url);

        let value = t
            .send_request(&init(), DEFAULT_REQUEST_TIMEOUT)
            .await
            .expect("the real answer should still arrive")
            .into_result()
            .unwrap();
        assert_eq!(value, serde_json::json!({"ok": true}));
    }

    // ─── error-propagation arms ───────────────────────────────────────────
    //
    // Each `foo()?` has an Err-return branch that only a failing `foo()`
    // reaches. A JSON *array* is the lever for the classify() arms: it is
    // valid JSON but neither a response nor a method frame, so `classify`
    // itself errors rather than returning a variant.

    #[tokio::test]
    async fn a_streamable_json_array_reply_fails_classification() {
        let _guard = always_on_tracing_guard();
        let app = Router::new().route(
            "/mcp",
            post(|| async { ([(CONTENT_TYPE, "application/json")], "[1,2,3]").into_response() }),
        );
        let url = format!("{}/mcp", serve(app).await);
        let mut t = transport(&url);
        assert!(
            t.send_request(&init(), DEFAULT_REQUEST_TIMEOUT)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_streamable_sse_array_frame_fails_classification() {
        let _guard = always_on_tracing_guard();
        let app = Router::new().route(
            "/mcp",
            post(|| async {
                ([(CONTENT_TYPE, "text/event-stream")], "data: [1,2,3]\n\n").into_response()
            }),
        );
        let url = format!("{}/mcp", serve(app).await);
        let mut t = transport(&url);
        assert!(
            t.send_request(&init(), DEFAULT_REQUEST_TIMEOUT)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_notification_to_a_dead_server_errors() {
        let _guard = always_on_tracing_guard();
        // Reaches the `post()?` arm inside post_expecting_success.
        let mut t = transport("http://127.0.0.1:1/mcp");
        assert!(
            t.send_notification(&JsonRpcRequest::notification("x", serde_json::json!({})))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn start_legacy_errors_when_the_stream_cannot_be_opened() {
        let _guard = always_on_tracing_guard();
        // Driven directly: reaching this via the public fallback needs the
        // server up (to return 405) yet the GET to fail, which is
        // contradictory on one host. Calling it against a dead port isn't.
        let mut t = transport("http://127.0.0.1:1/sse");
        assert!(t.start_legacy().await.is_err());
    }

    #[tokio::test]
    async fn a_legacy_post_to_a_dead_endpoint_errors() {
        let _guard = always_on_tracing_guard();
        // Legacy mode is established against a live server, then the server
        // goes away, so the *next* request's POST fails at the network level -
        // reaching the `post()?` arm inside `legacy_request`.
        //
        // The endpoint stays on the server's own origin. Naming a closed port
        // on another host would be shorter, but the transport now refuses a
        // cross-origin endpoint event before it ever posts, so that version of
        // this test would pass while exercising none of what it describes.
        let (app, _tx) = legacy_app("/messages");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(std::future::IntoFuture::into_future(axum::serve(
            listener, app,
        )));
        let mut t = transport(&format!("http://{addr}/sse"));
        t.send_request(&init(), DEFAULT_REQUEST_TIMEOUT)
            .await
            .expect("the first request establishes legacy mode");
        assert_eq!(t.mode, Mode::Legacy);

        // Awaiting the aborted handle is what makes this deterministic: it
        // returns only once the task has been polled to cancellation, and the
        // listener it owned is dropped with it.
        server.abort();
        let _ = server.await;
        assert!(
            t.send_request(
                &JsonRpcRequest::request(2, "tools/list", serde_json::json!({})),
                DEFAULT_REQUEST_TIMEOUT
            )
            .await
            .is_err(),
            "a POST to a server that has gone away must fail"
        );
    }

    #[tokio::test]
    async fn a_legacy_array_frame_fails_classification() {
        let _guard = always_on_tracing_guard();
        // Reaches handle_frame's `?` on the legacy path.
        let app = Router::new()
            .route(
                "/sse",
                get(|| async {
                    (
                        [(CONTENT_TYPE, "text/event-stream")],
                        axum::body::Body::from_stream(async_stream::stream! {
                            yield Ok::<_, std::io::Error>(
                                "event: endpoint\ndata: /messages\n\n".to_string());
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            yield Ok("data: [1,2,3]\n\n".to_string());
                        }),
                    )
                        .into_response()
                })
                .post(|| async { AxumStatus::METHOD_NOT_ALLOWED }),
            )
            .route("/messages", post(|| async { AxumStatus::ACCEPTED }));
        let url = format!("{}/sse", serve(app).await);
        let mut t = transport(&url);
        assert!(
            t.send_request(&init(), DEFAULT_REQUEST_TIMEOUT)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_notification_over_legacy_hits_the_id_less_post_path() {
        let _guard = always_on_tracing_guard();
        // Establishes legacy mode, then a notification (no id) is POSTed to
        // /messages, exercising the mock's id-less branch.
        let (app, _tx) = legacy_app("/messages");
        let url = format!("{}/sse", serve(app).await);
        let mut t = transport(&url);
        t.send_request(&init(), DEFAULT_REQUEST_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(t.mode, Mode::Legacy);
        t.send_notification(&JsonRpcRequest::notification("x", serde_json::json!({})))
            .await
            .expect("a 202 to a legacy notification is success");
    }

    // ─── mid-session 401 refresh ──────────────────────────────────────────

    /// A refresher that hands back a fixed token and counts its calls.
    struct CountingRefresher {
        calls: Arc<AtomicUsize>,
        value: String,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl BearerRefresher for CountingRefresher {
        async fn refresh(&self) -> anyhow::Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                anyhow::bail!("refresh boom");
            }
            Ok(self.value.clone())
        }
    }

    #[tokio::test]
    async fn a_401_triggers_a_refresh_and_a_successful_retry() {
        let _guard = always_on_tracing_guard();
        // The first POST (stale token) is answered 401; after the retry carries
        // the refreshed token, it succeeds.
        let calls = Arc::new(AtomicUsize::new(0));
        let app = Router::new().route(
            "/mcp",
            post(|headers: AxumHeaders| async move {
                let auth = headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                if auth == "Bearer fresh" {
                    ([(CONTENT_TYPE, "application/json")], ok_frame(1)).into_response()
                } else {
                    AxumStatus::UNAUTHORIZED.into_response()
                }
            }),
        );
        let url = format!("{}/mcp", serve(app).await);
        let mut t = transport(&url);
        t.set_bearer_refresher(Arc::new(CountingRefresher {
            calls: calls.clone(),
            value: "Bearer fresh".to_string(),
            fail: false,
        }));

        let value = t
            .send_request(&init(), DEFAULT_REQUEST_TIMEOUT)
            .await
            .expect("refresh + retry should succeed")
            .into_result()
            .unwrap();
        assert_eq!(value, serde_json::json!({"ok": true}));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "refreshed exactly once");
    }

    #[tokio::test]
    async fn a_401_without_a_refresher_surfaces_the_error() {
        let _guard = always_on_tracing_guard();
        let app = Router::new().route("/mcp", post(|| async { AxumStatus::UNAUTHORIZED }));
        let url = format!("{}/mcp", serve(app).await);
        let mut t = transport(&url);
        // No refresher configured → the 401 is a plain failure.
        assert!(
            t.send_request(&init(), DEFAULT_REQUEST_TIMEOUT)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_failed_refresh_propagates() {
        let _guard = always_on_tracing_guard();
        let calls = Arc::new(AtomicUsize::new(0));
        let app = Router::new().route("/mcp", post(|| async { AxumStatus::UNAUTHORIZED }));
        let url = format!("{}/mcp", serve(app).await);
        let mut t = transport(&url);
        t.set_bearer_refresher(Arc::new(CountingRefresher {
            calls: calls.clone(),
            value: String::new(),
            fail: true,
        }));
        let err = t
            .send_request(&init(), DEFAULT_REQUEST_TIMEOUT)
            .await
            .err()
            .expect("a failed refresh must surface");
        assert!(err.to_string().contains("refresh boom"), "got: {err}");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn the_retry_happens_at_most_once() {
        let _guard = always_on_tracing_guard();
        // The server always 401s (even after refresh); the retry must not loop.
        let calls = Arc::new(AtomicUsize::new(0));
        let app = Router::new().route("/mcp", post(|| async { AxumStatus::UNAUTHORIZED }));
        let url = format!("{}/mcp", serve(app).await);
        let mut t = transport(&url);
        t.set_bearer_refresher(Arc::new(CountingRefresher {
            calls: calls.clone(),
            value: "Bearer still-bad".to_string(),
            fail: false,
        }));
        assert!(
            t.send_request(&init(), DEFAULT_REQUEST_TIMEOUT)
                .await
                .is_err()
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "refresh tried once, not in a loop"
        );
    }

    #[test]
    fn set_auth_header_rejects_an_invalid_value() {
        let _guard = always_on_tracing_guard();
        // A control character can't be a header value; the update is skipped.
        let mut t = transport("http://127.0.0.1:1/mcp");
        t.set_auth_header("Bearer with\nnewline");
        assert!(t.headers.get("authorization").is_none());
        // A valid value is stored.
        t.set_auth_header("Bearer good");
        assert_eq!(t.headers.get("authorization").unwrap(), "Bearer good");
    }
}
