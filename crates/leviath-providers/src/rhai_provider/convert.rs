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
    TokenUsage {
        prompt_tokens: usize_field(obj, "prompt_tokens"),
        completion_tokens: usize_field(obj, "completion_tokens"),
        total_tokens: usize_field(obj, "total_tokens"),
        cached_tokens: usize_field(obj, "cached_tokens"),
        cache_write_tokens: usize_field(obj, "cache_write_tokens"),
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
            let transient = map
                .get("transient")
                .and_then(|d| d.as_bool().ok())
                .unwrap_or(false);
            return match kind.as_deref() {
                Some("rate_limited") => ProviderError::RateLimitExceeded,
                Some("transport") | Some("server") => ProviderError::RequestFailed(message),
                Some("api") => ProviderError::ApiError(message),
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
mod tests {
    use super::*;

    fn dyn_from_json(s: &str) -> Dynamic {
        let v: Value = serde_json::from_str(s).unwrap();
        rhai::serde::to_dynamic(v).unwrap()
    }

    #[test]
    fn finish_reasons() {
        assert_eq!(
            finish_reason_from_str(Some("tool_calls")),
            FinishReason::ToolCall
        );
        assert_eq!(
            finish_reason_from_str(Some("ToolCall")),
            FinishReason::ToolCall
        );
        assert_eq!(
            finish_reason_from_str(Some("length")),
            FinishReason::TokenLimit
        );
        assert_eq!(
            finish_reason_from_str(Some("stop_sequence")),
            FinishReason::Stop
        );
        assert_eq!(finish_reason_from_str(Some("stop")), FinishReason::Complete);
        assert_eq!(finish_reason_from_str(None), FinishReason::Complete);
    }

    #[test]
    fn parse_inference_full() {
        let d = dyn_from_json(
            r#"{"content":"hi","tool_calls":[{"id":"t1","name":"f","arguments":{"a":1}}],
                "tokens_used":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5,"cached_tokens":1},
                "finish_reason":"tool_calls"}"#,
        );
        let r = parse_inference_dynamic(d).unwrap();
        assert_eq!(r.content, "hi");
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0].id, "t1");
        assert_eq!(r.tool_calls[0].arguments["a"], 1);
        assert_eq!(r.tokens_used.total_tokens, 5);
        assert_eq!(r.tokens_used.cached_tokens, 1);
        assert_eq!(r.finish_reason, FinishReason::ToolCall);
    }

    #[test]
    fn parse_inference_defaults_and_missing_fields() {
        let r = parse_inference_dynamic(dyn_from_json("{}")).unwrap();
        assert_eq!(r.content, "");
        assert!(r.tool_calls.is_empty());
        assert_eq!(r.tokens_used.total_tokens, 0);
        assert_eq!(r.finish_reason, FinishReason::Complete);
    }

    #[test]
    fn parse_inference_rejects_non_map() {
        let err = parse_inference_dynamic(dyn_from_json("[1,2,3]")).unwrap_err();
        assert!(matches!(err, ProviderError::InvalidResponse(_)));
    }

    #[test]
    fn chunk_delta_only() {
        let c = chunk_from_dynamic(dyn_from_json(r#"{"delta":"tok"}"#)).unwrap();
        assert_eq!(c.delta, "tok");
        assert!(c.tokens.is_none());
        assert!(c.finish_reason.is_none());
        assert!(c.tool_calls.is_empty());
    }

    #[test]
    fn chunk_with_tools_tokens_finish() {
        let c = chunk_from_dynamic(dyn_from_json(
            r#"{"tool_calls":[{"index":2,"id":"x","name":"f","arguments_delta":"{\"a\":"}],
                "tokens":{"total_tokens":9},"finish_reason":"stop"}"#,
        ))
        .unwrap();
        assert_eq!(c.tool_calls[0].index, 2);
        assert_eq!(c.tool_calls[0].id.as_deref(), Some("x"));
        assert_eq!(c.tool_calls[0].arguments_delta, "{\"a\":");
        assert_eq!(c.tokens.unwrap().total_tokens, 9);
        assert_eq!(c.finish_reason, Some(FinishReason::Complete));
    }

    #[test]
    fn map_err_kinds() {
        let mk = |k: &str, extra: &[(&str, Dynamic)]| {
            let mut m = Map::new();
            m.insert("kind".into(), k.into());
            m.insert("message".into(), "boom".into());
            for (kk, vv) in extra {
                m.insert((*kk).into(), vv.clone());
            }
            Box::new(EvalAltResult::ErrorRuntime(
                Dynamic::from_map(m),
                Position::NONE,
            ))
        };
        assert!(matches!(
            map_rhai_err(mk("rate_limited", &[])),
            ProviderError::RateLimitExceeded
        ));
        assert!(matches!(
            map_rhai_err(mk("transport", &[])),
            ProviderError::RequestFailed(_)
        ));
        assert!(matches!(
            map_rhai_err(mk("server", &[])),
            ProviderError::RequestFailed(_)
        ));
        assert!(matches!(
            map_rhai_err(mk("api", &[])),
            ProviderError::ApiError(_)
        ));
        assert!(matches!(
            map_rhai_err(mk("invalid_response", &[])),
            ProviderError::InvalidResponse(_)
        ));
        assert!(matches!(
            map_rhai_err(mk("weird", &[])),
            ProviderError::Other(_)
        ));
    }

    #[test]
    fn map_err_transient_flag_without_kind() {
        let mut m = Map::new();
        m.insert("message".into(), "temporarily down".into());
        m.insert("transient".into(), true.into());
        let e = map_rhai_err(Box::new(EvalAltResult::ErrorRuntime(
            Dynamic::from_map(m),
            Position::NONE,
        )));
        assert!(matches!(e, ProviderError::RequestFailed(m) if m == "temporarily down"));
    }

    #[test]
    fn map_err_plain_string_and_non_runtime() {
        let s = map_rhai_err(Box::new(EvalAltResult::ErrorRuntime(
            "oops".into(),
            Position::NONE,
        )));
        assert!(matches!(s, ProviderError::Other(_)));
        let other = map_rhai_err(Box::new(EvalAltResult::ErrorSystem(
            "sys".to_string(),
            "x".into(),
        )));
        assert!(matches!(other, ProviderError::Other(_)));
    }

    #[test]
    fn host_err_maps_round_trip() {
        // RateLimited → rate_limited kind → RateLimitExceeded.
        let e = host_err_to_rhai(HostHttpError::RateLimited {
            retry_after: Some(5),
        });
        assert!(matches!(map_rhai_err(e), ProviderError::RateLimitExceeded));
        let e = host_err_to_rhai(HostHttpError::Api("HTTP 500: x".to_string()));
        assert!(matches!(map_rhai_err(e), ProviderError::ApiError(_)));
        let e = host_err_to_rhai(HostHttpError::Transport("reset".to_string()));
        assert!(matches!(map_rhai_err(e), ProviderError::RequestFailed(_)));
    }

    #[test]
    fn runtime_error_builds_runtime_variant() {
        let e = runtime_error("nope");
        assert!(matches!(&*e, EvalAltResult::ErrorRuntime(_, _)));
    }
}
