//! Tests for the built-in format checks.
//!
//! Each format gets three cases: something valid, the failure a model actually
//! produces, and proof that the check is well-formedness rather than shape.

use super::*;

#[test]
fn only_the_listed_formats_have_a_builtin() {
    for format in BUILTIN_FORMATS {
        assert!(has_builtin(format), "{format} should have one");
    }
    // A label this crate has never heard of is not validated, which is the
    // normal case: the label is opaque by design.
    for unknown in ["a2ui", "graphql", "markdown", "text/vnd.acme+xml", ""] {
        assert!(!has_builtin(unknown), "{unknown} should not have one");
    }
}

/// A near-miss gets no validation rather than the wrong one.
#[test]
fn a_near_miss_label_is_not_validated() {
    assert!(!has_builtin("json-lines"));
    assert!(!has_builtin("xml-fragment"));
    assert!(check(Some("json-lines"), "not json at all").is_ok());
}

#[test]
fn a_label_is_matched_case_and_whitespace_insensitively() {
    assert!(has_builtin("JSON"));
    assert!(has_builtin(" yaml "));
    assert!(check(Some("JSON"), "{\"a\":1}").is_ok());
}

#[test]
fn no_format_validates_nothing() {
    assert!(check(None, "anything at all").is_ok());
}

// ── json ─────────────────────────────────────────────────────────────────────

#[test]
fn json_accepts_valid_and_refuses_a_fenced_answer() {
    assert!(check(Some("json"), r#"{"summary":"ok","rows":[1,2]}"#).is_ok());
    // The failure that actually happens: the model wraps its answer in fences.
    let fenced = "```json\n{\"a\":1}\n```";
    assert!(check(Some("json"), fenced).is_err());
}

/// Well-formedness, not shape. A JSON document with entirely the wrong contents
/// still passes; that is what a JSON Schema is for.
#[test]
fn json_does_not_check_shape() {
    assert!(check(Some("json"), r#"{"totally":"unexpected"}"#).is_ok());
}

// ── xml ──────────────────────────────────────────────────────────────────────

#[test]
fn xml_accepts_valid_and_refuses_an_unclosed_tag() {
    assert!(check(Some("xml"), "<report><finding severity=\"high\"/></report>").is_ok());
    assert!(check(Some("xml"), "<report><finding></report>").is_err());
}

/// Prose parses as a single text event, so without the element check it would
/// pass as "valid XML". A stage that asked for XML and got a paragraph should
/// hear about it.
#[test]
fn xml_refuses_plain_prose() {
    let err = check(Some("xml"), "Here are my findings, in prose.").expect_err("prose is not XML");
    assert!(err.contains("plain text"), "{err}");
}

#[test]
fn xml_refuses_json_handed_back_by_mistake() {
    assert!(check(Some("xml"), r#"{"finding":"high"}"#).is_err());
}

// ── yaml ─────────────────────────────────────────────────────────────────────

#[test]
fn yaml_accepts_valid_and_refuses_broken_indentation() {
    assert!(check(Some("yaml"), "findings:\n  - severity: high\n").is_ok());
    assert!(check(Some("yml"), "findings:\n  - severity: high\n").is_ok());
    assert!(check(Some("yaml"), "a: [1, 2\nb: 3").is_err());
}

// ── csv ──────────────────────────────────────────────────────────────────────

#[test]
fn csv_accepts_valid_and_refuses_a_ragged_row() {
    assert!(check(Some("csv"), "name,value\nalpha,1\nbeta,2\n").is_ok());
    // A drifting column count is the failure a consumer actually hits.
    assert!(check(Some("csv"), "name,value\nalpha,1,extra\n").is_err());
}

#[test]
fn csv_refuses_an_empty_answer() {
    assert!(check(Some("csv"), "   \n").is_err());
}

#[test]
fn csv_refuses_an_unbalanced_quote() {
    assert!(check(Some("csv"), "name,value\n\"unterminated,1\n").is_err());
}

// ── toml ─────────────────────────────────────────────────────────────────────

#[test]
fn toml_accepts_valid_and_refuses_a_broken_table() {
    assert!(check(Some("toml"), "[section]\nkey = \"value\"\n").is_ok());
    assert!(check(Some("toml"), "[section\nkey = ").is_err());
}

/// Every failure message is something the agent can act on, since it goes back
/// as the refusal it retries against.
#[test]
fn a_failure_says_what_was_wrong() {
    for (format, bad) in [
        ("json", "not json"),
        ("xml", "<a>"),
        ("yaml", "a: [1, 2\nb: 3"),
        ("csv", "a,b\n1,2,3\n"),
        ("toml", "[oops"),
    ] {
        let outcome = check(Some(format), bad);
        assert!(
            outcome.is_err(),
            "{format} accepted {bad:?}, which it should not"
        );
        assert!(
            !outcome.expect_err("asserted Err just above").is_empty(),
            "{format} gave an empty reason"
        );
    }
}
