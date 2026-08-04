//! Tests for the `submit_output` handler.
//!
//! In a sibling file rather than an inline `#[cfg(test)] mod`, following the
//! layout the rest of this workspace uses where the module under test is small
//! and the tests are not.

use super::*;
use leviath_core::{Region, RegionKind};
use serde_json::json;

/// A window shaped the way `context_setup` builds one, with the pinned
/// `final_output` region present.
fn win() -> ContextWindow {
    let mut w = ContextWindow::new(100_000);
    w.add_region(Region::new("task".to_string(), RegionKind::Pinned, 5_000));
    w.add_region(Region::new(
        FINAL_OUTPUT_REGION.to_string(),
        RegionKind::Pinned,
        20_000,
    ));
    w
}

fn spec(format: Option<&str>, schema: Option<serde_json::Value>) -> OutputSpec {
    OutputSpec {
        format: format.map(str::to_string),
        schema,
        ..OutputSpec::default()
    }
}

fn region_text(window: &ContextWindow) -> String {
    window
        .get_region(FINAL_OUTPUT_REGION)
        .expect("the region exists")
        .content
        .iter()
        .map(|e| e.content.as_str())
        .collect::<Vec<_>>()
        .join("")
}

#[test]
fn only_the_submit_tool_is_claimed() {
    assert!(is_output_tool("submit_output"));
    assert!(!is_output_tool("context_write"));
    assert!(!is_output_tool("write_file"));
    assert!(!is_output_tool("submit_output_extra"));
}

#[test]
fn a_submission_is_recorded_verbatim_and_mirrored_into_the_region() {
    let mut w = win();
    let (ack, output) = handle_output_tool(
        &json!({"content": "Renamed two helpers and updated their callers."}),
        Some(&spec(Some("markdown"), None)),
        "summary",
        1234,
        &mut w,
    );
    let output = output.expect("the submission was accepted");
    assert_eq!(
        output.content,
        "Renamed two helpers and updated their callers."
    );
    assert_eq!(output.format.as_deref(), Some("markdown"));
    assert_eq!(output.stage, "summary");
    assert_eq!(output.submitted_at, 1234);
    assert!(!output.truncated);
    assert!(ack.contains("final output"), "{ack}");
    assert_eq!(region_text(&w), output.content);
}

/// The point of the whole design: a format the engine has never heard of goes
/// through untouched, with no parsing and no per-format branch.
#[test]
fn an_unrecognized_format_is_carried_through_without_inspection() {
    let mut w = win();
    let a2ui = r#"{"root":{"component":"Card","children":[{"component":"Text"}]}}"#;
    let (_, output) = handle_output_tool(
        &json!({ "content": a2ui }),
        Some(&spec(Some("a2ui"), None)),
        "summary",
        0,
        &mut w,
    );
    let output = output.expect("accepted");
    assert_eq!(output.content, a2ui, "byte-identical");
    assert_eq!(output.format.as_deref(), Some("a2ui"));
}

/// Content that is nothing like JSON is equally fine when no schema was asked
/// for, which is what makes an arbitrary text format work.
#[test]
fn a_format_with_no_schema_never_parses_the_content() {
    let mut w = win();
    let (_, output) = handle_output_tool(
        &json!({"content": "<report><finding>one</finding></report>"}),
        Some(&spec(Some("xml"), None)),
        "summary",
        0,
        &mut w,
    );
    assert_eq!(
        output.expect("accepted").content,
        "<report><finding>one</finding></report>"
    );
}

/// Naming a format is not asking for validation. Only a schema is.
#[test]
fn format_json_alone_validates_nothing() {
    let mut w = win();
    let (_, output) = handle_output_tool(
        &json!({"content": "this is not JSON at all"}),
        Some(&spec(Some("json"), None)),
        "summary",
        0,
        &mut w,
    );
    assert!(output.is_some(), "no schema means no check");
}

#[test]
fn no_spec_at_all_still_records_an_answer() {
    let mut w = win();
    let (_, output) = handle_output_tool(&json!({"content": "done"}), None, "summary", 0, &mut w);
    let output = output.expect("accepted");
    assert_eq!(output.content, "done");
    assert!(output.format.is_none());
}

