use super::*;
use crate::commands::validate::tests::{
    LAYOUT, bad_entry_manifest, clean_manifest, write_manifest,
};

/// The graph of a blueprint's text, as `check` reads it.
fn graph_of(text: &str) -> RunGraph {
    BlueprintFile::parse(text)
        .expect("the fixture parses")
        .run_graph()
}

/// A reviewer-shaped blueprint: no task input, one required input that
/// seeds a region of another name, one optional renamed input, and one
/// optional input that seeds its own region. Together they exercise every
/// annotation the formatter has.
pub(in crate::commands::validate) fn named_inputs_manifest() -> String {
    format!(
        r#"
[blueprint]
name = "inputs-agent"
version = "0.1.0"
description = "Named inputs"

[graph]
inputs = [
    {{ name = "diff", type = "text", required = true, binds = [{{ region = "patch" }}] }},
    {{ name = "criteria", type = "text", binds = [{{ region = "review_criteria" }}] }},
    {{ name = "focus", type = {{ kind = "int", min = 1 }}, binds = [{{ region = "focus" }}, {{ stage_max_iterations = "main" }}] }},
]
{LAYOUT}

[[graph.stages]]
name = "main"
model = {{ models = [{{ provider = "anthropic", model = "claude-sonnet-5" }}] }}
max_iterations = 5
"#
    )
    .replace(
        "regions = [",
        "regions = [{ name = \"patch\", kind = \"pinned\", budget = 2000 }, \
         { name = \"review_criteria\", kind = \"pinned\", budget = 1000 }, \
         { name = \"focus\", kind = \"pinned\", budget = 500 }, ",
    )
}

// ─── blueprint_path / check ──────────────────────────────────────────────

#[test]
fn a_path_names_a_blueprint_by_its_file_or_its_directory() {
    let dir = tempfile::tempdir().unwrap();
    let file = write_manifest(dir.path(), &clean_manifest());
    assert_eq!(blueprint_path(dir.path()), file);
    assert_eq!(blueprint_path(&file), file);
    let missing = dir.path().join("nowhere");
    assert_eq!(blueprint_path(&missing), missing.join(FILE_NAME));
}

impl CheckError {
    /// Which kind of failure this is, for an assertion. A method rather than
    /// an inline `matches!`, whose untaken arm reads as uncovered.
    fn kind(&self) -> &'static str {
        match self {
            Self::Io(_) => "io",
            Self::Parse(_) => "parse",
            Self::Validation(_) => "validation",
        }
    }

    /// The message, for an assertion.
    fn text(&self) -> String {
        match self {
            Self::Io(e) => e.to_string(),
            Self::Parse(e) | Self::Validation(e) => e.clone(),
        }
    }
}

#[test]
fn a_checked_blueprint_carries_its_file_graph_and_directory() {
    let dir = tempfile::tempdir().unwrap();
    write_manifest(dir.path(), &clean_manifest());
    let checked = check(dir.path()).unwrap();
    assert_eq!(checked.file.blueprint.name.as_str(), "ok-agent");
    // The graph takes its title from the blueprint's name.
    assert_eq!(checked.graph.title.as_deref(), Some("ok-agent"));
    assert_eq!(checked.agent_dir, dir.path());
}

#[test]
fn a_missing_blueprint_is_an_io_failure() {
    let dir = tempfile::tempdir().unwrap();
    let err = check(dir.path()).unwrap_err();
    assert_eq!(err.kind(), "io");
    assert!(err.text().contains("No agent.toml found"), "{}", err.text());
}

/// A blueprint an earlier release wrote, named by its directory or its
/// file, says how to convert it rather than "not found" or a parse error.
#[test]
fn an_old_manifest_says_how_to_convert_it() {
    let dir = tempfile::tempdir().unwrap();
    let old = dir.path().join("agent.leviath");
    std::fs::write(&old, "[agent]\nname = \"old\"\n").unwrap();
    for path in [dir.path(), old.as_path()] {
        let err = check(path).unwrap_err();
        assert_eq!(err.kind(), "io");
        assert!(
            err.text().contains("lev blueprint migrate"),
            "{}",
            err.text()
        );
    }
}

