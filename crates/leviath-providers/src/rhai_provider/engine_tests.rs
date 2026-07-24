//! Tests for [`super::engine`]. Separate file → excluded from the coverage
//! gate (only production code must hit 100%).
use super::*;
use tokio::sync::mpsc;

#[test]
fn pure_fns_are_callable_from_script() {
    let engine = build_init_engine();
    // parse_json + to_json round-trip
    let out: String = engine
        .eval(r#"let m = parse_json("{\"a\":1}"); to_json(m)"#)
        .unwrap();
    assert!(out.contains("\"a\":1"));
    // encode_uri
    let enc: String = engine.eval(r#"encode_uri("a b/c")"#).unwrap();
    assert_eq!(enc, "a%20b%2Fc");
    // encode_base64
    let b64: String = engine.eval(r#"encode_base64("hi")"#).unwrap();
    assert_eq!(b64, "aGk=");
    // count_tokens_heuristic
    let n: i64 = engine
        .eval(r#"count_tokens_heuristic("abcd", "general")"#)
        .unwrap();
    assert_eq!(n, 1);
}

#[test]
fn parse_json_reports_error() {
    let engine = build_init_engine();
    let err = engine
        .eval::<Dynamic>(r#"parse_json("{not json}")"#)
        .unwrap_err();
    assert!(err.to_string().contains("parse_json"));
}

#[test]
fn parse_sse_variants() {
    let engine = build_init_engine();
    assert!(
        engine
            .eval::<Dynamic>(r#"parse_sse("data: [DONE]")"#)
            .unwrap()
            .is_unit()
    );
    assert!(
        engine
            .eval::<Dynamic>(r#"parse_sse("")"#)
            .unwrap()
            .is_unit()
    );
    assert!(
        engine
            .eval::<Dynamic>(r#"parse_sse("not json")"#)
            .unwrap()
            .is_unit()
    );
    let ok: i64 = engine
        .eval(r#"let d = parse_sse("data: {\"n\":7}"); d.n"#)
        .unwrap();
    assert_eq!(ok, 7);
}

#[test]
fn env_var_present_and_absent() {
    let engine = build_init_engine();
    temp_env::with_var("LEVIATH_TEST_ENVX", Some("val"), || {
        let v: String = engine.eval(r#"env_var("LEVIATH_TEST_ENVX")"#).unwrap();
        assert_eq!(v, "val");
    });
    let missing = engine
        .eval::<Dynamic>(r#"env_var("LEVIATH_DEFINITELY_UNSET_XYZ")"#)
        .unwrap();
    assert!(missing.is_unit());
}

#[test]
fn to_json_rejects_unconvertible() {
    // A function pointer has no JSON representation.
    let engine = build_init_engine();
    let err = engine.eval::<String>(r#"to_json(|x| x)"#).unwrap_err();
    // Either a from_dynamic error or a type-mismatch; the point is it errors.
    let _ = err;
}

#[test]
fn count_tokens_heuristic_dispatches_by_hint() {
    assert_eq!(count_tokens_heuristic_fn("abcd", "gemini"), 1);
    assert_eq!(count_tokens_heuristic_fn("abcdefgh", "anthropic"), 3);
    assert!(count_tokens_heuristic_fn("hello world", "openai") >= 1);
}

#[test]
fn hex_digit_covers_both_arms() {
    assert_eq!(hex_digit(5), '5');
    assert_eq!(hex_digit(12), 'C');
}

#[test]
fn to_json_fn_serializes_a_value() {
    let mut m = rhai::Map::new();
    m.insert("a".into(), 1_i64.into());
    assert_eq!(to_json_fn(Dynamic::from_map(m)).unwrap(), "{\"a\":1}");
}

#[tokio::test]
async fn unary_errors_when_reply_is_dropped() {
    // The broker receives the job but drops the reply without responding →
    // the blocking host fn gets a closed-channel error.
    let (tx, mut rx) = mpsc::unbounded_channel::<BrokerJob>();
    let handle = tokio::task::spawn_blocking(move || {
        unary(
            &tx,
            HttpMethod::Get,
            "http://x",
            None,
            BTreeMap::new(),
            None,
        )
    });
    // Receive and drop the whole job (dropping the reply channel with it).
    drop(rx.recv().await.unwrap());
    assert!(handle.await.unwrap().is_err());
}

#[test]
fn unary_errors_when_broker_gone() {
    // Sending a job with no receiver fails immediately (channel closed).
    let (tx, rx) = mpsc::unbounded_channel::<BrokerJob>();
    drop(rx);
    let res = unary(
        &tx,
        HttpMethod::Get,
        "http://x",
        None,
        BTreeMap::new(),
        None,
    );
    assert!(res.is_err());
}

#[test]
fn stream_request_errors_when_broker_gone() {
    // With the broker's job receiver dropped, stream_request's send fails and
    // surfaces as a runtime error to the script.
    let (job_tx, job_rx) = mpsc::unbounded_channel::<BrokerJob>();
    drop(job_rx);
    let engine = build_exec_engine(ExecConfig {
        jobs: job_tx,
        timeout_secs: None,
        chunk_tx: None,
    });
    let err = engine
        .eval::<()>(r#"stream_request("http://x", "{}", #{}, |c| {})"#)
        .unwrap_err();
    assert!(err.to_string().contains("channel closed"));
}

#[test]
fn print_and_debug_are_silenced() {
    // Drives the no-op on_print/on_debug hooks (sandbox: no data leakage).
    let engine = build_init_engine();
    engine.run(r#"print("hello"); debug("world");"#).unwrap();
}

#[test]
fn encode_uri_passes_unreserved_and_encodes_rest() {
    let engine = build_init_engine();
    let out: String = engine.eval(r#"encode_uri("Aa0-_.~ /?")"#).unwrap();
    // Unreserved chars pass through; space/slash/question are percent-encoded.
    assert_eq!(out, "Aa0-_.~%20%2F%3F");
}
