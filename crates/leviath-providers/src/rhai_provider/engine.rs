//! Rhai engine construction for provider scripts: sandbox hardening, the pure
//! helper functions, and the channel-backed HTTP host functions.

use std::collections::BTreeMap;

use base64::Engine as _;
use rhai::{Dynamic, Engine, EvalAltResult, FnPtr, Map, NativeCallContext};
use tokio::sync::{mpsc, oneshot};

use crate::provider::StreamChunk;

use super::convert::{chunk_from_dynamic, host_err_to_rhai, runtime_error};
use super::host::{BrokerJob, HostRequest, HttpJob, HttpMethod, StreamHttpJob};

/// Max Rhai operations per script call — the only wall-clock bound on a pure
/// (non-I/O) runaway loop; HTTP is bounded separately by the request timeout.
const MAX_OPERATIONS: u64 = 500_000;

/// Build a sandboxed engine with the pure Leviath helper functions registered
/// but **no** network host functions. Used to run `initialize` offline and as
/// the base for the per-call execution engine.
pub fn build_init_engine() -> Engine {
    let mut engine = Engine::new();
    engine.set_max_operations(MAX_OPERATIONS);
    engine.set_max_string_size(2_000_000);
    engine.set_max_array_size(50_000);
    engine.set_max_map_size(50_000);
    // Explicit, generous expression-nesting depth. Rhai's default is much lower
    // in debug builds (stack-overflow guard for unoptimized code), which would
    // otherwise reject legitimate provider scripts under `cargo test`.
    engine.set_max_expr_depths(128, 128);
    engine.on_print(|_| {});
    engine.on_debug(|_, _, _| {});
    register_pure_fns(&mut engine);
    engine
}

/// Configuration for the per-call execution engine.
pub struct ExecConfig {
    /// Channel the HTTP host functions push jobs onto for the async broker.
    pub jobs: mpsc::UnboundedSender<BrokerJob>,
    /// Effective per-request timeout (the stage's `request_timeout_secs`).
    pub timeout_secs: Option<u64>,
    /// When streaming, the sink `__emit_chunk` feeds parsed chunks into.
    pub chunk_tx: Option<mpsc::UnboundedSender<crate::Result<StreamChunk>>>,
}

/// Build the per-call execution engine: the hardened base plus `http_get`,
/// `http_post`, `stream_request`, and (when streaming) `__emit_chunk`.
pub fn build_exec_engine(cfg: ExecConfig) -> Engine {
    let mut engine = build_init_engine();
    register_http_fns(&mut engine, cfg.jobs.clone(), cfg.timeout_secs);
    register_stream_request(&mut engine, cfg.jobs, cfg.timeout_secs);
    if let Some(tx) = cfg.chunk_tx {
        register_emit_chunk(&mut engine, tx);
    }
    engine
}

// ─── Pure helpers ────────────────────────────────────────────────────────────

fn register_pure_fns(engine: &mut Engine) {
    engine.register_fn("parse_json", parse_json_fn);
    engine.register_fn("to_json", to_json_fn);
    engine.register_fn("parse_sse", parse_sse_fn);
    engine.register_fn("encode_uri", |s: &str| percent_encode(s));
    engine.register_fn("encode_base64", |s: &str| {
        base64::engine::general_purpose::STANDARD.encode(s)
    });
    engine.register_fn("env_var", env_var_fn);
    engine.register_fn("count_tokens_heuristic", count_tokens_heuristic_fn);
}

/// `parse_json(str)` → Rhai value (runtime error on invalid JSON).
fn parse_json_fn(s: &str) -> Result<Dynamic, Box<EvalAltResult>> {
    let value: serde_json::Value =
        serde_json::from_str(s).map_err(|e| runtime_error(format!("parse_json: {e}")))?;
    rhai::serde::to_dynamic(value)
}

/// `to_json(value)` → JSON string.
fn to_json_fn(v: Dynamic) -> Result<String, Box<EvalAltResult>> {
    let json: serde_json::Value = rhai::serde::from_dynamic(&v)?;
    Ok(json.to_string())
}

/// `parse_sse(chunk)` → parsed data map, or `()` for `[DONE]`/blank/unparsable.
/// Tolerates an optional leading `data:` prefix (the broker already strips it,
/// so this is belt-and-suspenders for scripts that pass raw lines).
fn parse_sse_fn(chunk: &str) -> Dynamic {
    let payload = chunk
        .trim()
        .strip_prefix("data:")
        .map(str::trim)
        .unwrap_or_else(|| chunk.trim());
    if payload.is_empty() || payload == "[DONE]" {
        return Dynamic::UNIT;
    }
    match serde_json::from_str::<serde_json::Value>(payload) {
        Ok(v) => rhai::serde::to_dynamic(v).unwrap_or(Dynamic::UNIT),
        Err(_) => Dynamic::UNIT,
    }
}

