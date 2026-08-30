//! Conversions between Rhai values and Leviath inference types, plus mapping of
//! script/host errors into [`ProviderError`].

use rhai::{Dynamic, EvalAltResult, Map, Position};
use serde_json::Value;

use crate::provider::{
    FinishReason, InferenceResponse, ProviderError, Result, StreamChunk, TokenUsage, ToolCall,
    ToolCallDelta,
};

use super::host::HostHttpError;

/// Map a finish-reason string (Leviath-style or common API spellings) to a
/// [`FinishReason`]. Unknown/absent → `Complete`.
pub fn finish_reason_from_str(s: Option<&str>) -> FinishReason {
    match s {
        Some("ToolCall") | Some("tool_calls") | Some("tool_use") => FinishReason::ToolCall,
        Some("TokenLimit") | Some("length") | Some("max_tokens") => FinishReason::TokenLimit,
        Some("Stop") | Some("stop_sequence") => FinishReason::Stop,
        _ => FinishReason::Complete,
    }
}

/// Extract a `usize` field from a JSON object, defaulting to 0.
fn usize_field(obj: Option<&Value>, key: &str) -> usize {
    obj.and_then(|v| v.get(key))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize
}

/// Build a [`TokenUsage`] from an optional JSON `tokens`/`tokens_used` object.
fn parse_usage(obj: Option<&Value>) -> TokenUsage {
    // A script provider may report `cost_usd` for the call it just made. It is
    // the one that knows: it wrote the request, it saw whatever the endpoint
    // said, and nothing here has a rate card for a model it has never heard of.
    // Absent, the call is unpriced and its run reports UNKNOWN rather than a
    // total quietly missing it.
    let cost = obj
        .and_then(|v| v.get("cost_usd").or_else(|| v.get("cost")))
        .and_then(|v| v.as_f64());
    TokenUsage {
        prompt_tokens: usize_field(obj, "prompt_tokens"),
        completion_tokens: usize_field(obj, "completion_tokens"),
        total_tokens: usize_field(obj, "total_tokens"),
        cached_tokens: usize_field(obj, "cached_tokens"),
        cache_write_tokens: usize_field(obj, "cache_write_tokens"),
        reported_cost_usd: cost,
    }
}

/// Convert the map returned by a script's `inference` function into an
/// [`InferenceResponse`]. Lenient: missing fields default, and Rhai type quirks
/// (unit, i64 vs f64) are absorbed by going through `serde_json::Value`.
pub fn parse_inference_dynamic(value: Dynamic) -> Result<InferenceResponse> {
    let json: Value = rhai::serde::from_dynamic(&value).map_err(|e| {
        ProviderError::InvalidResponse(format!(
            "provider script returned an unconvertible value: {e}"
        ))
    })?;
    if !json.is_object() {
        return Err(ProviderError::InvalidResponse(format!(
            "provider script `inference` must return a map, got: {json}"
        )));
    }
    Ok(InferenceResponse {
        content: json
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        tool_calls: parse_tool_calls(&json),
        tokens_used: parse_usage(json.get("tokens_used")),
        finish_reason: finish_reason_from_str(json.get("finish_reason").and_then(|v| v.as_str())),
        reasoning: None,
    })
}

