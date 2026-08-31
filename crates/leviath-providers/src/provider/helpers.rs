//! The pieces every provider reaches for, kept out of the trait's own file.
//!
//! Nothing here is part of the [`Provider`](super::Provider) contract. They
//! are the shared implementation details several providers happen to need: the
//! Chat Completions vocabulary that OpenAI and OpenRouter both speak, the
//! once-per-process memo a provider builds by being refused, the capped body
//! reader, and a one-item stream.
//!
//! Split out when `provider.rs` reached the production-line limit, which was
//! the right moment: the trait, its request and response types, and the errors
//! it can raise are one thing to read, and this is the other.

use super::{FinishReason, ProviderError, Result, UnavailableReason};
use crate::failure::FailureKind;
use leviath_net::read_caps::{BodyReadError, JSON_BODY_CAP, read_body_capped, read_text_capped};

/// Map an OpenAI-style `finish_reason` string to a `FinishReason`.
///
/// Used by both the OpenAI and OpenRouter providers which share the same
/// Chat Completions API response schema.
pub(crate) fn parse_openai_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "stop" => FinishReason::Complete,
        "tool_calls" => FinishReason::ToolCall,
        "length" => FinishReason::TokenLimit,
        other => {
            tracing::debug!(
                reason = other,
                "unrecognised finish_reason from the provider"
            );
            FinishReason::Unknown
        }
    }
}

/// Turn the argument text of a tool call into the value the runtime executes.
///
/// Empty text is a call with no arguments, and becomes `{}` so a tool that
/// takes none is still called. Text that is not JSON is **kept as text**
/// rather than replaced by `{}`: the usual reason it is not JSON is that the
/// reply hit its output cap mid-argument, and executing the tool with nothing
/// hid that from the model. It re-sent the same oversized call and was cut
/// off the same way, five times in a row, before the stage gave up. A string
/// where an object should be fails schema validation, and the runtime reads
/// the string shape as "this call was cut off" and says so back to the model.
pub(crate) fn parse_tool_arguments(raw: &str) -> serde_json::Value {
    let raw = raw.trim();
    if raw.is_empty() {
        return serde_json::Value::Object(serde_json::Map::new());
    }
    serde_json::from_str(raw).unwrap_or_else(|_| serde_json::Value::String(raw.to_string()))
}

/// Model names remembered for the life of the process.
///
/// What a provider learns about a model by being refused (no temperature,
/// no tools over a reasoning effort) or by warning about it once is worth
/// keeping across requests, and clones of the provider share the same set.
#[derive(Debug, Clone, Default)]
pub(crate) struct ModelMemo(std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>);

impl ModelMemo {
    /// Whether `model` has been recorded.
    pub(crate) fn contains(&self, model: &str) -> bool {
        leviath_core::sync::lock(&self.0).contains(model)
    }

    /// Record `model`; `true` the first time, `false` if it was already there.
    pub(crate) fn insert(&self, model: &str) -> bool {
        leviath_core::sync::lock(&self.0).insert(model.to_string())
    }

    /// How many models are recorded.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        leviath_core::sync::lock(&self.0).len()
    }

    /// Whether nothing has been recorded.
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// How long a response's `Retry-After` header asks the caller to wait.
///
/// Only the delta-seconds form is read. The header's other form is an HTTP
/// date, which every provider API in use here answers with seconds instead, and
/// reading it would mean trusting the server's clock against ours; an
/// unparseable value is treated as no hint at all, which falls back to the
/// caller's own backoff.
pub(crate) fn retry_after_secs(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
}

