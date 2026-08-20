//! The HTTP seam behind the Rhai provider's host functions.
//!
//! Rhai scripts are synchronous and run on a `spawn_blocking` thread; their
//! `http_get`/`http_post`/`stream_request` host functions send an [`HttpJob`] /
//! [`StreamHttpJob`] over a channel to an async **broker** that performs the
//! real request through an [`HttpExecutor`]. Production uses [`ReqwestExecutor`];
//! tests inject a fake so no socket is ever bound. Rate limiting lives in the
//! provider (around the executor), not here - the executor is pure transport.

use std::collections::BTreeMap;

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};

/// HTTP method for a host request (the only two a provider script needs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    /// HTTP GET.
    Get,
    /// HTTP POST.
    Post,
}

/// A single HTTP request a script asked the host to perform.
#[derive(Debug, Clone)]
pub struct HostRequest {
    /// GET or POST.
    pub method: HttpMethod,
    /// Target URL.
    pub url: String,
    /// Request body (POST only).
    pub body: Option<String>,
    /// Request headers.
    pub headers: BTreeMap<String, String>,
    /// Per-request wall-clock deadline in seconds (the stage timeout).
    pub timeout_secs: Option<u64>,
}

/// A transport-level error from performing a [`HostRequest`], carrying enough
/// classification for the provider to map it to the right `ProviderError`.
#[derive(Debug, Clone)]
pub enum HostHttpError {
    /// HTTP 429 with an optional `Retry-After` (seconds).
    RateLimited {
        /// Parsed `Retry-After` header value, if present.
        retry_after: Option<u64>,
    },
    /// A non-2xx, non-429 response: `HTTP <status>: <body>`.
    Api(String),
    /// A transport failure (connection/timeout/read error).
    Transport(String),
}

/// The item type of a streaming job's event channel: one SSE `data:` payload,
/// or a terminal error.
pub type EventResult = Result<String, HostHttpError>;

/// A unary HTTP job: a request plus a one-shot reply channel.
pub struct HttpJob {
    /// The request to perform.
    pub request: HostRequest,
    /// Where the broker sends the result back to the blocking host fn.
    pub reply: oneshot::Sender<Result<String, HostHttpError>>,
}

/// A streaming HTTP job: a request plus a channel the broker feeds SSE `data:`
/// payloads into (one `Ok(String)` per event; a single `Err` on failure).
pub struct StreamHttpJob {
    /// The request to perform.
    pub request: HostRequest,
    /// Where the broker sends each parsed SSE data payload (or a terminal error).
    pub events: mpsc::Sender<Result<String, HostHttpError>>,
}

/// Jobs the broker services on behalf of a running script.
pub enum BrokerJob {
    /// A unary request/response.
    Unary(HttpJob),
    /// A streaming request.
    Stream(StreamHttpJob),
}

/// The async HTTP transport a provider's broker uses. Production =
/// [`ReqwestExecutor`]; tests inject a fake.
#[async_trait]
pub trait HttpExecutor: Send + Sync {
    /// Perform a unary request, returning the response body or a classified error.
    async fn execute(&self, req: HostRequest) -> Result<String, HostHttpError>;

    /// Perform a streaming request, sending each SSE `data:` payload into
    /// `events` (dropping the sender on stream end). The default parses standard
    /// SSE; a fake may override.
    async fn execute_stream(
        &self,
        req: HostRequest,
        events: mpsc::Sender<Result<String, HostHttpError>>,
    );
}

/// The production [`HttpExecutor`] backed by `reqwest`.
pub struct ReqwestExecutor {
    client: reqwest::Client,
    /// An HTTP/1.1-only client, used only to retry an HTTP/2 protocol fault.
    /// `None` when one could not be built, which just means no fallback.
    h1_client: Option<reqwest::Client>,
}

