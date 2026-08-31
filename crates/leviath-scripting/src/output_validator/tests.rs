//! Tests for script-backed output validators.

use super::*;

fn compiled(source: &str) -> OutputValidator {
    compile("test.rhai", source).expect("fixture compiles")
}

#[test]
fn a_validator_that_returns_unit_accepts() {
    let v = compiled("fn validate(content) { () }");
    assert_eq!(validate(&v, "anything"), Verdict::Valid);
}

/// An empty string is easy to write by accident and unambiguous in meaning, so
/// it reads as "fine" rather than as a blank complaint.
#[test]
fn an_empty_reason_accepts() {
    let v = compiled(r#"fn validate(content) { "" }"#);
    assert_eq!(validate(&v, "anything"), Verdict::Valid);
    let blank = compiled(r#"fn validate(content) { "   " }"#);
    assert_eq!(validate(&blank, "anything"), Verdict::Valid);
}

#[test]
fn a_returned_string_is_the_reason_the_agent_sees() {
    let v = compiled(r#"fn validate(content) { "the document has no root node" }"#);
    assert_eq!(
        validate(&v, "{}"),
        Verdict::Invalid("the document has no root node".to_string())
    );
}

/// The realistic shape: parse, look at the document, complain specifically.
#[test]
fn a_validator_can_inspect_the_content_it_is_given() {
    let v = compiled(
        r#"
        fn validate(content) {
            let doc = parse_json(content);
            if doc.root == () { return "missing `root`"; }
            ()
        }
        "#,
    );
    assert_eq!(validate(&v, r#"{"root":{"a":1}}"#), Verdict::Valid);
    assert_eq!(
        validate(&v, r#"{"other":1}"#),
        Verdict::Invalid("missing `root`".to_string())
    );
}

/// A throw is its own verdict, not folded into `Invalid`: the consumer decides
/// what happens to the submission, and either way the run flags the script as
/// broken. The error text travels with the verdict so a refusal can hand the
/// model something actionable.
#[test]
fn a_validator_that_throws_is_unusable_rather_than_a_rejection() {
    let v = compiled(r#"fn validate(content) { throw "boom" }"#);
    match validate(&v, "anything") {
        Verdict::Unusable(reason) => assert!(reason.contains("boom"), "{reason}"),
        other => panic!("expected Unusable, got {other:?}"),
    }
}

#[test]
fn a_validator_returning_the_wrong_type_is_unusable() {
    let v = compiled("fn validate(content) { 42 }");
    match validate(&v, "anything") {
        Verdict::Unusable(reason) => {
            assert!(reason.contains("() or a string"), "{reason}");
        }
        other => panic!("expected Unusable, got {other:?}"),
    }
}

/// An operation-bounded engine, so a runaway validator stops rather than
/// hanging the run.
#[test]
fn a_runaway_validator_is_stopped() {
    let v = compiled("fn validate(content) { let i = 0; loop { i += 1; } }");
    assert!(matches!(validate(&v, "x"), Verdict::Unusable(_)));
}

// ── compile-time shape ───────────────────────────────────────────────────────

/// Refused at spawn rather than silently never running, because the moment to
/// discover an agent cannot hand back its work is not the end of a long run.
#[test]
fn a_script_without_validate_is_refused() {
    let err = compile("v.rhai", "fn other(x) { () }").expect_err("no validate fn");
    assert!(format!("{err}").contains("must define fn validate"));
}

#[test]
fn a_validate_with_the_wrong_arity_is_refused() {
    let err = compile("v.rhai", "fn validate(a, b) { () }").expect_err("wrong arity");
    assert!(format!("{err}").contains("exactly one parameter"));
}

#[test]
fn a_script_that_does_not_compile_is_refused() {
    let err = compile("v.rhai", "fn validate(content) { this is not rhai").expect_err("bad syntax");
    assert!(format!("{err}").contains("v.rhai"));
}

/// The hardened engine gives a validator no filesystem and no network, the same
/// as every other script seam.
#[test]
fn a_validator_cannot_reach_the_filesystem() {
    // `open_file` is not registered on a hardened engine, so this fails to
    // compile or fails at the call. Either way it never reads anything.
    let outcome = compile(
        "v.rhai",
        r#"fn validate(content) { open_file("/etc/passwd") }"#,
    )
    .map(|v| validate(&v, "x"));
    match outcome {
        Err(_) => {}
        Ok(Verdict::Unusable(_)) => {}
        other => panic!("a validator must not reach the filesystem, got {other:?}"),
    }
}
