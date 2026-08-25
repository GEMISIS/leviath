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
        &OutputContext {
            spec: Some(&spec(Some("markdown"), None)),
            validators: None,
            stage: "summary",
            stage_names: &[],
            workdir: None,
        },
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
        &OutputContext {
            spec: Some(&spec(Some("a2ui"), None)),
            validators: None,
            stage: "summary",
            stage_names: &[],
            workdir: None,
        },
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
        &OutputContext {
            spec: Some(&spec(Some("xml"), None)),
            validators: None,
            stage: "summary",
            stage_names: &[],
            workdir: None,
        },
        0,
        &mut w,
    );
    assert_eq!(
        output.expect("accepted").content,
        "<report><finding>one</finding></report>"
    );
}

/// Naming a format asks for well-formedness, not shape. `json` means "this must
/// parse as JSON"; it does not mean "this must have the fields I wanted", which
/// is what a schema is for.
#[test]
fn a_format_checks_well_formedness_but_not_shape() {
    let mut w = win();
    // Parses, but has nothing anyone asked for. Accepted: no schema was given.
    let (_, output) = handle_output_tool(
        &json!({"content": r#"{"totally":"unexpected"}"#}),
        &OutputContext {
            spec: Some(&spec(Some("json"), None)),
            validators: None,
            stage: "summary",
            stage_names: &[],
            workdir: None,
        },
        0,
        &mut w,
    );
    assert!(output.is_some(), "shape is not the format check's business");

    // Does not parse. Refused, with no schema involved.
    let (message, refused) = handle_output_tool(
        &json!({"content": "this is not JSON at all"}),
        &OutputContext {
            spec: Some(&spec(Some("json"), None)),
            validators: None,
            stage: "summary",
            stage_names: &[],
            workdir: None,
        },
        0,
        &mut w,
    );
    assert!(refused.is_none());
    assert!(message.contains("not valid json"), "{message}");
}

#[test]
fn no_spec_at_all_still_records_an_answer() {
    let mut w = win();
    let (_, output) = handle_output_tool(
        &json!({"content": "done"}),
        &OutputContext {
            spec: None,
            validators: None,
            stage: "summary",
            stage_names: &[],
            workdir: None,
        },
        0,
        &mut w,
    );
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
        &OutputContext {
            spec: Some(&spec(Some("json"), Some(schema))),
            validators: None,
            stage: "summary",
            stage_names: &[],
            workdir: None,
        },
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
        &OutputContext {
            spec: Some(&spec(Some("json"), Some(schema))),
            validators: None,
            stage: "summary",
            stage_names: &[],
            workdir: None,
        },
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
        &OutputContext {
            spec: Some(&spec(None, Some(json!({"type": "object"})))),
            validators: None,
            stage: "summary",
            stage_names: &[],
            workdir: None,
        },
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
        &OutputContext {
            spec: // A misspelled `type` is the schema this workspace already uses to mean
        // "will not compile" (a typo'd Rhai `@param n strng` produces exactly it).
        Some(&spec(None, Some(json!({"type": "strng"})))),
            validators: None,
            stage: "summary",
            stage_names: &[],
            workdir: None,
        },
        0,
        &mut w,
    );
    assert!(output.is_some(), "a broken schema does not block the run");
}

#[test]
fn a_missing_content_argument_is_refused() {
    let mut w = win();
    let (message, output) = handle_output_tool(
        &json!({}),
        &OutputContext {
            spec: None,
            validators: None,
            stage: "summary",
            stage_names: &[],
            workdir: None,
        },
        0,
        &mut w,
    );
    assert!(output.is_none());
    assert!(message.starts_with("[error]"), "{message}");
    assert!(message.contains("content"), "{message}");
}

#[test]
fn a_blank_submission_is_refused() {
    let mut w = win();
    for blank in ["", "   ", "\n\t "] {
        let (message, output) = handle_output_tool(
            &json!({ "content": blank }),
            &OutputContext {
                spec: None,
                validators: None,
                stage: "summary",
                stage_names: &[],
                workdir: None,
            },
            0,
            &mut w,
        );
        assert!(output.is_none(), "{blank:?} should not count as an answer");
        assert!(message.starts_with("[error]"), "{message}");
    }
}

#[test]
fn an_oversized_submission_is_truncated_and_the_model_is_told() {
    let mut w = win();
    let huge = "x".repeat(leviath_core::output::MAX_FINAL_OUTPUT_BYTES + 10);
    let (ack, output) = handle_output_tool(
        &json!({ "content": huge }),
        &OutputContext {
            spec: None,
            validators: None,
            stage: "summary",
            stage_names: &[],
            workdir: None,
        },
        0,
        &mut w,
    );
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
    let (_, first) = handle_output_tool(
        &json!({"content": "draft"}),
        &OutputContext {
            spec: None,
            validators: None,
            stage: "summary",
            stage_names: &[],
            workdir: None,
        },
        1,
        &mut w,
    );
    assert_eq!(first.expect("accepted").content, "draft");
    let (_, second) = handle_output_tool(
        &json!({"content": "final"}),
        &OutputContext {
            spec: None,
            validators: None,
            stage: "summary",
            stage_names: &[],
            workdir: None,
        },
        2,
        &mut w,
    );
    assert_eq!(second.expect("accepted").content, "final");
    assert_eq!(region_text(&w), "final", "the region holds one answer");
}

/// The component is what every consumer reads, so a world whose layout lacks the
/// mirror region still records the answer rather than losing it.
#[test]
fn a_window_without_the_region_still_records_the_output() {
    let mut bare = ContextWindow::new(10_000);
    bare.add_region(Region::new("task".to_string(), RegionKind::Pinned, 1_000));
    let (_, output) = handle_output_tool(
        &json!({"content": "done"}),
        &OutputContext {
            spec: None,
            validators: None,
            stage: "summary",
            stage_names: &[],
            workdir: None,
        },
        0,
        &mut bare,
    );
    assert_eq!(output.expect("accepted").content, "done");
    assert!(bare.get_region(FINAL_OUTPUT_REGION).is_none());
}

// ── Artifacts ────────────────────────────────────────────────────────────────

/// An answer is one model response, so anything larger is a file. The artifact
/// list is how a consumer finds it without parsing prose.
#[test]
fn artifacts_inside_the_workdir_are_recorded() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("results.csv"), "a,b\n1,2\n").expect("write");
    let mut w = win();
    let (_, output) = handle_output_tool(
        &json!({
            "content": "2 rows gathered, written to results.csv",
            "artifacts": ["results.csv", "notes/summary.md"],
        }),
        &OutputContext {
            spec: None,
            validators: None,
            stage: "present",
            stage_names: &[],
            workdir: Some(dir.path()),
        },
        0,
        &mut w,
    );
    let output = output.expect("accepted");
    // A file that does not exist yet is fine: the check is where a path lands,
    // not whether the agent has finished writing it.
    assert_eq!(output.artifacts, vec!["results.csv", "notes/summary.md"]);
}

