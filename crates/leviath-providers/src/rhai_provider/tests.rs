//! Integration tests for [`RhaiProvider`] driven by a fake [`HttpExecutor`] so
//! no socket is ever bound. Runs on the default current-thread tokio runtime -
//! the exact flavor the channel broker must survive.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

/// No credential-shaped variable is allowlisted - the default posture.
fn no_env_allowlist() -> Arc<Vec<String>> {
    Arc::new(Vec::new())
}

use tokio::sync::mpsc;
use tokio_stream::StreamExt;

use super::*;
use crate::provider::{FinishReason, InferenceRequest, RateLimitConfig};
use host::{EventResult, HostHttpError, HostRequest, HttpExecutor};

// ── Fake executor ────────────────────────────────────────────────────────────

#[derive(Default)]
struct FakeExecutor {
    /// Queued unary responses, consumed in order.
    responses: Mutex<VecDeque<EventResult>>,
    /// Queued SSE events for the next `execute_stream` call.
    stream_events: Mutex<VecDeque<EventResult>>,
    /// Every request the broker performed, for assertions.
    calls: Mutex<Vec<HostRequest>>,
}

impl FakeExecutor {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    fn with_responses(responses: Vec<EventResult>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into()),
            ..Default::default()
        })
    }
    fn with_stream(events: Vec<EventResult>) -> Arc<Self> {
        Arc::new(Self {
            stream_events: Mutex::new(events.into()),
            ..Default::default()
        })
    }
    fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

#[async_trait::async_trait]
impl HttpExecutor for FakeExecutor {
    async fn execute(&self, req: HostRequest) -> EventResult {
        self.calls.lock().unwrap().push(req);
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok("{}".to_string()))
    }
    async fn execute_stream(&self, req: HostRequest, events: mpsc::Sender<EventResult>) {
        self.calls.lock().unwrap().push(req);
        let queued: Vec<EventResult> = self.stream_events.lock().unwrap().drain(..).collect();
        for ev in queued {
            if events.send(ev).await.is_err() {
                return;
            }
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn request(model: &str) -> InferenceRequest {
    InferenceRequest {
        system: Vec::new(),
        messages: Vec::new(),
        model: model.to_string(),
        max_tokens: 100,
        temperature: 0.0,
        tools: Vec::new(),
        extra: serde_json::Value::Null,
        request_timeout_secs: None,
    }
}

fn build(src: &str, executor: Arc<FakeExecutor>) -> Result<RhaiProvider> {
    RhaiProvider::from_source(
        src,
        executor,
        ScriptProviderSettings {
            name: "test".to_string(),
            init_config: serde_json::json!({}),
            caps: HashMap::new(),
            rate_limit: None,
            request_timeout_secs: None,
            env_allowlist: no_env_allowlist(),
        },
    )
}

fn build_rl(
    src: &str,
    executor: Arc<FakeExecutor>,
    rate_limit: Option<RateLimitConfig>,
) -> RhaiProvider {
    RhaiProvider::from_source(
        src,
        executor,
        ScriptProviderSettings {
            name: "test".to_string(),
            init_config: serde_json::json!({}),
            caps: HashMap::new(),
            rate_limit,
            request_timeout_secs: None,
            env_allowlist: no_env_allowlist(),
        },
    )
    .unwrap()
}

const NOOP_INIT: &str = "fn initialize(config) { #{} }\n";

// ── Construction ─────────────────────────────────────────────────────────────

#[test]
fn from_source_compile_error() {
    let err = build("fn inference( { oops", FakeExecutor::new())
        .err()
        .unwrap();
    assert!(matches!(err, ProviderError::Other(m) if m.contains("compile")));
}

#[test]
fn from_source_initialize_throw() {
    let src = "fn initialize(config) { throw #{ message: \"no key\", transient: false }; }\n\
               fn inference(s, r) { #{} }";
    let err = build(src, FakeExecutor::new()).err().unwrap();
    assert!(matches!(err, ProviderError::Other(m) if m == "no key"));
}

#[test]
fn from_source_missing_initialize() {
    let err = build("fn inference(s, r) { #{} }", FakeExecutor::new())
        .err()
        .unwrap();
    // call_fn on a missing `initialize` surfaces as a runtime error.
    assert!(matches!(err, ProviderError::Other(_)));
}

#[test]
fn from_source_initialize_receives_config() {
    let src = "fn initialize(config) { #{ m: config.model } }\n\
               fn inference(s, r) { #{ content: s.m } }";
    let p = RhaiProvider::from_source(
        src,
        FakeExecutor::new(),
        ScriptProviderSettings {
            name: "test".into(),
            init_config: serde_json::json!({ "model": "cfg-model" }),
            caps: HashMap::new(),
            rate_limit: None,
            request_timeout_secs: None,
            env_allowlist: no_env_allowlist(),
        },
    )
    .unwrap();
    let out = tokio_block(p.infer(&request("ignored")));
    assert_eq!(out.unwrap().content, "cfg-model");
}

#[test]
fn from_script_reads_missing_file() {
    let err = RhaiProvider::from_script(
        std::path::Path::new("/no/such/provider.rhai"),
        std::sync::Arc::new(crate::rhai_provider::host::ReqwestExecutor::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        )),
        crate::rhai_provider::ScriptProviderSettings {
            name: "test".to_string(),
            init_config: serde_json::json!({}),
            caps: HashMap::new(),
            rate_limit: None,
            request_timeout_secs: None,
            env_allowlist: no_env_allowlist(),
        },
    )
    .err()
    .unwrap();
    assert!(matches!(err, ProviderError::Other(m) if m.contains("read provider script")));
}

#[test]
fn from_script_loads_real_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.rhai");
    std::fs::write(
        &path,
        format!("{NOOP_INIT}fn inference(s,r) {{ #{{ content: \"ok\" }} }}"),
    )
    .unwrap();
    let p = RhaiProvider::from_script(
        &path,
        std::sync::Arc::new(crate::rhai_provider::host::ReqwestExecutor::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        )),
        crate::rhai_provider::ScriptProviderSettings {
            name: "test".to_string(),
            init_config: serde_json::json!({}),
            caps: HashMap::new(),
            rate_limit: None,
            request_timeout_secs: None,
            env_allowlist: no_env_allowlist(),
        },
    )
    .unwrap();
    assert_eq!(p.name(), "test");
}