impl ReqwestExecutor {
    /// Build an executor with a fresh-connection-per-request client (the
    /// `pool_max_idle_per_host(0)` fix); per-request deadlines come from
    /// [`HostRequest::timeout_secs`].
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            h1_client: None,
        }
    }

    /// An executor that can fall back to `h1_client` when an origin negotiates
    /// HTTP/2 and then fails every stream on it. See
    /// [`crate::provider::build_http1_client`].
    pub fn with_h1_fallback(client: reqwest::Client, h1_client: reqwest::Client) -> Self {
        Self {
            client,
            h1_client: Some(h1_client),
        }
    }

    /// Build the `reqwest::RequestBuilder` for a [`HostRequest`].
    fn build(&self, req: &HostRequest) -> reqwest::RequestBuilder {
        self.build_with(&self.client, req)
    }

    /// [`Self::build`] against a specific client, so the retry path can send
    /// the identical request over the HTTP/1.1-only one.
    fn build_with(&self, client: &reqwest::Client, req: &HostRequest) -> reqwest::RequestBuilder {
        let mut rb = match req.method {
            HttpMethod::Get => client.get(&req.url),
            HttpMethod::Post => client.post(&req.url),
        };
        for (k, v) in &req.headers {
            rb = rb.header(k, v);
        }
        if let Some(body) = &req.body {
            rb = rb.body(body.clone());
        }
        crate::provider::apply_request_timeout(rb, req.timeout_secs)
    }

    /// Send `req`, retrying a transport failure that looks transient and
    /// falling back to HTTP/1.1 once on an HTTP/2 protocol fault.
    ///
    /// Before this, one `send()` was the whole story: a single h2 stream error
    /// permanently lost the source, and the tool script's diagnostic blamed
    /// page size or blocking, neither of which was true.
    async fn send_with_retry(&self, req: &HostRequest) -> Result<reqwest::Response, HostHttpError> {
        let mut attempt = 0;
        // Sticky for the rest of the loop, deliberately. An earlier cut kept
        // this on the executor and recomputed it per attempt, so an h1 retry
        // that failed for its own reason cleared the flag and the next attempt
        // went straight back to the protocol already known to be broken here.
        let mut use_h1 = false;
        loop {
            let client = retry_client(&self.client, self.h1_client.as_ref(), use_h1);
            match self.build_with(client, req).send().await {
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    let chain = error_chain(&e);
                    if attempt >= TRANSPORT_RETRIES || !is_retryable_transport(&chain) {
                        return Err(HostHttpError::Transport(chain));
                    }
                    use_h1 = use_h1 || is_h2_protocol_error(&chain);
                    attempt += 1;
                    tracing::debug!(
                        url = %req.url,
                        attempt,
                        over_http1 = use_h1,
                        error = %chain,
                        "retrying a transport failure"
                    );
                    tokio::time::sleep(transport_backoff(attempt)).await;
                }
            }
        }
    }
}

/// Classify a response status, extracting `Retry-After` on 429. Returns `Ok`
/// (with the response, for the caller to read the body) on 2xx.
async fn classify(resp: reqwest::Response) -> Result<reqwest::Response, HostHttpError> {
    let status = resp.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        return Err(HostHttpError::RateLimited { retry_after });
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_else(|e| e.to_string());
        return Err(HostHttpError::Api(format!("HTTP {status}: {body}")));
    }
    Ok(resp)
}

/// Build the shared executor from the two outbound clients.
///
/// Takes both as `Result`s rather than building them, because the only caller
/// is a daemon-start-up path with nothing to return an error to and `reqwest`
/// cannot be made to fail from the outside. As parameters, "no HTTPS client at
/// all" and "a client but no HTTP/1.1 fallback" are both reachable from a test.
pub fn executor_from_clients(
    main: std::result::Result<reqwest::Client, crate::provider::HttpError>,
    h1: std::result::Result<reqwest::Client, crate::provider::HttpError>,
) -> std::result::Result<std::sync::Arc<dyn HttpExecutor>, crate::provider::HttpError> {
    main.map(|client| {
        let exec = match h1 {
            Ok(h1) => ReqwestExecutor::with_h1_fallback(client, h1),
            // No fallback is a degraded executor, not a failure: every origin
            // that works over HTTP/2 still works.
            Err(_) => ReqwestExecutor::new(client),
        };
        std::sync::Arc::new(exec) as std::sync::Arc<dyn HttpExecutor>
    })
}