/// A path that escapes the workdir is refused outright rather than dropped from
/// the list, which would send the caller looking for a file that was named and
/// then quietly forgotten.
#[test]
fn an_artifact_outside_the_workdir_refuses_the_whole_submission() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut w = win();
    let (message, output) = handle_output_tool(
        &json!({ "content": "done", "artifacts": ["../../etc/passwd"] }),
        &OutputContext {
            spec: None,
            validators: None,
            stage: "present",
            stage_names: &[],
            workdir: Some(dir.path()),
        },
        0,
        &mut w,
    );
    assert!(output.is_none(), "nothing recorded");
    assert!(message.starts_with("[error]"), "{message}");
    assert!(message.contains("../../etc/passwd"), "{message}");
}

#[test]
fn no_artifacts_argument_records_an_empty_list() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut w = win();
    let (_, output) = handle_output_tool(
        &json!({ "content": "done" }),
        &OutputContext {
            spec: None,
            validators: None,
            stage: "present",
            stage_names: &[],
            workdir: Some(dir.path()),
        },
        0,
        &mut w,
    );
    assert!(output.expect("accepted").artifacts.is_empty());
}

/// Unreachable for a real run, since every one carries its metadata. Loud
/// rather than silent if it ever is.
#[test]
fn artifacts_with_no_workdir_are_refused() {
    let mut w = win();
    let (message, output) = handle_output_tool(
        &json!({ "content": "done", "artifacts": ["results.csv"] }),
        &OutputContext {
            spec: None,
            validators: None,
            stage: "present",
            stage_names: &[],
            workdir: None,
        },
        0,
        &mut w,
    );
    assert!(output.is_none());
    assert!(message.contains("working directory"), "{message}");
}