// ── infer ────────────────────────────────────────────────────────────────────

fn tokio_block<F: std::future::Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(fut)
}

#[tokio::test]
async fn infer_no_http() {
    let src = format!(
        "{NOOP_INIT}fn inference(state, request) {{ \
         #{{ content: \"hello\", \
            tokens_used: #{{ prompt_tokens: 3, completion_tokens: 2, total_tokens: 5 }}, \
            finish_reason: \"Complete\" }} }}"
    );
    let exec = FakeExecutor::new();
    let p = build(&src, exec.clone()).unwrap();
    let r = p.infer(&request("m")).await.unwrap();
    assert_eq!(r.content, "hello");
    assert_eq!(r.tokens_used.total_tokens, 5);
    assert_eq!(r.finish_reason, FinishReason::Complete);
    assert_eq!(exec.call_count(), 0);
}

#[tokio::test]
async fn infer_single_http_post() {
    let src = format!(
        "{NOOP_INIT}fn inference(state, request) {{ \
         let resp = parse_json(http_post(\"http://api/x\", to_json(#{{ model: request.model }}), \
            #{{ \"Authorization\": \"Bearer k\" }})); \
         #{{ content: resp.text, tokens_used: #{{ total_tokens: resp.n }} }} }}"
    );
    let exec = FakeExecutor::with_responses(vec![Ok("{\"text\":\"hi\",\"n\":7}".to_string())]);
    let p = build(&src, exec.clone()).unwrap();
    let r = p.infer(&request("gpt-x")).await.unwrap();
    assert_eq!(r.content, "hi");
    assert_eq!(r.tokens_used.total_tokens, 7);
    // The broker performed exactly one request with our header + body.
    let calls = exec.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].url, "http://api/x");
    assert_eq!(calls[0].headers.get("Authorization").unwrap(), "Bearer k");
    assert!(calls[0].body.as_ref().unwrap().contains("gpt-x"));
}

