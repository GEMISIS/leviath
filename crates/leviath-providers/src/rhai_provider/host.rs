//! The HTTP seam behind the Rhai provider's host functions.
//!
//! Rhai scripts are synchronous and run on a `spawn_blocking` thread; their
//! `http_get`/`http_post`/`stream_request` host functions send an [`HttpJob`] /
//! [`StreamHttpJob`] over a channel to an async **broker** that performs the
//! real request through an [`HttpExecutor`]. Production uses [`ReqwestExecutor`];
//! tests inject a fake so no socket is ever bound. Rate limiting lives in the
//! provider (around the executor), not here — the executor is pure transport.

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
}

impl ReqwestExecutor {
    /// Build an executor with a fresh-connection-per-request client (the
    /// `pool_max_idle_per_host(0)` fix); per-request deadlines come from
    /// [`HostRequest::timeout_secs`].
    pub fn new() -> Self {
        Self {
            client: crate::provider::build_http_client(None),
        }
    }

    /// Build the `reqwest::RequestBuilder` for a [`HostRequest`].
    fn build(&self, req: &HostRequest) -> reqwest::RequestBuilder {
        let mut rb = match req.method {
            HttpMethod::Get => self.client.get(&req.url),
            HttpMethod::Post => self.client.post(&req.url),
        };
        for (k, v) in &req.headers {
            rb = rb.header(k, v);
        }
        if let Some(body) = &req.body {
            rb = rb.body(body.clone());
        }
        crate::provider::apply_request_timeout(rb, req.timeout_secs)
    }
}

impl Default for ReqwestExecutor {
    fn default() -> Self {
        Self::new()
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

#[async_trait]
impl HttpExecutor for ReqwestExecutor {
    async fn execute(&self, req: HostRequest) -> Result<String, HostHttpError> {
        let resp = self
            .build(&req)
            .send()
            .await
            .map_err(|e| HostHttpError::Transport(e.to_string()))?;
        let resp = classify(resp).await?;
        resp.text()
            .await
            .map_err(|e| HostHttpError::Transport(e.to_string()))
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
#[path = "host_tests.rs"]
mod tests;
