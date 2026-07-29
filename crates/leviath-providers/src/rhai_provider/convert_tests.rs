//! Tests for [`super::super::convert`]. Kept in a separate file so this test
//! code is not measured by the coverage gate (only production code must be
//! 100%), matching the crate's `tests.rs` convention.
use super::{
    chunk_from_dynamic, finish_reason_from_str, host_err_to_rhai, map_rhai_err,
    parse_inference_dynamic, runtime_error,
};
use crate::provider::{FinishReason, ProviderError};
use crate::rhai_provider::host::HostHttpError;
use rhai::{Dynamic, EvalAltResult, Map, Position};
use serde_json::Value;

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
    let mk = |k: &str| {
        let mut m = Map::new();
        m.insert("kind".into(), k.into());
        m.insert("message".into(), "boom".into());
        Box::new(EvalAltResult::ErrorRuntime(
            Dynamic::from_map(m),
            Position::NONE,
        ))
    };
    assert!(matches!(
        map_rhai_err(mk("rate_limited")),
        ProviderError::RateLimitExceeded
    ));
    assert!(matches!(
        map_rhai_err(mk("transport")),
        ProviderError::RequestFailed(_)
    ));
    assert!(matches!(
        map_rhai_err(mk("server")),
        ProviderError::RequestFailed(_)
    ));
    assert!(matches!(
        map_rhai_err(mk("api")),
        ProviderError::ApiError(_)
    ));
    assert!(matches!(
        map_rhai_err(mk("invalid_response")),
        ProviderError::InvalidResponse(_)
    ));
    assert!(matches!(map_rhai_err(mk("weird")), ProviderError::Other(_)));
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

#[test]
fn parse_inference_and_chunk_reject_unconvertible_dynamic() {
    // A function pointer has no serde_json representation, so `from_dynamic`
    // fails - exercising the InvalidResponse error arms.
    let fp = Dynamic::from(rhai::FnPtr::new("noop").unwrap());
    assert!(matches!(
        parse_inference_dynamic(fp.clone()).unwrap_err(),
        ProviderError::InvalidResponse(_)
    ));
    assert!(matches!(
        chunk_from_dynamic(fp).unwrap_err(),
        ProviderError::InvalidResponse(_)
    ));
}

#[test]
fn host_err_rate_limited_without_retry_after() {
    let e = host_err_to_rhai(HostHttpError::RateLimited { retry_after: None });
    assert!(matches!(map_rhai_err(e), ProviderError::RateLimitExceeded));
}

#[test]
fn map_err_defaults_message_when_absent() {
    // A throw map with a kind but no message falls to the default text.
    let mut m = Map::new();
    m.insert("transient".into(), true.into());
    let e = map_rhai_err(Box::new(EvalAltResult::ErrorRuntime(
        Dynamic::from_map(m),
        Position::NONE,
    )));
    assert!(matches!(e, ProviderError::RequestFailed(msg) if msg == "provider script error"));
}