#[tokio::test]
async fn infer_uses_two_arg_http_overloads() {
    // http_get(url, headers) and http_post(url, body) - the shorter overloads.
    let src = format!(
        "{NOOP_INIT}fn inference(state, request) {{ \
         let a = parse_json(http_get(\"http://a\", #{{ \"H\": \"v\" }})); \
         let b = parse_json(http_post(\"http://b\", \"{{}}\")); \
         #{{ content: a.x + b.y }} }}"
    );
    let exec = FakeExecutor::with_responses(vec![
        Ok("{\"x\":\"1\"}".to_string()),
        Ok("{\"y\":\"2\"}".to_string()),
    ]);
    let p = build(&src, exec.clone()).unwrap();
    let r = p.infer(&request("m")).await.unwrap();
    assert_eq!(r.content, "12");
    let calls = exec.calls.lock().unwrap();
    assert_eq!(calls[0].headers.get("H").unwrap(), "v");
    assert!(calls[1].body.is_some());
}

#[tokio::test]
async fn infer_rate_limited_without_limiter() {
    // A 429 with no configured rate limiter still maps to RateLimitExceeded and
    // exercises the "no limiter" branch of the broker's rate-limit accounting.
    let src = format!(
        "{NOOP_INIT}fn inference(s, r) {{ let x = http_get(\"http://api/x\"); #{{ content: x }} }}"
    );
    let exec =
        FakeExecutor::with_responses(vec![Err(HostHttpError::RateLimited { retry_after: None })]);
    let p = build(&src, exec).unwrap();
    let err = p.infer(&request("m")).await.err().unwrap();
    assert!(matches!(err, ProviderError::RateLimitExceeded { .. }));
}

#[tokio::test]
async fn infer_multiple_http_calls() {
    let src = format!(
        "{NOOP_INIT}fn inference(state, request) {{ \
         let a = parse_json(http_get(\"http://api/a\")); \
         let b = parse_json(http_get(\"http://api/b\")); \
         #{{ content: a.v + b.v }} }}"
    );
    let exec = FakeExecutor::with_responses(vec![
        Ok("{\"v\":\"x\"}".to_string()),
        Ok("{\"v\":\"y\"}".to_string()),
    ]);
    let p = build(&src, exec.clone()).unwrap();
    let r = p.infer(&request("m")).await.unwrap();
    assert_eq!(r.content, "xy");
    assert_eq!(exec.call_count(), 2);
}

#[tokio::test]
async fn infer_script_throw_maps_transient() {
    let src = format!(
        "{NOOP_INIT}fn inference(s, r) {{ throw #{{ message: \"upstream down\", transient: true }}; }}"
    );
    let p = build(&src, FakeExecutor::new()).unwrap();
    let err = p.infer(&request("m")).await.err().unwrap();
    assert!(err.is_transient());
    assert!(matches!(err, ProviderError::RequestFailed(m) if m == "upstream down"));
}

#[tokio::test]
async fn infer_http_429_maps_to_rate_limit() {
    let src = format!(
        "{NOOP_INIT}fn inference(s, r) {{ let x = http_post(\"http://api/x\", \"{{}}\", #{{}}); #{{ content: x }} }}"
    );
    let exec = FakeExecutor::with_responses(vec![Err(HostHttpError::RateLimited {
        retry_after: Some(1),
    })]);
    let p = build_rl(
        &src,
        exec,
        Some(RateLimitConfig {
            requests_per_minute: 100,
            tokens_per_minute: 100_000,
        }),
    );
    let err = p.infer(&request("m")).await.err().unwrap();
    assert!(matches!(err, ProviderError::RateLimitExceeded { .. }));
}

#[tokio::test]
async fn infer_http_api_error_propagates() {
    let src = format!(
        "{NOOP_INIT}fn inference(s, r) {{ let x = http_get(\"http://api/x\"); #{{ content: x }} }}"
    );
    let exec = FakeExecutor::with_responses(vec![Err(HostHttpError::Api("HTTP 400: bad".into()))]);
    // With a rate limiter so the success/reset arms exist elsewhere; here the
    // Err(_) no-op arm of serve_job is exercised.
    let p = build_rl(
        &src,
        exec,
        Some(RateLimitConfig {
            requests_per_minute: 100,
            tokens_per_minute: 100_000,
        }),
    );
    let err = p.infer(&request("m")).await.err().unwrap();
    assert!(matches!(err, ProviderError::ApiError(_)));
}