/// The mirror is a convenience, not the storage. Sized at a whole answer it
/// would pin ~65k tokens into every later inference for the rest of the run, so
/// a long answer is mirrored as a preview and the full text stays on the
/// component and on disk.
#[test]
fn a_long_answer_is_mirrored_as_a_bounded_preview() {
    let mut w = win();
    let long = "y".repeat(200_000);
    let (_, output) = handle_output_tool(
        &json!({ "content": long }),
        &OutputContext {
            spec: None,
            validators: None,
            stage: "summary",
            stage_names: &[],
            workdir: None,
        },
        0,
        &mut w,
    );
    // The component keeps the whole thing.
    assert_eq!(output.expect("accepted").content.len(), 200_000);

    let region = w.get_region(FINAL_OUTPUT_REGION).expect("region");
    assert!(!region.content.is_empty(), "a preview landed");
    assert!(
        region.current_tokens <= region.max_tokens,
        "and it fits the budget"
    );
    assert!(
        region.content[0].content.contains("not in context"),
        "and says where the rest is"
    );
}

// ── Built-in format checks ───────────────────────────────────────────────────

/// A format this crate can parse is checked for well-formedness, with no schema
/// involved. The failure it catches is the one that happens: fences.
#[test]
fn a_submission_that_is_not_the_format_it_claims_is_refused() {
    let mut w = win();
    let (message, output) = handle_output_tool(
        &json!({ "content": "```json\n{\"a\":1}\n```" }),
        &OutputContext {
            spec: Some(&spec(Some("json"), None)),
            validators: None,
            stage: "summary",
            stage_names: &[],
            workdir: None,
        },
        0,
        &mut w,
    );
    assert!(output.is_none(), "nothing recorded");
    assert!(message.contains("not valid json"), "{message}");
}

#[test]
fn a_well_formed_submission_in_a_known_format_is_accepted() {
    let mut w = win();
    let (_, output) = handle_output_tool(
        &json!({ "content": "<report><finding/></report>" }),
        &OutputContext {
            spec: Some(&spec(Some("xml"), None)),
            validators: None,
            stage: "summary",
            stage_names: &[],
            workdir: None,
        },
        0,
        &mut w,
    );
    assert!(output.is_some());
}

/// The label stays opaque. A format this crate has never parsed is carried
/// through unchecked, which is what lets a2ui work with no engine support.
#[test]
fn an_unknown_format_is_still_never_inspected() {
    let mut w = win();
    let (_, output) = handle_output_tool(
        &json!({ "content": "anything at all, really" }),
        &OutputContext {
            spec: Some(&spec(Some("a2ui"), None)),
            validators: None,
            stage: "summary",
            stage_names: &[],
            workdir: None,
        },
        0,
        &mut w,
    );
    assert!(output.is_some());
}

/// "This is not even JSON" is more useful than a list of missing properties, so
/// the format check runs first.
#[test]
fn the_format_check_reports_before_the_schema_check() {
    let mut w = win();
    let (message, output) = handle_output_tool(
        &json!({ "content": "not json at all" }),
        &OutputContext {
            spec: Some(&spec(
                Some("json"),
                Some(json!({"type": "object", "required": ["summary"]})),
            )),
            validators: None,
            stage: "summary",
            stage_names: &[],
            workdir: None,
        },
        0,
        &mut w,
    );
    assert!(output.is_none());
    assert!(message.contains("not valid json"), "{message}");
    assert!(!message.contains("required property"), "{message}");
}

// ── Agent-supplied validators ────────────────────────────────────────────────

fn validators_with(source: &str) -> crate::components::OutputValidators {
    let compiled =
        leviath_scripting::output_validator::compile("v.rhai", source).expect("fixture compiles");
    crate::components::OutputValidators::new(std::collections::HashMap::from([(
        "v.rhai".to_string(),
        std::sync::Arc::new(compiled),
    )]))
}

fn spec_with_validator(format: &str) -> OutputSpec {
    OutputSpec {
        format: Some(format.to_string()),
        validator: Some("v.rhai".to_string()),
        ..OutputSpec::default()
    }
}

