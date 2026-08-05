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

/// The answer points at what it could never contain, so the files it names are
/// listed rather than left for the reader to find in the prose.
#[test]
fn artifacts_are_listed_under_the_answer() {
    let answer = leviath_core::output::FinalOutput::new(
        "Revenue grew 12% over the period.",
        Some("markdown".to_string()),
        "present".to_string(),
        42,
    )
    .with_artifacts(vec![
        "data/dataset.csv".to_string(),
        "charts/trend.svg".to_string(),
    ]);

    let out = shown(Some(&answer), false, false).expect("there is an answer");

    assert!(out.contains("Files produced (2):"), "{out}");
    assert!(out.contains("  data/dataset.csv\n"), "{out}");
    assert!(out.contains("  charts/trend.svg\n"), "{out}");
}

/// `--raw` is for a shell pipeline, so it is the answer and nothing else: no
/// heading, and no file list to parse back off.
#[test]
fn raw_output_carries_no_file_list() {
    let answer =
        leviath_core::output::FinalOutput::new("just the answer", None, "present".to_string(), 42)
            .with_artifacts(vec!["data/dataset.csv".to_string()]);

    let out = shown(Some(&answer), false, true).expect("there is an answer");

    assert_eq!(out, "just the answer\n");
}

/// End to end over the real files: `meta.json` says there is an answer, the
/// sidecar beside it holds the bytes. Both have to line up, because a run whose
/// descriptor says yes and whose sidecar is missing reads as no answer at all.
#[tokio::test]
async fn execute_prints_an_answer_written_to_a_run_directory() {
    crate::runstate::with_isolated_runs_dir_async("result-execute", |_| async {
        let mut meta = crate::runstate::RunMeta::new(
            "run-answered".to_string(),
            "agent".to_string(),
            "/p".to_string(),
            "t".to_string(),
            None,
            "/w".to_string(),
            1,
        );
        let answer = leviath_core::output::FinalOutput::new(
            "the answer",
            Some("markdown".to_string()),
            "present".to_string(),
            42,
        );
        meta.final_output = Some(answer.descriptor());
        crate::runstate::create_run(&meta).expect("run dir");
        crate::runstate::write_final_output(
            &crate::runstate::run_dir("run-answered"),
            &answer.content,
        )
        .expect("sidecar");

        execute(ResultArgs {
            run_id: "run-answered".to_string(),
            json: false,
            raw: true,
        })
        .await
        .expect("the answer is there to print");
    })
    .await;
}

/// A run that never submitted exits non-zero rather than printing nothing, so
/// `lev result <id> > answer.txt` does not silently write an empty file.
#[tokio::test]
async fn execute_fails_when_the_run_never_answered() {
    crate::runstate::with_isolated_runs_dir_async("result-no-answer", |_| async {
        let meta = crate::runstate::RunMeta::new(
            "run-silent".to_string(),
            "agent".to_string(),
            "/p".to_string(),
            "t".to_string(),
            None,
            "/w".to_string(),
            1,
        );
        crate::runstate::create_run(&meta).expect("run dir");

        let err = execute(ResultArgs {
            run_id: "run-silent".to_string(),
            json: false,
            raw: false,
        })
        .await
        .expect_err("no answer is a failure exit");
        assert!(
            err.to_string().contains("produced no final output"),
            "{err}"
        );
    })
    .await;
}

/// And an unknown run says so, rather than reporting it as answerless.
#[tokio::test]
async fn execute_fails_for_a_run_that_does_not_exist() {
    crate::runstate::with_isolated_runs_dir_async("result-missing", |_| async {
        let err = execute(ResultArgs {
            run_id: "no-such-run".to_string(),
            json: false,
            raw: false,
        })
        .await
        .expect_err("an unknown run is an error");
        assert!(err.to_string().contains("no run 'no-such-run'"), "{err}");
    })
    .await;
}

/// An answer that already ends in a newline is not given a second one, or every
/// `lev result` would print a blank line the agent never wrote.
#[test]
fn an_answer_ending_in_a_newline_is_not_given_another() {
    let out = shown(Some(&answer("already terminated\n", None)), false, false)
        .expect("there is an answer");
    assert!(out.ends_with("already terminated\n"), "{out:?}");
    assert!(!out.ends_with("\n\n"), "{out:?}");
}