#[tokio::test]
async fn infer_records_tokens_and_resets_backoff() {
    // A successful http call with a rate limiter drives serve_job's Ok arm
    // (reset_backoff) and the record_tokens path.
    let src = format!(
        "{NOOP_INIT}fn inference(s, r) {{ let x = parse_json(http_get(\"http://api/x\")); \
         #{{ content: \"ok\", tokens_used: #{{ total_tokens: x.n }} }} }}"
    );
    let exec = FakeExecutor::with_responses(vec![Ok("{\"n\":11}".to_string())]);
    let p = build_rl(
        &src,
        exec,
        Some(RateLimitConfig {
            requests_per_minute: 100,
            tokens_per_minute: 100_000,
        }),
    );
    let r = p.infer(&request("m")).await.unwrap();
    assert_eq!(r.tokens_used.total_tokens, 11);
}

#[tokio::test]
async fn infer_rejects_non_map_return() {
    let src = format!("{NOOP_INIT}fn inference(s, r) {{ [1, 2, 3] }}");
    let p = build(&src, FakeExecutor::new()).unwrap();
    let err = p.infer(&request("m")).await.err().unwrap();
    assert!(matches!(err, ProviderError::InvalidResponse(_)));
}

// ── capabilities / metadata ──────────────────────────────────────────────────

#[test]
fn capabilities_and_metadata() {
    let src = format!(
        "// @provider testp\n// @max_context_tokens 40000\n// @max_output_tokens 8000\n\
         // @default_model dm\n{NOOP_INIT}fn inference(s, r) {{ #{{}} }}"
    );
    let mut caps = HashMap::new();
    // Names one field, which is what a `[model_capabilities]` entry looks like.
    caps.insert(
        "special".to_string(),
        crate::provider::ModelCapabilityOverride {
            max_context_tokens: Some(999),
            ..Default::default()
        },
    );
    let p = RhaiProvider::from_source(
        &src,
        FakeExecutor::new(),
        ScriptProviderSettings {
            name: "test".into(),
            init_config: serde_json::json!({}),
            caps,
            rate_limit: None,
            request_timeout_secs: None,
            env_allowlist: no_env_allowlist(),
        },
    )
    .unwrap();

    assert_eq!(p.name(), "test");
    assert_eq!(p.meta().default_model.as_deref(), Some("dm"));
    // The named field wins, and the ones it did not name are the script's own
    // rather than `Default`'s - a partial entry corrects, it does not replace.
    assert_eq!(p.max_context_tokens("special"), 999);
    let special = p.capabilities("special");
    assert_eq!(special.max_context_tokens, 999);
    assert_eq!(
        special.max_output_tokens, 8000,
        "the script's own output cap should survive an entry that never mentioned it"
    );
    // metadata default for an unknown model
    assert_eq!(p.max_context_tokens("other"), 40000);
    let c = p.capabilities("other");
    assert_eq!(c.max_context_tokens, 40000);
    assert_eq!(c.max_output_tokens, 8000);
    assert!(c.supports_streaming);
}

// ── count_tokens ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn count_tokens_uses_heuristic_without_script_fn() {
    let src = format!("{NOOP_INIT}fn inference(s, r) {{ #{{}} }}");
    let p = build(&src, FakeExecutor::new()).unwrap();
    // "abcdefgh" (8 chars) with a gpt- model → tiktoken; just assert it's > 0.
    assert!(p.count_tokens("abcdefgh", "gpt-4").await > 0);
    // non-gpt model → /4 heuristic
    assert_eq!(p.count_tokens("abcd", "llama").await, 1);
}

#[tokio::test]
async fn count_tokens_uses_script_fn() {
    let src = format!(
        "{NOOP_INIT}fn inference(s, r) {{ #{{}} }}\n\
         fn count_tokens(state, text, model) {{ count_tokens_heuristic(text, \"general\") + 100 }}"
    );
    let p = build(&src, FakeExecutor::new()).unwrap();
    // heuristic(abcd)=1, +100 = 101
    assert_eq!(p.count_tokens("abcd", "m").await, 101);
}