#[test]
fn a_submission_matching_its_schema_is_accepted() {
    let mut w = win();
    let schema = json!({
        "type": "object",
        "required": ["summary"],
        "properties": {"summary": {"type": "string"}}
    });
    let (_, output) = handle_output_tool(
        &json!({"content": r#"{"summary":"two files changed"}"#}),
        Some(&spec(Some("json"), Some(schema))),
        "summary",
        0,
        &mut w,
    );
    assert!(output.is_some());
}

#[test]
fn a_submission_violating_its_schema_is_refused_and_records_nothing() {
    let mut w = win();
    let schema = json!({
        "type": "object",
        "required": ["summary"],
        "properties": {"summary": {"type": "string"}}
    });
    let (message, output) = handle_output_tool(
        &json!({"content": r#"{"nope":1}"#}),
        Some(&spec(Some("json"), Some(schema))),
        "summary",
        0,
        &mut w,
    );
    assert!(output.is_none(), "nothing recorded");
    // The `[error]` prefix is load-bearing: it is in the dispatch layer's
    // no-effect list, so a refused submission is not counted as work done.
    assert!(message.starts_with("[error]"), "{message}");
    assert!(message.contains("schema"), "{message}");
    // And the region is untouched, so a bad correction cannot erase a good answer.
    assert_eq!(region_text(&w), "");
}

/// A schema means the author wants JSON, so content that will not parse is a
/// violation in its own right rather than a panic or a silent pass.
#[test]
fn content_that_is_not_json_fails_a_schema_check_with_a_readable_reason() {
    let mut w = win();
    let (message, output) = handle_output_tool(
        &json!({"content": "plain prose"}),
        Some(&spec(None, Some(json!({"type": "object"})))),
        "summary",
        0,
        &mut w,
    );
    assert!(output.is_none());
    assert!(message.contains("not valid JSON"), "{message}");
}

/// One bad schema must not make a run unable to finish, so an uncompilable
/// schema skips the check exactly as tool-argument validation does.
#[test]
fn an_uncompilable_schema_records_the_submission_unchecked() {
    let mut w = win();
    let (_, output) = handle_output_tool(
        &json!({"content": "anything"}),
        // A misspelled `type` is the schema this workspace already uses to mean
        // "will not compile" (a typo'd Rhai `@param n strng` produces exactly it).
        Some(&spec(None, Some(json!({"type": "strng"})))),
        "summary",
        0,
        &mut w,
    );
    assert!(output.is_some(), "a broken schema does not block the run");
}

#[test]
fn a_missing_content_argument_is_refused() {
    let mut w = win();
    let (message, output) = handle_output_tool(&json!({}), None, "summary", 0, &mut w);
    assert!(output.is_none());
    assert!(message.starts_with("[error]"), "{message}");
    assert!(message.contains("content"), "{message}");
}

#[test]
fn a_blank_submission_is_refused() {
    let mut w = win();
    for blank in ["", "   ", "\n\t "] {
        let (message, output) =
            handle_output_tool(&json!({ "content": blank }), None, "summary", 0, &mut w);
        assert!(output.is_none(), "{blank:?} should not count as an answer");
        assert!(message.starts_with("[error]"), "{message}");
    }
}

#[test]
fn an_oversized_submission_is_truncated_and_the_model_is_told() {
    let mut w = win();
    let huge = "x".repeat(leviath_core::output::MAX_FINAL_OUTPUT_BYTES + 10);
    let (ack, output) = handle_output_tool(&json!({ "content": huge }), None, "summary", 0, &mut w);
    let output = output.expect("accepted, just shortened");
    assert!(output.truncated);
    assert_eq!(
        output.content.len(),
        leviath_core::output::MAX_FINAL_OUTPUT_BYTES
    );
    assert!(ack.contains("truncated"), "{ack}");
}

/// Submitting twice replaces rather than appends: an agent that corrects itself
/// meant the correction.
#[test]
fn a_second_submission_replaces_the_first() {
    let mut w = win();
    let (_, first) = handle_output_tool(&json!({"content": "draft"}), None, "summary", 1, &mut w);
    assert_eq!(first.expect("accepted").content, "draft");
    let (_, second) = handle_output_tool(&json!({"content": "final"}), None, "summary", 2, &mut w);
    assert_eq!(second.expect("accepted").content, "final");
    assert_eq!(region_text(&w), "final", "the region holds one answer");
}

/// The component is what every consumer reads, so a world whose layout lacks the
/// mirror region still records the answer rather than losing it.
#[test]
fn a_window_without_the_region_still_records_the_output() {
    let mut bare = ContextWindow::new(10_000);
    bare.add_region(Region::new("task".to_string(), RegionKind::Pinned, 1_000));
    let (_, output) =
        handle_output_tool(&json!({"content": "done"}), None, "summary", 0, &mut bare);
    assert_eq!(output.expect("accepted").content, "done");
    assert!(bare.get_region(FINAL_OUTPUT_REGION).is_none());
}