/// `env_var(name)` → the value string, or `()` when unset.
fn env_var_fn(name: &str) -> Dynamic {
    match std::env::var(name) {
        Ok(v) => Dynamic::from(v),
        Err(_) => Dynamic::UNIT,
    }
}

/// `count_tokens_heuristic(text, hint)` → token estimate. `hint` selects the
/// tokenizer family via a representative model prefix.
fn count_tokens_heuristic_fn(text: &str, hint: &str) -> i64 {
    let model = match hint {
        "openai" => "gpt-4",
        "anthropic" => "claude-3",
        "gemini" => "gemini-1.5",
        _ => "general",
    };
    crate::tokenizer::count_tokens(text, model) as i64
}

/// Percent-encode for a URL component (RFC 3986 unreserved set passes through).
fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for &byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                out.push('%');
                out.push(hex_digit(byte >> 4));
                out.push(hex_digit(byte & 0x0f));
            }
        }
    }
    out
}

/// Map a nibble (0–15) to its uppercase hex digit.
fn hex_digit(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'A' + (nibble - 10)) as char,
    }
}

/// Convert a Rhai object-map of headers into a `BTreeMap<String,String>`.
fn headers_from_map(map: Map) -> BTreeMap<String, String> {
    map.into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

// ─── HTTP host functions ─────────────────────────────────────────────────────

fn register_http_fns(
    engine: &mut Engine,
    jobs: mpsc::UnboundedSender<BrokerJob>,
    timeout_secs: Option<u64>,
) {
    let j = jobs.clone();
    engine.register_fn("http_get", move |url: &str| {
        unary(
            &j,
            HttpMethod::Get,
            url,
            None,
            BTreeMap::new(),
            timeout_secs,
        )
    });
    let j = jobs.clone();
    engine.register_fn("http_get", move |url: &str, headers: Map| {
        unary(
            &j,
            HttpMethod::Get,
            url,
            None,
            headers_from_map(headers),
            timeout_secs,
        )
    });
    let j = jobs.clone();
    engine.register_fn("http_post", move |url: &str, body: &str| {
        unary(
            &j,
            HttpMethod::Post,
            url,
            Some(body.to_string()),
            BTreeMap::new(),
            timeout_secs,
        )
    });
    let j = jobs;
    engine.register_fn("http_post", move |url: &str, body: &str, headers: Map| {
        unary(
            &j,
            HttpMethod::Post,
            url,
            Some(body.to_string()),
            headers_from_map(headers),
            timeout_secs,
        )
    });
}

/// Perform one unary HTTP job: hand it to the broker and block this
/// (blocking-pool) thread on the reply.
fn unary(
    jobs: &mpsc::UnboundedSender<BrokerJob>,
    method: HttpMethod,
    url: &str,
    body: Option<String>,
    headers: BTreeMap<String, String>,
    timeout_secs: Option<u64>,
) -> Result<String, Box<EvalAltResult>> {
    let (reply, rx) = oneshot::channel();
    let job = HttpJob {
        request: HostRequest {
            method,
            url: url.to_string(),
            body,
            headers,
            timeout_secs,
        },
        reply,
    };
    jobs.send(BrokerJob::Unary(job))
        .map_err(|_| runtime_error("provider host channel closed"))?;
    match rx.blocking_recv() {
        Ok(Ok(text)) => Ok(text),
        Ok(Err(e)) => Err(host_err_to_rhai(e)),
        Err(_) => Err(runtime_error("provider host dropped the reply")),
    }
}

fn register_stream_request(
    engine: &mut Engine,
    jobs: mpsc::UnboundedSender<BrokerJob>,
    timeout_secs: Option<u64>,
) {
    engine.register_fn(
        "stream_request",
        move |ctx: NativeCallContext,
              url: &str,
              body: &str,
              headers: Map,
              callback: FnPtr|
              -> Result<(), Box<EvalAltResult>> {
            let (ev_tx, mut ev_rx) = mpsc::channel::<crate::rhai_provider::host::EventResult>(64);
            let job = StreamHttpJob {
                request: HostRequest {
                    method: HttpMethod::Post,
                    url: url.to_string(),
                    body: Some(body.to_string()),
                    headers: headers_from_map(headers),
                    timeout_secs,
                },
                events: ev_tx,
            };
            jobs.send(BrokerJob::Stream(job))
                .map_err(|_| runtime_error("provider host channel closed"))?;
            loop {
                match ev_rx.blocking_recv() {
                    Some(Ok(payload)) => {
                        // Drive the script's per-event closure; ignore its return.
                        let _: Dynamic = callback.call_within_context(&ctx, (payload,))?;
                    }
                    Some(Err(e)) => return Err(host_err_to_rhai(e)),
                    None => return Ok(()),
                }
            }
        },
    );
}

fn register_emit_chunk(
    engine: &mut Engine,
    chunk_tx: mpsc::UnboundedSender<crate::Result<StreamChunk>>,
) {
    engine.register_fn("__emit_chunk", move |result: Dynamic| {
        let _ = chunk_tx.send(chunk_from_dynamic(result));
    });
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