#[tokio::test]
async fn count_tokens_script_non_int_falls_back() {
    let src = format!(
        "{NOOP_INIT}fn inference(s, r) {{ #{{}} }}\n\
         fn count_tokens(state, text, model) {{ \"not an int\" }}"
    );
    let p = build(&src, FakeExecutor::new()).unwrap();
    // Falls back to the /4 heuristic for a non-gpt model.
    assert_eq!(p.count_tokens("abcdefgh", "llama").await, 2);
}

// ── list_models ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn list_models_none_is_empty() {
    let src = format!("{NOOP_INIT}fn inference(s, r) {{ #{{}} }}");
    let p = build(&src, FakeExecutor::new()).unwrap();
    assert!(p.list_models().await.unwrap().is_empty());
}

#[tokio::test]
async fn list_models_parses_and_filters() {
    let src = format!(
        "{NOOP_INIT}fn inference(s, r) {{ #{{}} }}\n\
         fn list_models(state) {{ [ \
           #{{ id: \"m1\", display_name: \"Model One\", max_context_tokens: 1000, max_output_tokens: 256 }}, \
           #{{ nope: 1 }} ] }}"
    );
    let p = build(&src, FakeExecutor::new()).unwrap();
    let models = p.list_models().await.unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "m1");
    assert_eq!(models[0].display_name.as_deref(), Some("Model One"));
    assert_eq!(models[0].provider, "test");
    assert_eq!(models[0].capabilities.max_context_tokens, 1000);
    assert_eq!(models[0].capabilities.max_output_tokens, 256);
}

#[tokio::test]
async fn list_models_non_array_is_empty() {
    let src = format!(
        "{NOOP_INIT}fn inference(s, r) {{ #{{}} }}\n\
         fn list_models(state) {{ #{{ not: \"an array\" }} }}"
    );
    let p = build(&src, FakeExecutor::new()).unwrap();
    assert!(p.list_models().await.unwrap().is_empty());
}

#[tokio::test]
async fn list_models_unconvertible_is_empty() {
    // Returning a value that can't convert to JSON (a function pointer) hits the
    // from_dynamic error arm of parse_models → empty list.
    let src = format!(
        "{NOOP_INIT}fn inference(s, r) {{ #{{}} }}\n\
         fn list_models(state) {{ [ |x| x ] }}"
    );
    let p = build(&src, FakeExecutor::new()).unwrap();
    assert!(p.list_models().await.unwrap().is_empty());
}

// ── streaming ────────────────────────────────────────────────────────────────

async fn collect_stream(
    p: &RhaiProvider,
    req: InferenceRequest,
) -> Vec<Result<crate::provider::StreamChunk>> {
    let mut s = p.infer_stream(&req).await.unwrap();
    let mut out = Vec::new();
    while let Some(item) = s.next().await {
        out.push(item);
    }
    out
}

const STREAM_SCRIPT: &str = "\
fn stream(state, request, on_chunk) {
    stream_request(\"http://api/stream\", \"{}\", #{}, |chunk| {
        let data = parse_sse(chunk);
        if data == () { return; }
        on_chunk.call(#{ delta: data.d });
    });
    on_chunk.call(#{ delta: \"\", finish_reason: \"Complete\", tokens: #{ total_tokens: 4 } });
}
";

#[tokio::test]
async fn native_stream_emits_chunks() {
    let src = format!("{NOOP_INIT}fn inference(s, r) {{ #{{ content: \"x\" }} }}\n{STREAM_SCRIPT}");
    let exec = FakeExecutor::with_stream(vec![
        Ok("{\"d\":\"a\"}".to_string()),
        Ok("{\"d\":\"b\"}".to_string()),
    ]);
    let p = build(&src, exec).unwrap();
    let chunks = collect_stream(&p, request("m")).await;
    let deltas: Vec<String> = chunks
        .iter()
        .map(|c| c.as_ref().unwrap().delta.clone())
        .collect();
    assert_eq!(deltas, vec!["a", "b", ""]);
    let last = chunks.last().unwrap().as_ref().unwrap();
    assert_eq!(last.finish_reason, Some(FinishReason::Complete));
    assert_eq!(last.tokens.as_ref().unwrap().total_tokens, 4);
}

