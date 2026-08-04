//! Tests for `lev result`'s rendering.

use super::*;

fn answer(content: &str, format: Option<&str>) -> leviath_core::FinalOutput {
    leviath_core::output::FinalOutput::new(
        content,
        format.map(str::to_string),
        "summary".to_string(),
        42,
    )
}

/// `render` takes the answer directly now: `meta.json` carries only the
/// descriptor, and the caller fetches the bytes from the sidecar.
fn shown(output: Option<&leviath_core::FinalOutput>, json: bool, raw: bool) -> Option<String> {
    render("run-1", output, json, raw)
}

#[test]
fn a_run_with_no_answer_renders_nothing() {
    assert!(shown(None, false, false).is_none());
    assert!(shown(None, true, false).is_none());
    assert!(shown(None, false, true).is_none());
}

#[test]
fn the_default_rendering_names_the_run_the_stage_and_the_shape() {
    let out = shown(
        Some(&answer("Renamed two helpers.", Some("markdown"))),
        false,
        false,
    )
    .expect("there is an answer");
    assert!(out.contains("run-1"), "{out}");
    assert!(out.contains("summary"), "{out}");
    assert!(out.contains("(markdown)"), "{out}");
    assert!(out.contains("Renamed two helpers."), "{out}");
}

#[test]
fn an_answer_with_no_declared_shape_omits_the_label() {
    let out = shown(Some(&answer("plain", None)), false, false).expect("there is an answer");
    assert!(!out.contains("()"), "{out}");
    assert!(out.contains("plain"), "{out}");
}

/// `--raw` is for pipelines, so it emits the answer and nothing else - no
/// heading to strip and no label to confuse a downstream parser.
#[test]
fn raw_prints_only_the_answer() {
    let out = shown(Some(&answer(r#"{"root":{}}"#, Some("a2ui"))), false, true)
        .expect("there is an answer");
    assert_eq!(out, "{\"root\":{}}\n");
}

#[test]
fn raw_does_not_double_a_trailing_newline() {
    let out =
        shown(Some(&answer("ends already\n", None)), false, true).expect("there is an answer");
    assert_eq!(out, "ends already\n");
}

/// The JSON form carries the whole record, because a caller parsing it wants
/// the format label and the truncation flag as much as the content.
#[test]
fn json_carries_the_shape_and_the_truncation_flag() {
    let out = shown(Some(&answer(r#"{"root":{}}"#, Some("a2ui"))), true, false)
        .expect("there is an answer");
    let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
    assert_eq!(parsed["content"].as_str().unwrap(), r#"{"root":{}}"#);
    assert_eq!(parsed["format"].as_str().unwrap(), "a2ui");
    assert_eq!(parsed["stage"].as_str().unwrap(), "summary");
    assert_eq!(parsed["submitted_at"].as_i64().unwrap(), 42);
    assert!(!parsed["truncated"].as_bool().unwrap());
}

#[test]
fn a_truncated_answer_says_so() {
    let huge = "x".repeat(leviath_core::output::MAX_FINAL_OUTPUT_BYTES + 1);
    let out = shown(Some(&answer(&huge, None)), false, false).expect("there is an answer");
    assert!(out.contains("truncated"), "the reader is told");
}

/// An unrecognized format is printed byte for byte. Nothing here reformats,
/// re-indents, or re-serializes an answer.
#[test]
fn an_unrecognized_format_is_printed_verbatim() {
    let doc = "<report>\n  <finding severity=\"high\"/>\n</report>";
    let out =
        shown(Some(&answer(doc, Some("vnd.acme+xml"))), false, true).expect("there is an answer");
    assert_eq!(out, format!("{doc}\n"));
}