/// A blueprint that exists but cannot be read (here, a directory where the
/// file should be) is an I/O failure too, not a parse error.
#[test]
fn an_unreadable_blueprint_is_an_io_failure() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(FILE_NAME)).unwrap();
    assert_eq!(check(dir.path()).unwrap_err().kind(), "io");
}

#[test]
fn malformed_toml_and_an_unknown_key_are_parse_failures() {
    let dir = tempfile::tempdir().unwrap();
    write_manifest(dir.path(), "not valid toml [[[");
    assert_eq!(check(dir.path()).unwrap_err().kind(), "parse");
    // An agent.toml refuses a key it does not know, and says which.
    write_manifest(
        dir.path(),
        &clean_manifest().replace(
            "max_iterations = 5",
            "max_iterations = 5\navailable_tools = []",
        ),
    );
    let err = check(dir.path()).unwrap_err();
    assert_eq!(err.kind(), "parse");
    assert!(err.text().contains("available_tools"), "{}", err.text());
}

/// Every problem the graph has is listed, one per line, each with its path.
#[test]
fn every_problem_with_a_graph_is_listed_with_its_path() {
    let dir = tempfile::tempdir().unwrap();
    write_manifest(
        dir.path(),
        &bad_entry_manifest().replace(
            "max_iterations = 5",
            "max_iterations = 5\nhide = [\"nope\"]",
        ),
    );
    let err = check(dir.path()).unwrap_err();
    assert_eq!(err.kind(), "validation");
    let text = err.text();
    assert!(text.contains("problem(s):"), "{text}");
    let issues: Vec<&str> = text.lines().skip(1).collect();
    assert!(issues.len() >= 2, "{text}");
    assert!(issues.iter().all(|l| l.starts_with("  graph.")), "{text}");
    assert!(
        text.contains("does-not-exist") && text.contains("nope"),
        "{text}"
    );
}

/// A custom region's script must exist and compile: the same failure a
/// spawn would hit, surfaced by `lev validate`.
#[test]
fn custom_region_scripts_are_checked() {
    let dir = tempfile::tempdir().unwrap();
    let text = clean_manifest().replace(
        "regions = [",
        "regions = [{ name = \"brain\", kind = { kind = \"custom\", code = { file = \"hooks/brain.rhai\" } }, budget = 1000 }, ",
    );
    write_manifest(dir.path(), &text);
    let err = check(dir.path()).unwrap_err();
    assert_eq!(err.kind(), "validation");
    assert!(err.text().contains("brain"), "{}", err.text());

    std::fs::create_dir(dir.path().join("hooks")).unwrap();
    std::fs::write(
        dir.path().join("hooks/brain.rhai"),
        "fn render(ctx) { \"ok\" }",
    )
    .unwrap();
    assert!(check(dir.path()).is_ok());
}

/// Output validators are compiled the way a spawn compiles them, so one that
/// does not compile fails `lev validate` rather than the end of a run.
#[test]
fn an_output_validator_that_does_not_compile_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("shape.rhai"), "fn validate(content) { ][ }").unwrap();
    write_manifest(
        dir.path(),
        &clean_manifest().replace(
            "max_iterations = 5",
            "max_iterations = 5\noutput = { format = \"a2ui\", validator = { file = \"shape.rhai\" } }",
        ),
    );
    let err = check(dir.path()).unwrap_err();
    assert!(err.text().contains("output validator"), "{}", err.text());
}

/// A stage hook whose file is not there fails `lev validate` rather than the
/// run.
#[test]
fn a_stage_hook_script_that_is_missing_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    write_manifest(
        dir.path(),
        &clean_manifest().replace(
            "max_iterations = 5",
            "max_iterations = 5\nhooks = { on_stage_enter = { file = \"missing.rhai\" } }",
        ),
    );
    let err = check(dir.path()).unwrap_err();
    assert!(err.text().contains("stage hook"), "{}", err.text());
}

// ─── input_lines / input_summaries ───────────────────────────────────────