/// Which client an attempt should use.
///
/// A free function because the HTTP/1.1 arm is otherwise only reachable from a
/// live origin that negotiates HTTP/2 and then fails the stream, which no local
/// test can stage. Here it is one comparison a test can make directly.
fn retry_client<'a>(
    main: &'a reqwest::Client,
    h1: Option<&'a reqwest::Client>,
    use_h1: bool,
) -> &'a reqwest::Client {
    match h1 {
        Some(client) if use_h1 => client,
        _ => main,
    }
}

/// How many extra attempts a retryable transport failure gets.
///
/// Deliberately not `inference_retry_attempts`: that governs a whole inference
/// call with a model's latency behind it, while this is a socket that failed
/// before any work happened.
const TRANSPORT_RETRIES: u32 = 2;

/// Backoff before retry `n` (1-based).
fn transport_backoff(attempt: u32) -> std::time::Duration {
    std::time::Duration::from_millis(200 * u64::from(attempt))
}

/// Flatten an error and its `source()` chain into one string.
///
/// The classification below reads this rather than the `reqwest::Error` itself:
/// the interesting detail (an `h2` protocol fault) is several links down the
/// chain, and `reqwest::Error` has no public constructor, so a chain string is
/// also the only shape a test can drive.
pub fn error_chain(err: &(dyn std::error::Error + 'static)) -> String {
    let mut out = err.to_string();
    let mut source = err.source();
    while let Some(e) = source {
        out.push_str(": ");
        out.push_str(&e.to_string());
        source = e.source();
    }
    out
}

/// Whether a transport failure is worth another attempt.
///
/// Connection resets, timeouts and protocol faults are all "the socket did not
/// work this time". A DNS failure or a refused connection is not: retrying
/// those just spends the deadline, so they fall through to the error the agent
/// sees.
pub fn is_retryable_transport(chain: &str) -> bool {
    let lower = chain.to_lowercase();
    is_h2_protocol_error(chain)
        || lower.contains("connection reset")
        || lower.contains("connection closed")
        || lower.contains("broken pipe")
        || lower.contains("unexpected eof")
        || lower.contains("timed out")
}

/// Whether the failure is an HTTP/2 protocol fault that a plain HTTP/1.1
/// attempt is likely to clear.
///
/// Measured against `investors.cerebras.ai`, which answered a burst of
/// concurrent fetches with `http2 error: stream error received: unexpected
/// internal error encountered` and served the same URL fine over HTTP/1.1.
/// Both of that host's pages were primary sources for a research run, and
/// because nothing retried, the run cited two URLs it had never read.
pub fn is_h2_protocol_error(chain: &str) -> bool {
    let lower = chain.to_lowercase();
    lower.contains("http2 error") || lower.contains("h2 error")
}

/// Content types that are not text a model can read.
///
/// Checked before the body is decoded, so a PDF or an image is named rather
/// than shovelled into a context region as bytes.
pub(crate) fn unsupported_content_type(content_type: Option<&str>) -> Option<String> {
    let ct = content_type?;
    let essence = ct.split(';').next().unwrap_or(ct).trim().to_lowercase();
    let readable = essence.starts_with("text/")
        || essence.contains("json")
        || essence.contains("xml")
        || essence.contains("javascript")
        || essence.contains("x-yaml")
        || essence.contains("urlencoded");
    match readable {
        true => None,
        false => Some(format!(
            "the response is {essence}, not text - this fetch read bytes, not a document"
        )),
    }
}

/// Whether a decoded body is mostly Unicode replacement characters.
///
/// This is the shape a compressed body takes after a lossy UTF-8 decode, and it
/// is the last line of defence behind the `gzip`/`brotli`/`zstd` features: a
/// server that compresses unsolicited (finance.yahoo.com does) used to land a
/// whole gzip member in a research agent's sources region, and nothing noticed
/// because the tool's own emptiness check only catches *short* output.
///
/// The ratio, not a count: a legitimate page may carry a stray replacement
/// character from a genuinely malformed byte, but not one in ten.
pub(crate) fn looks_like_mojibake(body: &str) -> bool {
    let mut total = 0usize;
    let mut bad = 0usize;
    for ch in body.chars() {
        total += 1;
        if ch == char::REPLACEMENT_CHARACTER {
            bad += 1;
        }
    }
    total >= 64 && bad * 10 > total
}

#[async_trait]
impl HttpExecutor for ReqwestExecutor {
    async fn execute(&self, req: HostRequest) -> Result<String, HostHttpError> {
        let resp = self.send_with_retry(&req).await?;
        let resp = classify(resp).await?;
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        if let Some(reason) = unsupported_content_type(content_type.as_deref()) {
            return Err(HostHttpError::Api(format!("unreadable body: {reason}")));
        }
        let body = resp
            .text()
            .await
            .map_err(|e| HostHttpError::Transport(e.to_string()))?;
        // Named rather than returned. A caller cannot tell mojibake from a page
        // that genuinely says very little, and a model handed either will cite
        // whatever it remembers being at that URL.
        if looks_like_mojibake(&body) {
            return Err(HostHttpError::Api(format!(
                "unreadable body: {} decoded to mostly replacement characters, so it \
                 was compressed or in an encoding this client could not decode. \
                 This fetch did not read the page.",
                content_type.as_deref().unwrap_or("the response")
            )));
        }
        Ok(body)
    }

    async fn execute_stream(
        &self,
        req: HostRequest,
        events: mpsc::Sender<Result<String, HostHttpError>>,
    ) {
        let resp = match self.build(&req).send().await {
            Ok(r) => r,
            Err(e) => {
                let _ = events
                    .send(Err(HostHttpError::Transport(e.to_string())))
                    .await;
                return;
            }
        };
        let resp = match classify(resp).await {
            Ok(r) => r,
            Err(e) => {
                let _ = events.send(Err(e)).await;
                return;
            }
        };
        forward_sse(resp.bytes_stream(), events).await;
    }
}

/// Drain a byte stream as Server-Sent Events, forwarding each `data:` payload
/// into `events`. Stops on the `[DONE]` sentinel; on a read error sends one
/// `Err` and stops. Dropping `events` on return signals stream end.
pub(crate) async fn forward_sse<S, E>(
    stream: S,
    events: mpsc::Sender<Result<String, HostHttpError>>,
) where
    S: futures_core::Stream<Item = Result<bytes::Bytes, E>>,
    E: std::fmt::Display,
{
    use tokio_stream::StreamExt;
    tokio::pin!(stream);
    let mut buffer = String::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(bytes) => {
                if let Ok(text) = std::str::from_utf8(&bytes) {
                    buffer.push_str(text);
                }
                for ev in drain_sse_events(&mut buffer) {
                    match ev {
                        SseEvent::Data(payload) => {
                            if events.send(Ok(payload)).await.is_err() {
                                return; // receiver gone
                            }
                        }
                        SseEvent::Done => return,
                    }
                }
            }
            Err(e) => {
                let _ = events
                    .send(Err(HostHttpError::Transport(e.to_string())))
                    .await;
                return;
            }
        }
    }
    // Flush a trailing event that had no final blank line.
    if let Some(SseEvent::Data(payload)) = final_sse_event(&buffer) {
        let _ = events.send(Ok(payload)).await;
    }
}