#[tokio::test]
async fn native_stream_mid_stream_error() {
    let src = format!("{NOOP_INIT}fn inference(s, r) {{ #{{}} }}\n{STREAM_SCRIPT}");
    let exec = FakeExecutor::with_stream(vec![
        Ok("{\"d\":\"a\"}".to_string()),
        Err(HostHttpError::Transport("reset".into())),
    ]);
    let p = build(&src, exec).unwrap();
    let chunks = collect_stream(&p, request("m")).await;
    assert_eq!(chunks[0].as_ref().unwrap().delta, "a");
    assert!(chunks.iter().any(|c| c.is_err()));
}

#[tokio::test]
async fn native_stream_script_throw_becomes_error_item() {
    let src = format!(
        "{NOOP_INIT}fn inference(s, r) {{ #{{}} }}\n\
         fn stream(state, request, on_chunk) {{ throw #{{ message: \"boom\", transient: false }}; }}"
    );
    let p = build(&src, FakeExecutor::new()).unwrap();
    let chunks = collect_stream(&p, request("m")).await;
    assert_eq!(chunks.len(), 1);
    assert!(matches!(chunks[0], Err(ProviderError::Other(ref m)) if m == "boom"));
}

#[tokio::test]
async fn stream_fallback_collapses_infer() {
    // No `stream` fn → default path collapses infer() into one chunk.
    let src = format!(
        "{NOOP_INIT}fn inference(s, r) {{ #{{ content: \"whole\", \
         tool_calls: [ #{{ id: \"t\", name: \"f\", arguments: #{{ a: 1 }} }} ], \
         finish_reason: \"ToolCall\" }} }}"
    );
    let p = build(&src, FakeExecutor::new()).unwrap();
    let chunks = collect_stream(&p, request("m")).await;
    assert_eq!(chunks.len(), 1);
    let c = chunks[0].as_ref().unwrap();
    assert_eq!(c.delta, "whole");
    assert_eq!(c.tool_calls.len(), 1);
    assert_eq!(c.tool_calls[0].index, 0);
    assert_eq!(c.finish_reason, Some(FinishReason::ToolCall));
}

#[tokio::test]
async fn stream_fallback_propagates_infer_error() {
    let src = format!(
        "{NOOP_INIT}fn inference(s, r) {{ throw #{{ message: \"nope\", transient: false }}; }}"
    );
    let p = build(&src, FakeExecutor::new()).unwrap();
    let err = p.infer_stream(&request("m")).await.err().unwrap();
    assert!(matches!(err, ProviderError::Other(m) if m == "nope"));
}

// ── free-function coverage ───────────────────────────────────────────────────

#[tokio::test]
async fn task_failed_and_finalize_stream_arms() {
    // Produce a real JoinError from a panicking blocking task.
    let handle = tokio::task::spawn_blocking(|| panic!("kaboom"));
    let join_err = handle.await.err().unwrap();
    let e = task_failed(join_err);
    assert!(matches!(e, ProviderError::Other(m) if m.contains("task failed")));

    // finalize_stream: Ok(Ok) sends nothing; Ok(Err) and Err(join) each send one.
    let (tx, mut rx) = mpsc::unbounded_channel();
    finalize_stream(Ok(Ok(())), &tx);
    finalize_stream(Ok(Err(ProviderError::Other("x".into()))), &tx);
    let handle2 = tokio::task::spawn_blocking(|| panic!("again"));
    let je2 = handle2.await.err().unwrap();
    finalize_stream(Err(je2), &tx);
    drop(tx);
    let mut errs = 0;
    while let Some(item) = rx.recv().await {
        assert!(item.is_err());
        errs += 1;
    }
    assert_eq!(errs, 2);
}

#[tokio::test]
async fn native_stream_callback_throw_becomes_error_item() {
    // The per-event closure throws → stream_request's callback dispatch errors
    // and propagates out of stream(), surfacing as a terminal error item.
    let src = format!(
        "{NOOP_INIT}fn inference(s, r) {{ #{{}} }}\n\
         fn stream(state, request, on_chunk) {{ \
           stream_request(\"http://x\", \"{{}}\", #{{}}, |chunk| {{ throw \"callback boom\"; }}); }}"
    );
    let exec = FakeExecutor::with_stream(vec![Ok("{\"d\":\"a\"}".to_string())]);
    let p = build(&src, exec).unwrap();
    let chunks = collect_stream(&p, request("m")).await;
    assert!(chunks.iter().any(|c| c.is_err()));
}