/// Validate says what `lev run` would accept, including that `--task` is not
/// among the flags.
#[test]
fn input_lines_name_every_input_and_the_missing_task() {
    let lines = input_lines(&graph_of(&named_inputs_manifest()));
    assert_eq!(
        lines,
        vec![
            "  Inputs: --diff (text, required, seeds region 'patch'), \
             --criteria (text, seeds region 'review_criteria'), --focus (an integer at least 1)"
                .to_string(),
            "  Note: this agent takes no --task; give it input via --diff, \
             --criteria, --focus"
                .to_string(),
        ]
    );
}

#[test]
fn input_lines_of_a_task_taking_agent_skip_the_refusal_note() {
    let text = clean_manifest().replace(
        "[graph]\n",
        "[graph]\ninputs = [{ name = \"task\", type = \"text\", required = true, binds = [{ region = \"system\" }] }]\n",
    );
    let lines = input_lines(&graph_of(&text));
    assert_eq!(
        lines,
        vec!["  Inputs: --task (text, required, seeds region 'system')".to_string()],
        "an agent that takes a task needs no note about refusing one"
    );
}

#[test]
fn input_lines_without_any_input_say_so() {
    assert_eq!(
        input_lines(&graph_of(&clean_manifest())),
        vec!["  Inputs: none - this agent takes no --task or other caller input".to_string()]
    );
}

/// The summaries feed the JSON report. An input with a default is never
/// missing, so it is not required whatever it says.
#[test]
fn input_summaries_carry_key_type_regions_and_required() {
    let text = named_inputs_manifest().replace(
        "{ name = \"criteria\", type = \"text\", ",
        "{ name = \"criteria\", type = \"text\", required = true, default = { text = \"be kind\" }, ",
    );
    let summaries = input_summaries(&graph_of(&text));
    assert_eq!(
        summaries,
        vec![
            InputSummary {
                key: "diff".to_string(),
                region: Some("patch".to_string()),
                kind: "text".to_string(),
                regions: vec!["patch".to_string()],
                required: true,
            },
            InputSummary {
                key: "criteria".to_string(),
                region: Some("review_criteria".to_string()),
                kind: "text".to_string(),
                regions: vec!["review_criteria".to_string()],
                required: false,
            },
            InputSummary {
                key: "focus".to_string(),
                region: Some("focus".to_string()),
                kind: "an integer at least 1".to_string(),
                regions: vec!["focus".to_string()],
                required: false,
            },
        ]
    );
}

// ─── mime_lines ──────────────────────────────────────────────────────────

#[test]
fn mime_lines_say_what_each_stage_takes_and_hands_back() {
    let stage = |name: &str, extra: &str| {
        format!(
            "\n[[graph.stages]]\nname = \"{name}\"\n\
             model = {{ models = [{{ provider = \"anthropic\", model = \"claude-sonnet-4-6\" }}] }}\n\
             max_iterations = 5\n{extra}"
        )
    };
    let text = format!(
        r#"
[blueprint]
name = "test"
version = "0.1.0"

[graph]
{LAYOUT}

[graph.mime_types."application/x-acme-scene"]
family = "model"
check = {{ file = "checks/scene.rhai" }}

[graph.mime_types."image/gif"]
check = {{ inline = "fn check(bytes, mime_type) {{ true }}" }}

[graph.mime_types."model/obj"]
text = true
{}{}{}{}"#,
        stage("plan", ""),
        stage(
            "cut",
            "input_accepts = [\"audio/*\", \"image/*\"]\ninput_as_text = [\"model/obj\"]\n\
             tool_accepts = { spawn_agent = [\"image/*\"] }\n\
             output = { artifacts = [{ name = \"final\", mime_type = \"video/mp4\", required = true }, \
             { name = \"notes\", mime_type = \"text/*\" }] }\n"
        ),
        stage(
            "ship",
            "output = { artifacts = [{ name = \"bundle\", mime_type = \"application/zip\" }] }\n"
        ),
        stage("hear", "input_accepts = [\"audio/*\"]\n"),
    )
    // The regions every stage sees take text only, so a stage that declares
    // nothing takes nothing beyond it.
    .replace(
        "kind = \"pinned\", budget = 1000 }",
        "kind = \"pinned\", budget = 1000, accepts = [\"text/plain\"] }",
    )
    .replace(
        "max_items = 50 }, budget = 10000 }",
        "max_items = 50 }, budget = 10000, accepts = [\"text/*\"] }",
    );
    let lines = mime_lines(&graph_of(&text));
    assert_eq!(
        lines,
        vec![
            "  Mime types: adds 3 rows for its runs: application/x-acme-scene (check \
             checks/scene.rhai), image/gif (check inline), model/obj"
                .to_string(),
            "  Mime, stage 'cut': takes audio/*, image/*; as text: model/obj; hands back \
             final (video/mp4, required), notes (text/*); limits spawn_agent to [image/*]"
                .to_string(),
            "  Mime, stage 'ship': hands back bundle (application/zip)".to_string(),
            "  Mime, stage 'hear': takes audio/*".to_string(),
        ]
    );
}

