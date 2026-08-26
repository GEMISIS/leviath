//! Tests for the parent module. The standard child-module file name
//! (`tests.rs`) keeps this scaffolding outside the coverage report:
//! cargo-llvm-cov's default ignore regex excludes `tests.rs` and
//! `*_tests.rs` files, so only production code answers to the 100% gate.
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
fn parse_inference_passes_a_thought_signature_through() {
    // A script wrapping a provider that issues per-call replay tokens
    // (Gemini's `thought_signature`) can hand one back; absent stays None.
    let d = dyn_from_json(
        r#"{"content":"","tool_calls":[
            {"id":"t1","name":"f","arguments":{},"thought_signature":"sig"},
            {"id":"t2","name":"g","arguments":{}}],
            "finish_reason":"tool_calls"}"#,
    );
    let r = parse_inference_dynamic(d).unwrap();
    assert_eq!(r.tool_calls[0].thought_signature.as_deref(), Some("sig"));
    assert_eq!(r.tool_calls[1].thought_signature, None);
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
    assert!(
        c.tool_calls[0].thought_signature.is_none(),
        "a script that says nothing about signatures is not made to invent one"
    );
}

/// A script fronting an endpoint that signs its tool calls passes the signature
/// through, because the model it is fronting will demand it back on the next
/// turn and only the script knows it is there.
#[test]
fn chunk_carries_a_thought_signature_a_script_supplied() {
    let c = chunk_from_dynamic(dyn_from_json(
        r#"{"tool_calls":[{"index":0,"id":"x","name":"f","arguments_delta":"{}",
                "thought_signature":"sig-abc"}]}"#,
    ))
    .unwrap();
    assert_eq!(
        c.tool_calls[0].thought_signature.as_deref(),
        Some("sig-abc")
    );
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
        ProviderError::RateLimitExceeded { .. }
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

/// A script talking to an OpenAI-compatible endpoint must fail over and trip
/// the breaker exactly as a built-in provider does (issue #201). It throws one
/// formatted string, so the classification has to come out of the message.
#[test]
fn a_scripts_payment_error_classifies_like_a_built_in_providers() {
    let api = |message: &str| {
        let mut m = Map::new();
        m.insert("kind".into(), "api".into());
        m.insert("message".into(), message.into());
        map_rhai_err(Box::new(EvalAltResult::ErrorRuntime(
            Dynamic::from_map(m),
            Position::NONE,
        )))
    };

    let err = api("HTTP 402 Payment Required: {\"error\":{\"message\":\"no credits\"}}");
    assert_eq!(
        err.unavailable_reason(),
        Some(crate::provider::UnavailableReason::CreditsExhausted)
    );
    assert!(!err.is_transient());

    assert_eq!(
        api("HTTP 401: bad key").unavailable_reason(),
        Some(crate::provider::UnavailableReason::AuthFailed)
    );
    // An ordinary API error is still an ordinary API error.
    let plain = api("HTTP 400: unknown field `foo`");
    assert_eq!(plain.unavailable_reason(), None);
    assert!(matches!(plain, ProviderError::ApiError(_)));
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
    assert!(matches!(
        map_rhai_err(e),
        ProviderError::RateLimitExceeded { .. }
    ));
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
    assert!(matches!(
        map_rhai_err(e),
        ProviderError::RateLimitExceeded { .. }
    ));
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