/// The point of the seam: a format nothing in this codebase can parse, checked
/// by the person whose format it is.
#[test]
fn an_agent_supplied_validator_rejects_a_bad_answer() {
    let vals = validators_with(
        r#"
        fn validate(content) {
            let doc = parse_json(content);
            if doc.root == () { return "an a2ui document needs a `root` node"; }
            ()
        }
        "#,
    );
    let mut w = win();
    let (message, output) = handle_output_tool(
        &json!({"content": r#"{"nope":1}"#}),
        &OutputContext {
            spec: Some(&spec_with_validator("a2ui")),
            validators: Some(&vals),
            stage: "summary",
            stage_names: &[],
            workdir: None,
        },
        0,
        &mut w,
    );
    assert!(output.is_none(), "nothing recorded");
    assert!(message.contains("needs a `root` node"), "{message}");
}

#[test]
fn an_agent_supplied_validator_accepts_a_good_answer() {
    let vals = validators_with(
        r#"
        fn validate(content) {
            let doc = parse_json(content);
            if doc.root == () { return "missing root"; }
            ()
        }
        "#,
    );
    let mut w = win();
    let (_, output) = handle_output_tool(
        &json!({"content": r#"{"root":{"component":"Card"}}"#}),
        &OutputContext {
            spec: Some(&spec_with_validator("a2ui")),
            validators: Some(&vals),
            stage: "summary",
            stage_names: &[],
            workdir: None,
        },
        0,
        &mut w,
    );
    assert!(output.is_some());
}

/// A broken validator must not read as "every answer is wrong". That would burn
/// the retry budget on a script bug and end the run with nothing at all.
#[test]
fn a_broken_validator_records_the_submission_unchecked() {
    let vals = validators_with(r#"fn validate(content) { throw "the script is broken" }"#);
    let mut w = win();
    let (_, output) = handle_output_tool(
        &json!({"content": "the agent's perfectly good answer"}),
        &OutputContext {
            spec: Some(&spec_with_validator("a2ui")),
            validators: Some(&vals),
            stage: "summary",
            stage_names: &[],
            workdir: None,
        },
        0,
        &mut w,
    );
    assert!(
        output.is_some(),
        "a script bug must not cost the agent its answer"
    );
    assert_eq!(
        vals.broken_names(),
        vec!["v.rhai".to_string()],
        "and the run records which script it could not use, so the failure is \
         not only a line in the daemon log"
    );
}

/// Once per script, not once per retry. A validator that throws throws every
/// time, so a stage that submits three corrections would otherwise report the
/// same broken script three times.
#[test]
fn a_broken_validator_is_recorded_once_however_often_it_is_hit() {
    let vals = validators_with(r#"fn validate(content) { throw "still broken" }"#);
    let mut w = win();
    for _ in 0..3 {
        handle_output_tool(
            &json!({"content": "an answer"}),
            &OutputContext {
                spec: Some(&spec_with_validator("a2ui")),
                validators: Some(&vals),
                stage: "summary",
                stage_names: &[],
                workdir: None,
            },
            0,
            &mut w,
        );
    }
    assert_eq!(vals.broken_names(), vec!["v.rhai".to_string()]);
}

/// A validator that works records nothing: the flag means "a script this run
/// needed could not be used", and a clean run has to be able to say so by
/// carrying an empty list.
#[test]
fn a_working_validator_records_nothing() {
    let vals = validators_with("fn validate(content) { () }");
    let mut w = win();
    handle_output_tool(
        &json!({"content": "an answer"}),
        &OutputContext {
            spec: Some(&spec_with_validator("a2ui")),
            validators: Some(&vals),
            stage: "summary",
            stage_names: &[],
            workdir: None,
        },
        0,
        &mut w,
    );
    assert!(vals.broken_names().is_empty());
}

/// A blueprint naming a validator that was never compiled (a caller overrode
/// the format, so it was retired) simply does not run one.
#[test]
fn a_named_validator_with_nothing_compiled_is_skipped() {
    let mut w = win();
    let (_, output) = handle_output_tool(
        &json!({"content": "anything"}),
        &OutputContext {
            spec: Some(&spec_with_validator("a2ui")),
            validators: None,
            stage: "summary",
            stage_names: &[],
            workdir: None,
        },
        0,
        &mut w,
    );
    assert!(output.is_some());
}

/// A region too small even for the marker cannot hold a preview at all, and a
/// bare marker would say "this was cut" about nothing. An empty mirror is the
/// honest result; the answer itself is on the component either way.
#[test]
fn a_region_smaller_than_the_marker_mirrors_nothing() {
    // One token of room is four bytes, and the marker is far longer.
    assert_eq!(fit_to_region("a long answer that will not fit", 1), "");
}

#[test]
fn a_region_with_room_keeps_what_it_can_and_says_it_was_cut() {
    let fitted = fit_to_region(&"x".repeat(10_000), 100);
    assert!(fitted.starts_with("xxxx"), "the answer's front survives");
    assert!(fitted.ends_with(MIRROR_TRUNCATION_MARKER), "and it says so");
    assert!(
        fitted.len() <= 400,
        "within the region's four-bytes-a-token"
    );
}

#[test]
fn an_answer_that_fits_is_mirrored_whole() {
    assert_eq!(fit_to_region("short", 100), "short");
}

/// Everything a submission is judged against, with no blueprint behind it.
fn ctx<'a>(stage: &'a str, stage_names: &'a [String]) -> OutputContext<'a> {
    OutputContext {
        spec: None,
        validators: None,
        stage,
        stage_names,
        workdir: None,
    }
}

/// The reported failure, reproduced: a dead-ended run completing `complete`
/// with a routing token as its whole deliverable.
///
/// A benchmark run exhausted its report stage's iterations, dead-ended into the
/// output stage with a heavily compacted context, and submitted the literal
/// string `analyze` - one of the blueprint's transition-choice tokens. Every
/// check passed, so the run reported success with a one-word answer, scored 0.0,
/// and sat in a results matrix as finished until a person read it.
#[test]
fn a_transition_token_is_refused_rather_than_recorded_as_the_answer() {
    let mut w = win();
    let stages = ["gather", "analyze", "report"].map(str::to_string);
    let (message, output) = handle_output_tool(
        &json!({"content": "analyze"}),
        &ctx("report", &stages),
        0,
        &mut w,
    );
    assert!(output.is_none(), "a stage name is not an answer");
    assert!(message.starts_with("[error]"), "{message}");
    assert!(message.contains("analyze"), "{message}");
    assert!(message.contains("name of a stage"), "{message}");
    // Nothing reached the region either: a refused submission records nothing.
    assert_eq!(region_text(&w), "");
}

#[test]
fn a_routing_token_is_caught_whatever_its_case_or_padding() {
    // The model is echoing a token it half-remembers, so it arrives however it
    // arrives.
    let mut w = win();
    let stages = ["analyze".to_string()];
    for content in ["Analyze", "  analyze\n", "ANALYZE"] {
        let (message, output) = handle_output_tool(
            &json!({ "content": content }),
            &ctx("report", &stages),
            0,
            &mut w,
        );
        assert!(output.is_none(), "{content} should be refused");
        assert!(message.starts_with("[error]"), "{message}");
    }
}

/// The guard is deliberately narrow: it knows this blueprint's stage names, not
/// "short answers are suspicious".
#[test]
fn a_one_word_answer_that_is_not_a_stage_name_is_still_an_answer() {
    // A classifier answering `positive`, a yes/no question. Refusing these to
    // catch the routing case would break working agents.
    let mut w = win();
    let stages = ["gather", "analyze"].map(str::to_string);
    let (_, output) = handle_output_tool(
        &json!({"content": "positive"}),
        &ctx("classify", &stages),
        0,
        &mut w,
    );
    assert_eq!(
        output.expect("a one-word answer is accepted").content,
        "positive"
    );
}

/// A submission that merely *contains* a stage name is untouched: the guard is
/// for a reply that is nothing but the token.
#[test]
fn a_real_answer_mentioning_a_stage_name_is_recorded() {
    let mut w = win();
    let stages = ["analyze".to_string()];
    let content = "The analyze stage found three regressions, listed below.";
    let (_, output) = handle_output_tool(
        &json!({ "content": content }),
        &ctx("report", &stages),
        0,
        &mut w,
    );
    assert_eq!(output.expect("accepted").content, content);
}

/// With no blueprint in hand there is nothing to compare against, and the
/// submission is taken at face value - the pre-existing behaviour.
#[test]
fn without_stage_names_a_submission_is_accepted_as_before() {
    let mut w = win();
    let (_, output) = handle_output_tool(
        &json!({"content": "analyze"}),
        &ctx("report", &[]),
        0,
        &mut w,
    );
    assert_eq!(output.expect("accepted").content, "analyze");
}