/// Check an HTTP response for errors and return it on success.
///
/// - On 429 (rate limit): notifies the optional rate limiter and returns `RateLimitExceeded`.
/// - On a provider-fatal failure (see [`UnavailableReason::classify`]): returns `Unavailable`.
/// - On any other non-2xx: reads the body and returns `ApiError`.
/// - On 2xx: returns `Ok(response)` so the caller can read the body.
///
/// Pass the full `reqwest::Response`; it is returned back on success.
pub(crate) async fn check_http_response(
    response: reqwest::Response,
    limiter: Option<&crate::rate_limit::RateLimiter>,
) -> Result<reqwest::Response> {
    let status = response.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        // Extract retry-after *before* consuming the response body.
        let retry_after = retry_after_secs(response.headers());
        if let Some(l) = limiter {
            l.handle_rate_limit(retry_after).await;
        }
        // The hint rides along on the error: the client-side limiter paces the
        // *next* request, while the dispatch layer's retry loop is what decides
        // how long this one waits before trying again.
        return Err(ProviderError::RateLimitExceeded {
            retry_after_secs: retry_after,
        });
    }
    if !status.is_success() {
        // Capped like a good body: an error page is a body too, and a gateway
        // that answers a 502 with its whole access log is still a peer.
        let error_body = read_text_capped(response, JSON_BODY_CAP)
            .await
            .unwrap_or_else(|e| e.to_string());
        // The kind rides in front of the status so it survives every layer that
        // only passes strings - including a Rhai script, which sees the message
        // and nothing else. A bare status left "their endpoint is down" and
        // "your base_url has a typo" reading identically.
        let kind = FailureKind::from_status(status.as_u16());
        let detail = format!(
            "[{}] HTTP {}: {} - {}",
            kind.label(),
            status,
            error_body,
            kind.remedy()
        );
        // An out-of-credits or bad-key response is worth telling apart: the
        // runtime fails over on it and counts it against the provider's
        // circuit breaker, where a plain `ApiError` would just kill the run.
        return Err(
            match UnavailableReason::classify(status.as_u16(), &error_body) {
                Some(reason) => ProviderError::Unavailable { reason, detail },
                None => ProviderError::ApiError(detail),
            },
        );
    }
    Ok(response)
}

/// Read a response body to completion and parse it as JSON.
///
/// The two halves fail for entirely different reasons and must not share an
/// error variant. Bytes that never arrived - a reset connection, a socket that
/// died while the machine was asleep, a truncated body - are a *transport*
/// failure: [`ProviderError::RequestFailed`], which is transient, gets retried,
/// counts against the provider's circuit breaker and is eligible for failover.
/// Bytes that arrived and did not fit the schema are the provider's own fault:
/// [`ProviderError::InvalidResponse`], which is permanent, because sending the
/// same request again produces the same unusable answer.
///
/// `reqwest`'s own `Response::json` collapses both into one `Decode` error whose
/// message is the famously unhelpful "error decoding response body". Routing
/// that through `InvalidResponse` made every network blip permanent: a run with
/// dozens of iterations of completed work died outright rather than retrying,
/// because a dead socket was being reported as malformed JSON. The streaming
/// path already drew this line correctly (see `openai_compat::stream_chat`);
/// this is the buffered path drawing the same one.
pub(crate) async fn decode_json<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T> {
    decode_json_capped(response, JSON_BODY_CAP).await
}

/// [`decode_json`] with the body cap as a parameter, so a test can hit the
/// cap with a few kilobytes rather than 64 MiB.
///
/// A body past the cap is [`ProviderError::InvalidResponse`]: the same
/// request would draw the same oversized answer, so retrying it only spends
/// the attempts, and the message names the cap and the peer.
pub(crate) async fn decode_json_capped<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
    cap: usize,
) -> Result<T> {
    let bytes = read_body_capped(response, cap)
        .await
        .map_err(ProviderError::from)?;
    serde_json::from_slice(&bytes).map_err(|e| ProviderError::InvalidResponse(e.to_string()))
}

impl From<BodyReadError> for ProviderError {
    /// Bytes that never arrived are a transport failure (retried, counted
    /// against the breaker); a body past the cap is the provider's own fault
    /// and permanent, since the same request draws the same oversized answer.
    fn from(e: BodyReadError) -> Self {
        match e {
            BodyReadError::Transport(e) => {
                ProviderError::transport("reading the response body", &e)
            }
            too_large @ BodyReadError::TooLarge { .. } => {
                ProviderError::InvalidResponse(too_large.to_string())
            }
        }
    }
}

// Helper module for single-item streams
pub(super) mod stream_once {
    use futures_core::Stream;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    pub struct Once<T> {
        item: Option<T>,
    }

    impl<T: Unpin> Stream for Once<T> {
        type Item = T;

        fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Ready(self.item.take())
        }
    }

    pub fn once<T>(item: T) -> Once<T> {
        Once { item: Some(item) }
    }
}