/// A region with no `accepts` takes anything, which is not worth a line; one
/// that takes a type says so, and a region the stage hides does not count.
#[test]
fn a_stage_takes_what_its_visible_regions_accept() {
    let text = clean_manifest()
        .replace(
            "regions = [",
            "regions = [{ name = \"pics\", kind = \"clearable\", budget = 1000, accepts = [\"image/*\"] }, \
             { name = \"tapes\", kind = \"clearable\", budget = 1000, accepts = [\"audio/*\", \"image/*\"] }, ",
        )
        .replace("max_iterations = 5", "max_iterations = 5\nhide = [\"tapes\"]");
    let graph = graph_of(&text);
    assert_eq!(
        mime_lines(&graph),
        vec!["  Mime, stage 'main': takes image/*".to_string()]
    );
    // One mime row reads as a row.
    let one = clean_manifest().replace(
        "[[graph.stages]]",
        "[graph.mime_types.\"image/gif\"]\ntext = false\n\n[[graph.stages]]",
    );
    assert_eq!(
        mime_lines(&graph_of(&one)),
        vec!["  Mime types: adds 1 row for its runs: image/gif".to_string()]
    );
}

// ─── graph_lines / success_lines ─────────────────────────────────────────

#[test]
fn graph_lines_draw_each_stage_its_edges_and_its_revisit_cap() {
    let text = format!(
        r#"
[blueprint]
name = "g"
version = "0.1.0"

[graph]
entry = "b"
{LAYOUT}
edges = [
    {{ name = "b", from = "a", to = "b" }},
    {{ name = "a", from = "b", to = "a", when = "error" }},
    {{ name = "c", from = "b", to = "c", when = "max_iterations" }},
    {{ name = "c2", from = "a", to = "c", when = "llm_choice" }},
    {{ name = "c3", from = "c", to = "a", when = "dead_end" }},
    {{ name = "c4", from = "c", to = "b", when = "stuck", stuck = {{ after_iterations = 3 }} }},
]
stages = [{{ name = "a", max_revisits = 3 }}, {{ name = "b" }}, {{ name = "c" }}, {{ name = "d" }}]
"#
    );
    assert_eq!(
        graph_lines(&graph_of(&text)),
        vec![
            "  Entry stage: 'b'".to_string(),
            "  - a → b, c [llm_choice] (max_revisits: 3)".to_string(),
            "  - b → a [error], c [max_iterations]".to_string(),
            "  - c → a [dead_end], b [stuck]".to_string(),
            "  - d (terminal)".to_string(),
        ]
    );
    // A graph with no stages has no entry to name.
    let empty = graph_of(
        "[blueprint]\nname = \"e\"\nversion = \"1\"\n\n[graph]\nstages = []\nlayout = { total_budget_tokens = 0, regions = [] }\n",
    );
    assert_eq!(graph_lines(&empty), vec!["  Entry stage: ''".to_string()]);
}

#[test]
fn success_lines_name_the_blueprint_and_gather_every_part() {
    let dir = tempfile::tempdir().unwrap();
    write_manifest(dir.path(), &named_inputs_manifest());
    let lines = success_lines(&check(dir.path()).unwrap());
    assert_eq!(lines[0], "✓ Blueprint 'inputs-agent' is valid.");
    assert_eq!(lines[1], "  1 stages, version 0.1.0");
    assert!(
        lines.iter().any(|l| l.starts_with("  Inputs: --diff")),
        "{lines:#?}"
    );
    assert!(
        lines.iter().any(|l| l == "  - main (terminal)"),
        "{lines:#?}"
    );
}