/// Parse the `tool_calls` array of an `inference` result.
fn parse_tool_calls(json: &Value) -> Vec<ToolCall> {
    json.get("tool_calls")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|tc| ToolCall {
                    id: tc
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    name: tc
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    arguments: tc.get("arguments").cloned().unwrap_or(Value::Null),
                    // A script wrapping a provider that issues per-call replay
                    // tokens (Gemini's `thought_signature`) can pass one through.
                    thought_signature: tc
                        .get("thought_signature")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Convert a map the script passed to `on_chunk` into a [`StreamChunk`]. `tokens`
/// and `finish_reason` are only populated when present (so mid-stream deltas
/// don't carry a spurious zeroed usage or a Complete reason).
pub fn chunk_from_dynamic(value: Dynamic) -> Result<StreamChunk> {
    let json: Value = rhai::serde::from_dynamic(&value).map_err(|e| {
        ProviderError::InvalidResponse(format!("on_chunk received an unconvertible value: {e}"))
    })?;
    Ok(StreamChunk {
        delta: json
            .get("delta")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        tool_calls: parse_tool_call_deltas(&json),
        tokens: json.get("tokens").map(|t| parse_usage(Some(t))),
        finish_reason: json
            .get("finish_reason")
            .and_then(|v| v.as_str())
            .map(|s| finish_reason_from_str(Some(s))),
        // A script provider speaks whatever wire format it likes; there is no
        // opaque reasoning item to carry, and inventing one from a script's
        // JSON would let a script forge another provider's token.
        reasoning: None,
    })
}

/// Parse the `tool_calls` array of a stream chunk into [`ToolCallDelta`]s.
fn parse_tool_call_deltas(json: &Value) -> Vec<ToolCallDelta> {
    json.get("tool_calls")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|tc| ToolCallDelta {
                    index: tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
                    id: tc.get("id").and_then(|v| v.as_str()).map(str::to_string),
                    name: tc.get("name").and_then(|v| v.as_str()).map(str::to_string),
                    arguments_delta: tc
                        .get("arguments_delta")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    // A script talking to an endpoint that signs its tool calls
                    // can pass the signature straight through; one that does
                    // not simply omits the key.
                    thought_signature: tc
                        .get("thought_signature")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Build a Rhai runtime exception carrying a structured error map so the
/// provider can distinguish a 429 from other host failures after it propagates
/// out of the script (`kind` ∈ {`rate_limited`, `api`, `transport`}).
pub fn host_err_to_rhai(e: HostHttpError) -> Box<EvalAltResult> {
    let mut map = Map::new();
    match e {
        HostHttpError::RateLimited { retry_after } => {
            map.insert("kind".into(), "rate_limited".into());
            if let Some(ra) = retry_after {
                map.insert("retry_after".into(), (ra as i64).into());
            }
            map.insert("message".into(), "rate limit exceeded".into());
        }
        HostHttpError::Api(msg) => {
            map.insert("kind".into(), "api".into());
            map.insert("message".into(), msg.into());
        }
        HostHttpError::Transport(msg) => {
            map.insert("kind".into(), "transport".into());
            map.insert("message".into(), msg.into());
        }
    }
    Box::new(EvalAltResult::ErrorRuntime(
        Dynamic::from_map(map),
        Position::NONE,
    ))
}

/// Build a plain Rhai runtime exception from a message.
pub fn runtime_error(msg: impl Into<String>) -> Box<EvalAltResult> {
    Box::new(EvalAltResult::ErrorRuntime(
        msg.into().into(),
        Position::NONE,
    ))
}

/// Map a Rhai evaluation error into a [`ProviderError`], preserving the transient
/// vs permanent classification that the runtime's retry logic relies on.
///
/// A script `throw #{ message, transient, kind }` and a host-function error both
/// surface as `ErrorRuntime` carrying a map; a bare `throw "text"` surfaces as a
/// string. `kind` (when present) wins; otherwise `transient` selects
/// `RequestFailed` vs `Other`.
/// A [`FailureKind`] by the label a script writes.
///
/// Matched against the labels the built-in providers use, so a script and a
/// native provider describing the same failure describe it the same way. An
/// unknown name is ignored rather than rejected: a script written against a
/// later build must not fail on this build because it named a kind that does
/// not exist here yet.
fn named_failure_kind(name: &str) -> Option<crate::provider::FailureKind> {
    use crate::provider::FailureKind;
    [
        FailureKind::DnsFailure,
        FailureKind::ConnectionRefused,
        FailureKind::TlsFailure,
        FailureKind::Timeout,
        FailureKind::ConnectionDropped,
        FailureKind::Transport,
        FailureKind::BadRequest,
        FailureKind::NotFound,
        FailureKind::ServerError,
        FailureKind::MalformedResponse,
    ]
    .into_iter()
    .find(|k| k.label() == name)
}

pub fn map_rhai_err(err: Box<EvalAltResult>) -> ProviderError {
    if let EvalAltResult::ErrorRuntime(val, _) = &*err {
        if let Some(map) = val.clone().try_cast::<Map>() {
            let get_str = |k: &str| {
                map.get(k)
                    .and_then(|d| d.clone().into_string().ok())
                    .filter(|s| !s.is_empty())
            };
            let kind = get_str("kind");
            let message = get_str("message").unwrap_or_else(|| "provider script error".to_string());
            // A script may say what actually went wrong, which is the half the
            // caller cannot work out for itself: `kind` says how to *treat* the
            // failure - retry, fail over, give up - while `failure_kind` says
            // what it *was*. A script talking to its own endpoint knows the
            // difference between a refused connection and a rejected key, and
            // without this it could only fold both into "transport" or "api".
            //
            // Prefixed onto the message rather than held beside it, the same way
            // a built-in provider carries it, so both arrive at the log and the
            // API through one channel.
            let message = match get_str("failure_kind").and_then(|name| named_failure_kind(&name)) {
                Some(kind) if !message.starts_with('[') => {
                    format!("[{}] {message} - {}", kind.label(), kind.remedy())
                }
                _ => message,
            };
            let transient = map
                .get("transient")
                .and_then(|d| d.as_bool().ok())
                .unwrap_or(false);
            return match kind.as_deref() {
                // A script reports the kind and nothing else, so there is no
                // `Retry-After` to carry; the retry loop falls back to its own
                // capacity backoff.
                Some("rate_limited") => ProviderError::RateLimitExceeded {
                    retry_after_secs: None,
                },
                Some("transport") | Some("server") => ProviderError::RequestFailed(message),
                // A script's `api` error is the same shape a built-in provider
                // gets back from an HTTP call, so it classifies the same way:
                // an OpenAI-compatible endpoint answering 402 through a Rhai
                // provider must fail over and trip the breaker exactly as the
                // native OpenRouter provider does. The script has
                // no status code to hand us separately, so it is read back out
                // of the message.
                Some("api") => match crate::provider::UnavailableReason::from_message(&message) {
                    Some(reason) => ProviderError::Unavailable {
                        reason,
                        detail: message,
                    },
                    None => ProviderError::ApiError(message),
                },
                Some("invalid_response") => ProviderError::InvalidResponse(message),
                _ if transient => ProviderError::RequestFailed(message),
                _ => ProviderError::Other(message),
            };
        }
        return ProviderError::Other(val.to_string());
    }
    ProviderError::Other(err.to_string())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod cost_tests {
    use super::*;

    /// A script provider knows what its own endpoint charged; nothing else
    /// does, since a custom model has no rate card here. Both spellings are
    /// accepted because a script author will reach for either.
    #[test]
    fn a_script_provider_can_report_its_own_cost() {
        for key in ["cost_usd", "cost"] {
            let usage = serde_json::json!({
                "prompt_tokens": 12, "completion_tokens": 4, key: 0.0021
            });
            let parsed = parse_usage(Some(&usage));
            assert_eq!(parsed.reported_cost_usd, Some(0.0021), "via `{key}`");
        }
    }

    /// Silence means unpriced, not free.
    #[test]
    fn a_script_provider_that_says_nothing_leaves_the_call_unpriced() {
        let usage = serde_json::json!({"prompt_tokens": 12, "completion_tokens": 4});
        assert_eq!(parse_usage(Some(&usage)).reported_cost_usd, None);
    }
}

#[cfg(test)]
mod failure_kind_tests {
    use super::*;
    use crate::provider::FailureKind;

    fn thrown(map: Map) -> ProviderError {
        map_rhai_err(Box::new(EvalAltResult::ErrorRuntime(
            rhai::Dynamic::from_map(map),
            rhai::Position::NONE,
        )))
    }

    /// A script that knows what went wrong can say so, and it arrives the same
    /// way a built-in provider's does.
    #[test]
    fn a_script_can_name_what_actually_went_wrong() {
        let mut map = Map::new();
        map.insert("kind".into(), "transport".into());
        map.insert("failure_kind".into(), "connection-refused".into());
        map.insert("message".into(), "could not reach my box".into());

        let err = thrown(map);
        assert_eq!(err.failure_kind(), Some(FailureKind::ConnectionRefused));
        let text = err.to_string();
        assert!(text.contains("could not reach my box"), "{text}");
        assert!(text.contains("check the provider is running"), "{text}");
    }

    /// `kind` and `failure_kind` answer different questions and both are kept:
    /// one decides how the runtime treats the failure, the other says what it
    /// was. A `failure_kind` must not quietly change the first.
    #[test]
    fn naming_the_failure_does_not_change_how_it_is_treated() {
        let mut map = Map::new();
        map.insert("kind".into(), "api".into());
        map.insert("failure_kind".into(), "server-error".into());
        map.insert("message".into(), "HTTP 402 out of credits".into());

        // Still `Unavailable`, because `api` + a 402 in the message is what
        // decides failover - the label rides along without disturbing it.
        // Asserted through `unavailable_reason`, which is what actually decides
        // failover, rather than on the variant's shape: a `matches!` inside an
        // assertion that always passes leaves its other arm uncovered, and the
        // reason is the more useful claim anyway.
        assert_eq!(
            thrown(map).unavailable_reason(),
            Some(crate::provider::UnavailableReason::CreditsExhausted),
            "the label must not disturb how the failure is treated"
        );
    }

    /// A script written against a later build must not fail on this one for
    /// naming a kind this build has never heard of.
    #[test]
    fn an_unknown_failure_kind_is_ignored_rather_than_refused() {
        let mut map = Map::new();
        map.insert("kind".into(), "transport".into());
        map.insert("failure_kind".into(), "quantum-decoherence".into());
        map.insert("message".into(), "something odd".into());

        let err = thrown(map);
        assert_eq!(err.failure_kind(), None);
        assert!(err.to_string().contains("something odd"));
    }

    /// A script that says nothing gets what it always got.
    #[test]
    fn a_script_that_names_no_failure_kind_is_unchanged() {
        let mut map = Map::new();
        map.insert("kind".into(), "transport".into());
        map.insert("message".into(), "plain old failure".into());

        let err = thrown(map);
        assert_eq!(err.failure_kind(), None);
        assert!(err.to_string().contains("plain old failure"));
    }

    /// Every label a built-in provider can produce is one a script may name, so
    /// the two vocabularies cannot drift apart.
    #[test]
    fn every_builtin_label_is_nameable_from_a_script() {
        for kind in [
            FailureKind::DnsFailure,
            FailureKind::ConnectionRefused,
            FailureKind::TlsFailure,
            FailureKind::Timeout,
            FailureKind::ConnectionDropped,
            FailureKind::Transport,
            FailureKind::BadRequest,
            FailureKind::NotFound,
            FailureKind::ServerError,
            FailureKind::MalformedResponse,
        ] {
            let label = kind.label();
            assert_eq!(
                named_failure_kind(label),
                Some(kind),
                "{label} is not nameable"
            );
        }
    }
}