#[tokio::test]
async fn count_tokens_script_throw_falls_back() {
    // A throwing count_tokens makes dispatch return Err (the `.await?` path);
    // count_tokens then falls back to the heuristic.
    let src = format!(
        "{NOOP_INIT}fn inference(s, r) {{ #{{}} }}\n\
         fn count_tokens(state, text, model) {{ throw #{{ message: \"boom\", transient: false }}; }}"
    );
    let p = build(&src, FakeExecutor::new()).unwrap();
    assert_eq!(p.count_tokens("abcdefgh", "llama").await, 2);
}

#[tokio::test]
async fn dispatch_maps_task_panic_to_error() {
    // A panic inside the blocking script task surfaces as a JoinError, which
    // dispatch maps to ProviderError::Other (the task_failed path).
    let src = format!("{NOOP_INIT}fn inference(s, r) {{ #{{}} }}");
    let p = build(&src, FakeExecutor::new()).unwrap();
    let r = p
        .dispatch(
            None,
            Box::new(|_engine: &rhai::Engine| -> Result<rhai::Dynamic> {
                panic!("boom in blocking task")
            }),
        )
        .await;
    assert!(matches!(r, Err(ProviderError::Other(m)) if m.contains("task failed")));
}

#[test]
fn sample_groq_script_compiles_and_initializes() {
    // The documented example must actually load (compile + offline initialize)
    // and advertise its optional functions.
    let src = include_str!("../../../../docs/examples/groq.rhai");
    let p = RhaiProvider::from_source(
        src,
        FakeExecutor::new(),
        ScriptProviderSettings {
            name: "groq".to_string(),
            init_config: serde_json::json!({ "api_key": "test-key" }),
            caps: HashMap::new(),
            rate_limit: None,
            request_timeout_secs: None,
            env_allowlist: no_env_allowlist(),
        },
    )
    .unwrap();
    assert_eq!(p.meta().provider.as_deref(), Some("groq"));
    assert_eq!(p.max_context_tokens("any"), 131072);
    assert!(p.has_stream && p.has_count_tokens && p.has_list_models);
}

#[test]
fn request_to_dynamic_round_trips() {
    let d = request_to_dynamic(&request("mymodel"));
    let json: serde_json::Value = rhai::serde::from_dynamic(&d).unwrap();
    assert_eq!(json["model"], "mymodel");
}

#[test]
fn tokio_block_helper_used() {
    // Exercise the sync helper (also proves broker works when driven via a
    // freshly-built current-thread runtime).
    let src = format!("{NOOP_INIT}fn inference(s, r) {{ #{{ content: \"z\" }} }}");
    let p = build(&src, FakeExecutor::new()).unwrap();
    let out = tokio_block(p.infer(&request("m"))).unwrap();
    assert_eq!(out.content, "z");
}

/// A script provider sees what each system block is and how much it moves, so it
/// can build its own cache scheme rather than being handed Anthropic's.
///
/// This is the whole reason the ordering policy lives in the Anthropic provider
/// and the *facts* live on the block: a script talking to some other API has a
/// different cache shape - different marker count, different minimum, maybe
/// none at all - and needs the same inputs to make its own decision.
#[test]
fn a_script_sees_each_system_block_region_and_volatility() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.rhai");
    // Echo back what the script can see, so a change to the wire shape fails
    // here rather than silently removing a capability script authors rely on.
    std::fs::write(
        &path,
        format!(
            "{NOOP_INIT}fn inference(s, r) {{ \
                let seen = \"\"; \
                for b in r.system {{ \
                    if seen != \"\" {{ seen += \",\"; }} \
                    seen += b.region + \":\" + b.volatility; \
                }} \
                #{{ content: seen }} \
            }}"
        ),
    )
    .unwrap();
    let p = RhaiProvider::from_script(
        &path,
        std::sync::Arc::new(crate::rhai_provider::host::ReqwestExecutor::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        )),
        crate::rhai_provider::ScriptProviderSettings {
            name: "test".to_string(),
            init_config: serde_json::json!({}),
            caps: HashMap::new(),
            rate_limit: None,
            request_timeout_secs: None,
            env_allowlist: no_env_allowlist(),
        },
    )
    .unwrap();

    let request = InferenceRequest {
        system: vec![
            crate::provider::SystemBlock {
                text: "the task".to_string(),
                cache_hint: leviath_core::CacheHint::Always,
                volatility: leviath_core::Volatility::Stable,
                region: "task".to_string(),
            },
            crate::provider::SystemBlock {
                text: "findings so far".to_string(),
                cache_hint: leviath_core::CacheHint::Always,
                volatility: leviath_core::Volatility::Grows,
                region: "findings".to_string(),
            },
        ],
        messages: vec![crate::provider::Message {
            role: "user".to_string(),
            content: "hello".into(),
            cache_breakpoint: false,
        }],
        model: "m".to_string(),
        max_tokens: 16,
        temperature: 0.0,
        tools: vec![],
        extra: serde_json::Value::Null,
        request_timeout_secs: None,
    };

    let response = tokio_block(p.infer(&request)).expect("the script runs");
    assert_eq!(response.content, "task:stable,findings:grows");
}