/// One parsed SSE event: a `data:` payload or the `[DONE]` sentinel.
enum SseEvent {
    Data(String),
    Done,
}

/// Pull every complete (`\n\n`-terminated) event out of `buffer`.
fn drain_sse_events(buffer: &mut String) -> Vec<SseEvent> {
    let mut out = Vec::new();
    while let Some(idx) = buffer.find("\n\n") {
        let block: String = buffer.drain(..idx + 2).collect();
        if let Some(ev) = parse_sse_block(&block) {
            out.push(ev);
        }
    }
    out
}

/// Parse a leftover (non-`\n\n`-terminated) trailing block at stream end.
fn final_sse_event(buffer: &str) -> Option<SseEvent> {
    if buffer.trim().is_empty() {
        return None;
    }
    parse_sse_block(buffer)
}

/// Extract the `data:` payload from one SSE event block. Returns `Done` on the
/// `[DONE]` sentinel and `None` when the block carries no data line.
fn parse_sse_block(block: &str) -> Option<SseEvent> {
    for line in block.lines() {
        let line = line.trim_start();
        if let Some(data) = line.strip_prefix("data:") {
            let data = data.trim();
            if data == "[DONE]" {
                return Some(SseEvent::Done);
            }
            if !data.is_empty() {
                return Some(SseEvent::Data(data.to_string()));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests;
