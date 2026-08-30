//! Tests for the parent module. The standard child-module file name
//! (`tests.rs`) keeps this scaffolding outside the coverage report:
//! cargo-llvm-cov's default ignore regex excludes `tests.rs` and
//! `*_tests.rs` files, so only production code answers to the 100% gate.
use super::*;
use std::sync::Arc;
use tokio::sync::mpsc;

/// No credential-shaped variable is allowlisted - the default posture, and the
/// one these tests should be checking against.
fn no_env_allowlist() -> Arc<Vec<String>> {
    Arc::new(Vec::new())
}

#[test]
fn pure_fns_are_callable_from_script() {
    let engine = build_init_engine(no_env_allowlist());
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
    let engine = build_init_engine(no_env_allowlist());
    let err = engine
        .eval::<Dynamic>(r#"parse_json("{not json}")"#)
        .unwrap_err();
    assert!(err.to_string().contains("parse_json"));
}

#[test]
fn parse_sse_variants() {
    let engine = build_init_engine(no_env_allowlist());
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
    let engine = build_init_engine(no_env_allowlist());
    // An ordinary (non-credential-shaped) name reads normally. Note this is
    // deliberately *not* `LEVIATH_`-prefixed: that whole namespace is treated as
    // sensitive, since it holds the API token and the config-path redirects.
    temp_env::with_var("PROVIDER_TEST_REGION", Some("val"), || {
        let v: String = engine.eval(r#"env_var("PROVIDER_TEST_REGION")"#).unwrap();
        assert_eq!(v, "val");
    });
    let missing = engine
        .eval::<Dynamic>(r#"env_var("PROVIDER_DEFINITELY_UNSET_XYZ")"#)
        .unwrap();
    assert!(missing.is_unit());
}

/// A provider script runs during inference, not through a tool call, so nothing
/// it does passes an approval prompt. An unallowlisted credential name therefore
/// reads as unset - scripts already handle "my key isn't in the environment" by
/// falling back to their `initialize` config, so this puts them on that path
/// rather than failing the whole inference.
#[test]
fn env_var_refuses_credential_names_unless_allowlisted() {
    let engine = build_init_engine(no_env_allowlist());
    temp_env::with_var("ANTHROPIC_API_KEY", Some("sk-ant-secret"), || {
        let v = engine
            .eval::<Dynamic>(r#"env_var("ANTHROPIC_API_KEY")"#)
            .unwrap();
        assert!(v.is_unit(), "the key must not reach the script");
    });
}

#[test]
fn env_var_allowlist_permits_exactly_the_named_variable() {
    let engine = build_init_engine(Arc::new(vec!["MY_PROVIDER_KEY".to_string()]));
    temp_env::with_vars(
        [
            ("MY_PROVIDER_KEY", Some("mine")),
            ("ANTHROPIC_API_KEY", Some("sk-ant-secret")),
        ],
        || {
            let allowed: String = engine.eval(r#"env_var("MY_PROVIDER_KEY")"#).unwrap();
            assert_eq!(allowed, "mine");
            let refused = engine
                .eval::<Dynamic>(r#"env_var("ANTHROPIC_API_KEY")"#)
                .unwrap();
            assert!(refused.is_unit(), "allowlisting one name allows only it");
        },
    );
}

/// `eval` and `import` are both disabled: either would reach code that never
/// passed whatever review the script itself did.
#[test]
fn eval_and_module_imports_are_disabled() {
    let engine = build_init_engine(no_env_allowlist());
    assert!(engine.eval::<i64>(r#"eval("1 + 1")"#).is_err());
    assert!(engine.eval::<i64>(r#"import "other" as m; 1"#).is_err());
}

#[test]
fn to_json_rejects_unconvertible() {
    // A function pointer has no JSON representation.
    let engine = build_init_engine(no_env_allowlist());
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
        env_allowlist: no_env_allowlist(),
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
    let engine = build_init_engine(no_env_allowlist());
    engine.run(r#"print("hello"); debug("world");"#).unwrap();
}

/// A map carrying non-printable characters must come back as JSON, not as
/// Rhai's `Debug`-escaped lookalike.
///
/// Rhai's `map_basic` package registers `to_json(&mut Map)`, and that
/// signature beats ours unless the same one is registered over it, sending
/// every request body a provider script builds through `format_map_as_json`.
/// That writes strings with `Debug`, which
/// spells a narrow no-break space `\u{202f}`. JSON has no such escape, so the
/// API refused the whole request and named the offset it choked on.
///
/// The characters are named as Rust escapes and interpolated into the script
/// source, because pasting them in literally leaves an invisible test.
#[test]
fn to_json_on_a_map_is_valid_json_for_non_printable_characters() {
    let engine = build_init_engine(no_env_allowlist());
    // Narrow no-break space, zero-width space and a BOM are non-printable, so
    // `Debug` escapes them. A non-breaking hyphen is printable and never was.
    let text = "NASDAQ:\u{202f}CBRS\u{200b} wafer\u{2011}scale\u{feff}";
    let out: String = engine
        .eval(&format!("to_json(#{{ content: \"{text}\" }})"))
        .expect("to_json");
    assert!(
        !out.contains("\\u{"),
        "Rhai's Debug-escaped formatter answered: {out}"
    );
    let parsed: serde_json::Value = serde_json::from_str(&out).expect("body must be JSON");
    assert_eq!(
        parsed["content"],
        serde_json::Value::String(text.to_string())
    );
}

/// The same guarantee for the shapes that never reached Rhai's overload: a
/// bare string, an array, and a map nested inside one.
#[test]
fn to_json_is_valid_json_for_nested_and_scalar_shapes() {
    let engine = build_init_engine(no_env_allowlist());
    let out: String = engine
        .eval("to_json([#{ a: \"x\u{202f}y\" }, \"b\u{200b}c\", 1])")
        .expect("to_json");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&out).expect("JSON"),
        serde_json::json!([{ "a": "x\u{202f}y" }, "b\u{200b}c", 1])
    );
}

#[test]
fn decode_base64_round_trips_and_reports_its_failures() {
    let engine = build_init_engine(no_env_allowlist());

    // A provider script that encodes something can now read it back, and the
    // implementation is the one the tool engine offers under the same name.
    let out: String = engine
        .eval(r#"decode_base64(encode_base64("provider · 🐙"))"#)
        .expect("round trips");
    assert_eq!(out, "provider · 🐙");

    // A value written by anything else decodes too.
    let decoded: String = engine.eval(r#"decode_base64("aGk=")"#).expect("decodes");
    assert_eq!(decoded, "hi");

    // And a failure is an error the script sees, not an empty string it would
    // carry on with.
    let err = engine
        .eval::<String>(r#"decode_base64("not base64!")"#)
        .expect_err("refused");
    assert!(err.to_string().contains("not valid base64"), "{err}");
}

#[test]
fn encode_uri_passes_unreserved_and_encodes_rest() {
    let engine = build_init_engine(no_env_allowlist());
    let out: String = engine.eval(r#"encode_uri("Aa0-_.~ /?")"#).unwrap();
    // Unreserved chars pass through; space/slash/question are percent-encoded.
    assert_eq!(out, "Aa0-_.~%20%2F%3F");
}