// ── check_source ─────────────────────────────────────────────────────────────

/// The smallest script that is a usable provider.
const GOOD_PROVIDER: &str = "// @provider mock\n\
                             // @description a mock\n\
                             // @default_model mock-1\n\
                             fn initialize(config) { #{} }\n\
                             fn inference(state, request) { #{ content: \"hi\" } }";

/// The `Other` message a shape or compile failure carries.
fn refusal(err: ProviderError) -> String {
    let ProviderError::Other(message) = err else {
        unreachable!("a compile or shape failure is Other")
    };
    message
}

#[test]
fn check_source_accepts_a_provider_and_returns_its_annotations() {
    let meta = check_source("mock.rhai", GOOD_PROVIDER).expect("a usable provider");
    assert_eq!(meta.provider.as_deref(), Some("mock"));
    assert_eq!(meta.description, "a mock");
    assert_eq!(meta.default_model.as_deref(), Some("mock-1"));
}

#[test]
fn check_source_reports_a_syntax_error() {
    let message = refusal(check_source("mock.rhai", "fn inference( { oops").unwrap_err());
    assert!(
        message.contains("compile provider script mock.rhai"),
        "{message}"
    );
}

/// The gap this exists to close: a script with no `inference` used to compile,
/// initialize and cache, then fail at the first real inference.
#[test]
fn check_source_rejects_a_script_with_no_inference() {
    let message = refusal(check_source("mock.rhai", "fn initialize(config) { #{} }").unwrap_err());
    assert!(
        message.contains("must define fn inference(state, request)"),
        "{message}"
    );
}

#[test]
fn check_source_rejects_a_script_with_no_initialize() {
    let message =
        refusal(check_source("mock.rhai", "fn inference(state, request) { #{} }").unwrap_err());
    assert!(
        message.contains("must define fn initialize(config)"),
        "{message}"
    );
}

/// A function of the right name and the wrong shape is the likelier typo, and
/// "not defined" would be a lie about a name that is right there in the file.
#[test]
fn check_source_names_the_arity_a_wrong_entry_point_has() {
    let message = refusal(
        check_source(
            "mock.rhai",
            "fn initialize(config) { #{} }\nfn inference(state) { #{} }",
        )
        .unwrap_err(),
    );
    assert!(
        message.contains("fn inference must take 2 parameters (state, request), found 1"),
        "{message}"
    );
}

/// `check_source` compiles and introspects; it must not run the script, or the
/// ungated validate route would be executing whatever was posted to it.
#[test]
fn check_source_does_not_run_initialize() {
    let src = "fn initialize(config) { throw \"initialize ran\"; }\n\
               fn inference(state, request) { #{} }";
    check_source("mock.rhai", src).expect("the throw is never reached");
}

/// The loader agrees with the route: what `check_source` refuses, a load
/// refuses too, so a script the API accepts is one a run will accept.
#[test]
fn from_source_rejects_a_script_with_no_inference() {
    let message = refusal(
        build("fn initialize(config) { #{} }", FakeExecutor::new())
            .err()
            .expect("inference is required"),
    );
    assert!(
        message.contains("fn inference(state, request)"),
        "{message}"
    );
}
