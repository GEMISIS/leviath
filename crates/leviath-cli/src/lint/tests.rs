use super::*;

/// The two regions every fixture's layout starts with. Further regions may be
/// appended after it as `[[graph.layout.regions]]` tables.
const LAYOUT: &str = r#"
[graph.layout]
total_budget_tokens = 0

[[graph.layout.regions]]
name = "system"
kind = "pinned"
budget = 1000

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 50 }
budget = 10000
"#;

/// Wrap `graph_toml` (graph-level keys first, then stage and edge tables) in
/// the smallest blueprint that parses, with [`LAYOUT`] after it.
fn manifest(graph_toml: &str) -> String {
    format!(
        "[blueprint]\nname = \"lint-fixture\"\nversion = \"0.1.0\"\ndescription = \"a fixture\"\n\n\
         [graph]\n{graph_toml}\n{LAYOUT}"
    )
}

/// [`manifest`] with a `task` input bound to a region of its own, which a
/// blueprint that runs its own fan-out workers must have: each work item's
/// inputs land there.
fn manifest_taking_task(graph_toml: &str) -> String {
    format!(
        "{}\n[[graph.inputs]]\nname = \"task\"\ntype = \"text\"\nbinds = [{{ region = \"task\" }}]\n\n\
         [[graph.layout.regions]]\nname = \"task\"\nkind = \"pinned\"\nbudget = 1000\n",
        manifest(graph_toml)
    )
}

impl LintEnv {
    /// A default env that also knows how big the shipped models' windows are,
    /// which is what the percentage-budget check needs to say a number.
    fn default_with_windows() -> Self {
        Self {
            model_windows: crate::commands::models::builtin_model_windows(),
            ..Self::default()
        }
    }
}

/// Read an `agent.toml` fixture.
fn parse(content: &str) -> BlueprintFile {
    BlueprintFile::parse(content).unwrap_or_else(|e| panic!("fixture parses: {e}\n{content}"))
}

/// Lint an `agent.toml` fixture.
fn lint(content: &str, env: &LintEnv) -> Vec<LintFinding> {
    lint_blueprint(&parse(content), env)
}

/// The graph of an `agent.toml` fixture, for the builders.
fn graph_of(content: &str) -> RunGraph {
    parse(content).run_graph()
}

/// The codes reported, in order, so a test can assert on the whole outcome
/// rather than on one finding it went looking for.
fn codes(findings: &[LintFinding]) -> Vec<&'static str> {
    findings.iter().map(|f| f.code).collect()
}

/// Every finding carrying `code`.
fn with_code<'a>(findings: &'a [LintFinding], code: &str) -> Vec<&'a LintFinding> {
    findings.iter().filter(|f| f.code == code).collect()
}

/// A blueprint row that changes what a built-in type is warns; one that only
/// adds to it, or describes a type of the agent's own, does not.
#[test]
fn a_mime_row_that_changes_a_builtin_type_is_flagged() {
    let text = format!(
        "{}\n[graph.mime_types.\"image/png\"]\nfamily = \"model\"\ntext = true\n\
         [graph.mime_types.\"image/webp\"]\nextensions = [\"webp\", \"wbp\"]\n\
         [graph.mime_types.\"application/x-acme-scene\"]\nfamily = \"model\"\n\
         [graph.mime_types.\"model/*\"]\ntext = true\n\
         [graph.mime_types.\"model/obj\"]\ntext = true\n",
        manifest(CLEAN_STAGE)
    );
    let findings = lint(&text, &LintEnv::default());
    let said = with_code(&findings, "mime-type-overrides-builtin");
    // `image/png` and the `model/*` family row, whose text flag the compiled
    // table sets; `model/obj` already reads as text, and the rest add to
    // their types or describe a new one.
    let messages: Vec<&str> = said.iter().map(|f| f.message.as_str()).collect();
    assert_eq!(said.len(), 2, "{messages:?}");
    assert!(
        messages[0].contains("image/png")
            && messages[0].contains("family from image to model; text from false to true"),
        "{messages:?}"
    );
    assert!(
        messages[1].contains("model/*") && messages[1].contains("text from false to true"),
        "{messages:?}"
    );
}

/// A stage that declares everything the linter looks for, so a test can add a
/// single defect and see only that.
const CLEAN_STAGE: &str = r#"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
description = "Main"
max_iterations = 10
tools = ["read_file"]
"#;

fn known_tools(names: &[&str]) -> HashSet<String> {
    names.iter().map(|n| (*n).to_string()).collect()
}

// ─── Nothing to report ────────────────────────────────────────────────────────

#[test]
fn a_fully_declared_stage_reports_nothing() {
    let findings = lint(&manifest(CLEAN_STAGE), &LintEnv::default());
    assert!(findings.is_empty(), "{:?}", codes(&findings));
}

/// An empty [`LintEnv`] is "I don't know", and an unknown fact must never
/// become a finding: no tool catalog means no unknown-tool errors, no model
/// catalog means no unknown-model warnings.
#[test]
fn an_empty_env_skips_every_environment_dependent_check() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "madeup", model = "no-such-model" }] }
max_iterations = 10
tools = ["raed_file"]
"#,
    );
    let findings = lint(&toml, &LintEnv::default());
    assert!(findings.is_empty(), "{:?}", codes(&findings));
}

// ─── Declaration checks ───────────────────────────────────────────────────────

/// The defect from the report: no models listed, so the stage silently runs on
/// the user's default provider.
#[test]
fn a_stage_with_no_model_is_warned_about() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
max_iterations = 10
"#,
    );
    let findings = lint(&toml, &LintEnv::default());
    assert_eq!(codes(&findings), ["stage-missing-model"]);
    assert!(
        findings[0].message.contains("default_provider"),
        "{findings:?}"
    );
    let fix = findings[0].fix.as_deref().expect("the fix names the stage");
    assert!(fix.contains("'main'") && fix.contains("models"), "{fix}");
}

#[test]
fn a_stage_with_no_max_iterations_is_warned_about() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
"#,
    );
    let findings = lint(&toml, &LintEnv::default());
    assert_eq!(codes(&findings), ["stage-missing-max-iterations"]);
    assert!(
        findings[0].message.contains("default_max_iterations"),
        "{findings:?}"
    );
}

/// A fan_out stage runs no inference of its own, so it has no iteration count
/// to cap and must not be nagged for one.
#[test]
fn a_fan_out_stage_needs_no_max_iterations() {
    let toml = manifest_taking_task(
        r#"

[[graph.stages]]
name = "split"
mode = { fan_out = { worker = { stage = "work" } } }
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }

[[graph.stages]]
name = "work"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
allow_as_worker = true

[[graph.stages]]
name = "recover"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5

[[graph.edges]]
name = "recover"
from = "split"
to = "recover"
when = "error"
"#,
    );
    let findings = lint(&toml, &LintEnv::default());
    assert!(findings.is_empty(), "{:?}", codes(&findings));
}

/// `fail_all` with nowhere to go means one flaky worker ends the run.
#[test]
fn a_fail_all_fan_out_without_an_escape_is_warned_about() {
    let toml = manifest_taking_task(
        r#"

[[graph.stages]]
name = "split"
mode = { fan_out = { worker = { stage = "work" }, on_worker_failure = "fail_all" } }
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }

[[graph.stages]]
name = "work"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
allow_as_worker = true
"#,
    );
    let findings = lint(&toml, &LintEnv::default());
    assert_eq!(codes(&findings), ["fanout-no-escape"]);
}

/// A fan-out to a blueprint that will not load is an error, as it is at
/// spawn. One that loads, a fan-out to a stage, and an install nobody asked
/// about say nothing.
#[test]
fn a_fan_out_to_a_blueprint_that_will_not_load_is_an_error() {
    use crate::daemon::resolve_env::tests::{MANIFEST, install};
    let fan_to = |worker: &str| {
        manifest_taking_task(&format!(
            r#"

[[graph.stages]]
name = "split"
mode = {{ fan_out = {{ worker = {worker} }} }}
model = {{ models = [{{ provider = "anthropic", model = "claude-sonnet-5" }}] }}

[[graph.stages]]
name = "after"
model = {{ models = [{{ provider = "anthropic", model = "claude-sonnet-5" }}] }}
"#
        ))
    };
    let agents = tempfile::tempdir().unwrap();
    install(&agents, "helper", MANIFEST);
    let env_for =
        |toml: &str| LintEnv::default().with_workers(&graph_of(toml), Some(agents.path()));

    let ghost = fan_to(r#"{ blueprint = { name = "ghost" } }"#);
    let found = lint(&ghost, &env_for(&ghost));
    let missing = with_code(&found, "fanout-worker-missing");
    assert_eq!(missing.len(), 1, "{:?}", codes(&found));
    assert_eq!(missing[0].severity, LintSeverity::Error);
    assert!(
        missing[0].message.contains("'ghost'"),
        "{}",
        missing[0].message
    );

    let helper = fan_to(r#"{ blueprint = { name = "helper" } }"#);
    assert!(with_code(&lint(&helper, &env_for(&helper)), "fanout-worker-missing").is_empty());
    // A worker picked at run time has no name to load until then.
    let query = fan_to(r#"{ query = "a researcher" }"#);
    assert!(with_code(&lint(&query, &env_for(&query)), "fanout-worker-missing").is_empty());
    let unasked = lint(&ghost, &LintEnv::default());
    assert!(with_code(&unasked, "fanout-worker-missing").is_empty());
}

/// A work item's inputs land in the regions the graph binds them to, so a
/// graph that runs its own workers and binds no input leaves every worker
/// without its work - and the run still completes, which is why the lint is
/// an error rather than a warning.
#[test]
fn a_fan_out_on_a_graph_with_no_bound_input_is_an_error() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "split"
mode = { fan_out = { worker = { stage = "work" } } }
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }

[[graph.stages]]
name = "work"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
allow_as_worker = true
"#,
    );
    let findings = lint(&toml, &LintEnv::default());
    assert_eq!(codes(&findings), ["fanout-worker-task-unheld"]);
    let finding = &findings[0];
    assert_eq!(finding.severity, LintSeverity::Error);
    assert_eq!(finding.stage.as_deref(), Some("split"));
    assert!(finding.message.contains("stage 'work'"), "{finding:?}");
    assert!(
        finding
            .fix
            .as_deref()
            .is_some_and(|f| f.contains("binds = [{ region = \"task\" }]")),
        "{finding:?}"
    );
}

/// A worker that runs another blueprint is linted when that one is validated
/// itself; what *this* graph binds says nothing about it.
#[test]
fn a_fan_out_onto_another_agent_is_not_held_to_this_blueprints_regions() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "split"
mode = { fan_out = { worker = { blueprint = { name = "some-other-agent" } } } }
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
"#,
    );
    let findings = lint(&toml, &LintEnv::default());
    assert!(
        !codes(&findings).contains(&"fanout-worker-task-unheld"),
        "{:?}",
        codes(&findings)
    );
}

/// The default policy merges what succeeded, so there is nothing to escape from
/// and nothing to say.
#[test]
fn a_continuing_fan_out_needs_no_escape() {
    let toml = manifest_taking_task(
        r#"

[[graph.stages]]
name = "split"
mode = { fan_out = { worker = { stage = "work" } } }
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }

[[graph.stages]]
name = "work"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
allow_as_worker = true
"#,
    );
    let findings = lint(&toml, &LintEnv::default());
    assert!(findings.is_empty(), "{:?}", codes(&findings));
}

/// A `dead_end` edge answers the same question - "this stage may not be able to
/// go on" - so it satisfies the check too.
#[test]
fn a_dead_end_edge_satisfies_the_fan_out_escape_check() {
    let toml = manifest_taking_task(
        r#"

[[graph.stages]]
name = "split"
mode = { fan_out = { worker = { stage = "work" }, on_worker_failure = "fail_all" } }
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }

[[graph.stages]]
name = "work"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
allow_as_worker = true

[[graph.stages]]
name = "recover"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5

[[graph.edges]]
name = "recover"
from = "split"
to = "recover"
when = "dead_end"
"#,
    );
    let findings = lint(&toml, &LintEnv::default());
    assert!(findings.is_empty(), "{:?}", codes(&findings));
}

// ─── dead-end-possible ────────────────────────────────────────────────────────

/// A graph whose only way on is a stage with a spendable revisit budget.
fn strandable(extra_edge: &str) -> String {
    format!(
        r#"
[blueprint]
name = "strandable"
version = "0.1.0"
description = "a fixture"

[graph]

[[graph.stages]]
name = "work"
model = {{ models = [{{ provider = "anthropic", model = "claude-sonnet-5" }}] }}
description = "Work"
max_iterations = 10
tools = ["read_file"]

[[graph.stages]]
name = "review"
model = {{ models = [{{ provider = "anthropic", model = "claude-sonnet-5" }}] }}
description = "Review"
max_iterations = 10
max_revisits = 2
tools = ["read_file"]

[[graph.stages]]
name = "answer"
mode = "output"
model = {{ models = [{{ provider = "anthropic", model = "claude-sonnet-5" }}] }}
description = "Answer"
max_iterations = 10

[[graph.edges]]
name = "review"
from = "work"
to = "review"

[graph.layout]
total_budget_tokens = 0

[[graph.layout.regions]]
name = "system"
kind = "pinned"
budget = 1000

[[graph.layout.regions]]
name = "conversation"
kind = {{ kind = "sliding_window", max_items = 50 }}
budget = 10000
{extra_edge}
"#
    )
}

#[test]
fn a_strandable_stage_is_warned_about() {
    let findings = lint(&strandable(""), &LintEnv::default());
    assert!(
        codes(&findings).contains(&"dead-end-possible"),
        "{:?}",
        codes(&findings)
    );
}

/// The remedy the message names has to be one that silences it, or an author
/// who follows the advice literally is left reaching for the other one - which
/// is a route the model can take on every visit.
#[test]
fn a_dead_end_edge_satisfies_the_check() {
    let toml = strandable(
        "\n[[graph.edges]]\nname = \"answer\"\nfrom = \"work\"\nto = \"answer\"\nwhen = \"dead_end\"\n",
    );
    let findings = lint(&toml, &LintEnv::default());
    assert!(
        !codes(&findings).contains(&"dead-end-possible"),
        "the recommended fix should silence it: {:?}",
        codes(&findings)
    );
}

/// An `error` edge is the other escape the runtime consults on this path.
#[test]
fn an_error_edge_also_satisfies_the_check() {
    let toml = strandable(
        "\n[[graph.edges]]\nname = \"answer\"\nfrom = \"work\"\nto = \"answer\"\nwhen = \"error\"\n",
    );
    assert!(!codes(&lint(&toml, &LintEnv::default())).contains(&"dead-end-possible"));
}

/// `max_iterations` does **not**, and the message must not offer it. It fires
/// when a stage burns its iteration budget, which is a different event: on the
/// stranding path `resolve_transition` never consults it, so counting it would
/// silence the warning without preventing the strand.
#[test]
fn a_max_iterations_edge_does_not_satisfy_the_check() {
    let toml = strandable(
        "\n[[graph.edges]]\nname = \"answer\"\nfrom = \"work\"\nto = \"answer\"\nwhen = \"max_iterations\"\n",
    );
    let findings = lint(&toml, &LintEnv::default());
    assert!(
        codes(&findings).contains(&"dead-end-possible"),
        "{:?}",
        codes(&findings)
    );
    let fix = with_code(&findings, "dead-end-possible")[0]
        .fix
        .clone()
        .expect("the finding carries a fix");
    assert!(fix.contains("dead_end"), "{fix}");
    assert!(
        !fix.contains("max_iterations"),
        "it should no longer recommend an inert remedy: {fix}"
    );
}

/// An escape to a stage that can itself run out is no escape, so it does not
/// silence the warning.
#[test]
fn a_dead_end_edge_to_an_exhaustible_stage_does_not_count() {
    let toml = r#"
[blueprint]
name = "strandable"
version = "0.1.0"
description = "a fixture"

[graph]

[[graph.stages]]
name = "work"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
description = "Work"
max_iterations = 10
tools = ["read_file"]

[[graph.stages]]
name = "review"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
description = "Review"
max_iterations = 10
max_revisits = 2
tools = ["read_file"]

[[graph.stages]]
name = "fallback"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
description = "Fallback"
max_iterations = 10
max_revisits = 1
tools = ["read_file"]

[[graph.edges]]
name = "review"
from = "work"
to = "review"

[[graph.edges]]
name = "fallback"
from = "work"
to = "fallback"
when = "dead_end"

[graph.layout]
total_budget_tokens = 0

[[graph.layout.regions]]
name = "system"
kind = "pinned"
budget = 1000

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 50 }
budget = 10000
"#;
    assert!(codes(&lint(toml, &LintEnv::default())).contains(&"dead-end-possible"));
}

// ─── Tool checks ──────────────────────────────────────────────────────────────

#[test]
fn a_misspelled_tool_is_an_error() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
tools = ["read_file", "raed_file"]
"#,
    );
    let env = LintEnv {
        known_tools: known_tools(&["read_file"]),
        ..LintEnv::default()
    };
    let findings = lint(&toml, &env);
    assert_eq!(codes(&findings), ["unknown-tool"]);
    assert!(findings[0].is_error());
    assert!(findings[0].message.contains("raed_file"), "{findings:?}");
}

/// `server__tool` names an MCP tool, which resolves only once that server is
/// installed. That is not a property of the blueprint, so it is never flagged.
#[test]
fn an_mcp_tool_name_is_never_unknown() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
tools = ["github__create_issue"]
"#,
    );
    let env = LintEnv {
        known_tools: known_tools(&["read_file"]),
        ..LintEnv::default()
    };
    assert!(lint(&toml, &env).is_empty());
}

/// A stage granting a whole connector has a tool set nobody can enumerate at
/// lint time - it is whatever that server advertises at spawn, which is the
/// point of naming the server. So a permission that looks orphaned might name
/// a tool the connector grants, and the check has nothing to tell them apart
/// with. Skipped rather than guessed, the same way an MCP tool name is never
/// reported as unknown.
#[test]
fn a_stage_granting_a_connector_does_not_get_orphan_permission_errors() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
tools = ["read_file"]
connectors = ["github"]
[graph.stages.tool_permissions]
create_issue = "ask"
"#,
    );
    assert!(
        lint(&toml, &LintEnv::default()).is_empty(),
        "the permission may well name a tool the connector grants"
    );
}

#[test]
fn a_permission_for_an_ungranted_tool_is_an_error() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
tools = ["read_file"]
[graph.stages.tool_permissions]
write_file = "allow"
"#,
    );
    let findings = lint(&toml, &LintEnv::default());
    assert_eq!(codes(&findings), ["orphan-stage-permission"]);
    assert!(findings[0].is_error());
    assert!(findings[0].message.contains("write_file"), "{findings:?}");
}

#[test]
fn a_permission_for_a_granted_tool_is_fine() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
tools = ["read_file"]
[graph.stages.tool_permissions]
read_file = "allow"
"#,
    );
    assert!(lint(&toml, &LintEnv::default()).is_empty());
}

// ─── Tool groups ──────────────────────────────────────────────────────────────

/// An install that knows one tool of each kind, so a group-aware check can
/// say which group reaches what.
fn grouped_env() -> LintEnv {
    let sources = [
        ("read_file", ToolGroup::Builtin),
        ("shell", ToolGroup::Builtin),
        ("bash", ToolGroup::Builtin),
        ("write_file", ToolGroup::Builtin),
        ("ask_user_text", ToolGroup::Builtin),
        ("ask_user_choice", ToolGroup::Builtin),
        ("ask_user_confirm", ToolGroup::Builtin),
        ("present_for_review", ToolGroup::Builtin),
        ("edit_document", ToolGroup::Builtin),
        ("spawn_agent", ToolGroup::Subagent),
        ("summarize", ToolGroup::Scripts),
    ];
    LintEnv {
        known_tools: known_tools(&sources.map(|(n, _)| n)),
        tool_sources: sources
            .map(|(n, g)| (n.to_string(), g))
            .into_iter()
            .collect(),
        ..LintEnv::default()
    }
}

/// A group token is a grant, not a tool name: `@scripts` is never "not a
/// built-in", and everything it reaches is left to the runtime to enumerate.
#[test]
fn a_group_token_is_not_an_unknown_tool() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
tools = ["@scripts", "@mcp", "read_file"]
"#,
    );
    let findings = lint(&toml, &grouped_env());
    assert!(findings.is_empty(), "{:?}", codes(&findings));
}

/// `Stage::validate` cannot say whether `@scripts` reaches `summarize`; the
/// lint can, given an inventory, and a required tool nothing grants is an
/// error either way.
#[test]
fn a_required_tool_no_group_reaches_is_an_error() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
tools = ["@scripts"]
required_tools = ["summarize", "read_file", "github__create_issue"]
"#,
    );
    let findings = lint(&toml, &grouped_env());
    let missing = with_code(&findings, "required-tool-not-granted");
    let named: Vec<&str> = missing
        .iter()
        .map(|f| {
            f.message
                .split('\'')
                .nth(1)
                .expect("the message quotes the tool")
        })
        .collect();
    assert_eq!(named, ["read_file", "github__create_issue"], "{findings:?}");
    assert!(missing.iter().all(|f| f.is_error()));
    assert!(missing[0].message.contains("@scripts"), "{findings:?}");

    // With no inventory the question cannot be answered, so it is not asked.
    assert!(
        with_code(
            &lint(&toml, &LintEnv::default()),
            "required-tool-not-granted"
        )
        .is_empty()
    );
}

/// A required tool reached by name (either spelling) or by its group is fine,
/// and `@all` reaches everything, MCP names included.
#[test]
fn a_required_tool_a_group_reaches_is_fine() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
tools = ["@builtin", "@scripts", "bash"]
required_tools = ["summarize", "read_file", "shell"]
allow_blocking_tools = true
[graph.stages.tool_permissions]
shell = "ask"
"#,
    );
    let findings = lint(&toml, &grouped_env());
    assert!(findings.is_empty(), "{:?}", codes(&findings));

    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
tools = ["@all"]
required_tools = ["spawn_agent", "github__create_issue"]
allow_blocking_tools = true
[graph.stages.tool_permissions]
shell = "ask"
"#,
    );
    let findings = lint(&toml, &grouped_env());
    assert!(findings.is_empty(), "{:?}", codes(&findings));
}

/// A permission is orphaned only when nothing grants the tool: a group that
/// reaches it counts, and with no inventory to classify by the check stays
/// quiet, as it does under a connector.
#[test]
fn a_permission_a_group_reaches_is_not_orphaned() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
tools = ["@scripts", "@mcp", "read_file"]
[graph.stages.tool_permissions]
summarize = "allow"
github__create_issue = "ask"
write_file = "allow"
"#,
    );
    let findings = lint(&toml, &grouped_env());
    assert_eq!(codes(&findings), ["orphan-stage-permission"]);
    assert!(findings[0].message.contains("write_file"), "{findings:?}");

    assert!(lint(&toml, &LintEnv::default()).is_empty());
}

/// `@builtin` in an autonomous stage carries every blocking tool, reported
/// once as the group; keeping one in `required_tools` takes it off the list,
/// and `allow_blocking_tools` silences it.
#[test]
fn a_builtin_group_in_an_autonomous_stage_is_warned_about_once() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
tools = ["@builtin"]
required_tools = ["ask_user_text"]
[graph.stages.tool_permissions]
shell = "allow"
"#,
    );
    // The required ask tool also earns its own `holds-under-yolo` note, which
    // is not what this test is about.
    let blocking = |toml: &str| -> Vec<LintFinding> {
        lint(toml, &grouped_env())
            .into_iter()
            .filter(|f| f.code != "holds-under-yolo")
            .collect()
    };
    let findings = blocking(&toml);
    assert_eq!(codes(&findings), ["blocking-tool-in-autonomous-stage"]);
    assert_eq!(findings[0].severity, LintSeverity::Warning);
    assert!(findings[0].message.contains("'@builtin'"), "{findings:?}");
    assert!(
        findings[0].message.contains("present_for_review"),
        "{findings:?}"
    );
    assert!(
        !findings[0].message.contains("ask_user_text"),
        "{findings:?}"
    );

    let silenced = blocking(&toml.replace(
        "required_tools",
        "allow_blocking_tools = true\nrequired_tools",
    ));
    assert!(silenced.is_empty(), "{:?}", codes(&silenced));

    // Naming every blocking tool in required_tools leaves nothing to say.
    let all_required = toml.replace(
        r#"required_tools = ["ask_user_text"]"#,
        r#"required_tools = ["ask_user_text", "ask_user_choice", "ask_user_confirm", "present_for_review", "edit_document"]"#,
    );
    let findings = blocking(&all_required);
    assert!(findings.is_empty(), "{:?}", codes(&findings));
}

/// The shell arrives with `@builtin` as surely as by name, and its default
/// policy is still `ask`. Said once, through the group; not at all when the
/// shell is also named, since that finding already covers it.
#[test]
fn a_builtin_group_with_no_shell_policy_is_warned_about() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
tools = ["@all"]
allow_blocking_tools = true
"#,
    );
    let findings = lint(&toml, &grouped_env());
    assert_eq!(codes(&findings), ["implicit-shell-policy"]);
    assert!(
        findings[0].message.contains("'shell' through '@all'"),
        "{findings:?}"
    );
    assert!(
        findings[0]
            .fix
            .as_deref()
            .is_some_and(|f| f.contains("set shell =")),
        "{findings:?}"
    );

    let named = lint(
        &toml.replace(r#"["@all"]"#, r#"["@all", "bash"]"#),
        &grouped_env(),
    );
    let shells = with_code(&named, "implicit-shell-policy");
    assert_eq!(shells.len(), 1, "{named:?}");
    assert!(shells[0].message.contains("'bash'"), "{named:?}");
}

/// An output stage reaching the built-ins through a group can modify the
/// workspace, reported once as the group rather than once per member.
#[test]
fn an_output_stage_granting_the_builtin_group_is_warned_about() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
mode = "output"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
tools = ["@builtin", "submit_output"]
require_output = true
[graph.stages.tool_permissions]
shell = "allow"
"#,
    );
    let findings = lint(&toml, &grouped_env());
    let modify = with_code(&findings, "output-stage-can-modify");
    assert_eq!(modify.len(), 1, "{findings:?}");
    assert!(modify[0].message.contains("'@builtin'"), "{findings:?}");
}

/// Routing tool output into a region with `@builtin` granted is fine: the
/// group carries `context_read` along with `read_file`.
#[test]
fn a_builtin_group_satisfies_the_region_read_check() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
tools = ["@builtin"]
allow_blocking_tools = true
[graph.stages.layout]
total_budget_tokens = 0
[graph.stages.tool_routing]
default_region = "notes"
[graph.stages.tool_permissions]
shell = "allow"
[[graph.stages.layout.regions]]
name = "system"
kind = "pinned"
budget = 1000
[[graph.stages.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 50 }
budget = 10000
[[graph.stages.layout.regions]]
name = "notes"
kind = "temporary"
budget = 1000
"#,
    );
    let findings = lint(&toml, &grouped_env());
    assert!(
        with_code(&findings, "routing-without-region-read").is_empty(),
        "{:?}",
        codes(&findings)
    );
}

/// A required region is enforceable through `@builtin`: the group carries the
/// context-writing tools.
#[test]
fn a_builtin_group_makes_a_required_region_enforceable() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
tools = ["@builtin"]
allow_blocking_tools = true
[graph.stages.layout]
total_budget_tokens = 0
[graph.stages.tool_permissions]
shell = "allow"
[[graph.stages.layout.regions]]
name = "system"
kind = "pinned"
budget = 1000
[[graph.stages.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 50 }
budget = 10000
[[graph.stages.layout.regions]]
name = "findings"
kind = "pinned"
budget = 1000
required = true
"#,
    );
    let findings = lint(&toml, &grouped_env());
    assert!(
        with_code(&findings, "required-region-unenforceable").is_empty(),
        "{:?}",
        codes(&findings)
    );
}

// ─── Blocking tools ───────────────────────────────────────────────────────────

#[test]
fn an_autonomous_stage_granting_an_ask_tool_is_warned_about() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
tools = ["ask_user_text"]
"#,
    );
    let findings = lint(&toml, &LintEnv::default());
    assert_eq!(codes(&findings), ["blocking-tool-in-autonomous-stage"]);
    assert!(
        findings[0].message.contains("until a person answers"),
        "{findings:?}"
    );
}

/// Every name the runtime dispatches to a human is covered, not just the
/// `ask_user_*` family.
#[test]
fn every_blocking_interaction_tool_is_flagged() {
    for tool in BLOCKING_INTERACTION_TOOLS {
        let toml = manifest(&format!(
            r#"

[[graph.stages]]
name = "main"
model = {{ models = [{{ provider = "anthropic", model = "claude-sonnet-5" }}] }}
max_iterations = 10
tools = ["{tool}"]
"#
        ));
        assert_eq!(
            codes(&lint(&toml, &LintEnv::default())),
            ["blocking-tool-in-autonomous-stage"],
            "{tool}"
        );
    }
}

#[test]
fn allow_blocking_tools_silences_the_warning() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
tools = ["ask_user_text"]
allow_blocking_tools = true
"#,
    );
    assert!(lint(&toml, &LintEnv::default()).is_empty());
}

/// Naming the tool in `required_tools` says the same thing one tool at a time,
/// and says it about the runtime too: the stage keeps that tool when the run is
/// unattended. The lint has nothing left to point out.
#[test]
fn required_tools_silences_the_warning_for_that_tool() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
tools = ["ask_user_text", "ask_user_confirm"]
required_tools = ["ask_user_text"]
"#,
    );
    // Only the tool that was *not* kept is still warned about. The kept one is
    // noted instead, because keeping it is what makes it hold under `--yolo`.
    let findings = lint(&toml, &LintEnv::default());
    assert_eq!(
        codes(&findings),
        ["blocking-tool-in-autonomous-stage", "holds-under-yolo"]
    );
    assert!(
        findings[0].message.contains("ask_user_confirm"),
        "{:?}",
        findings[0].message
    );
    assert!(
        findings[1].message.contains("ask_user_text"),
        "{:?}",
        findings[1].message
    );
}

/// An interactive stage is where a person is expected, so the same grant is
/// unremarkable there.
#[test]
fn an_interactive_stage_may_grant_ask_tools_freely() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
mode = "interactive"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
tools = ["ask_user_text"]
"#,
    );
    assert!(lint(&toml, &LintEnv::default()).is_empty());
}

// ─── Shell policy ─────────────────────────────────────────────────────────────

/// The default for a shell grant is `ask`, and an `ask` nobody answers waits
/// rather than denying, so an unattended run hangs on the first command.
#[test]
fn a_shell_grant_with_no_policy_is_warned_about() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
tools = ["bash"]
"#,
    );
    let findings = lint(&toml, &LintEnv::default());
    assert_eq!(codes(&findings), ["implicit-shell-policy"]);
    assert!(findings[0].message.contains("'bash'"), "{findings:?}");
}

/// `bash` and `shell` are the same tool, so both spellings are checked.
#[test]
fn the_canonical_shell_spelling_is_checked_too() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
tools = ["shell"]
"#,
    );
    assert_eq!(
        codes(&lint(&toml, &LintEnv::default())),
        ["implicit-shell-policy"]
    );
}

#[test]
fn a_stage_level_shell_policy_settles_it() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
tools = ["bash"]
[graph.stages.tool_permissions]
bash = "ask"
"#,
    );
    assert!(lint(&toml, &LintEnv::default()).is_empty());
}

/// A permission written under either spelling reaches the tool, so neither is
/// a mismatch to warn about. Policy resolution looks up both, so `bash = "ask"`
/// against a stage granting `shell` counts, and a warning here would send the
/// author off to fix something that works.
#[test]
fn either_spelling_of_a_permission_settles_the_shell() {
    for (granted, written) in [("shell", "bash"), ("bash", "shell")] {
        let toml = format!(
            "{}\n[graph.tool_permissions]\n{written} = \"ask\"\n",
            manifest(&format!(
                r#"

[[graph.stages]]
name = "main"
model = {{ models = [{{ provider = "anthropic", model = "claude-sonnet-5" }}] }}
max_iterations = 10
tools = ["{granted}"]
"#
            ))
        );
        let found = codes(&lint(&toml, &LintEnv::default()));
        assert!(found.is_empty(), "{granted}/{written}: {found:?}");
    }
}

#[test]
fn a_graph_level_shell_policy_settles_it() {
    let toml = format!(
        "{}\n[graph.tool_permissions]\nbash = \"deny\"\n",
        manifest(
            r#"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
tools = ["bash"]
"#,
        )
    );
    assert!(lint(&toml, &LintEnv::default()).is_empty());
}

// ─── Models and providers ─────────────────────────────────────────────────────

#[test]
fn a_model_missing_from_a_known_catalog_is_warned_about() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-9" }] }
max_iterations = 10
"#,
    );
    let env = LintEnv {
        known_models: vec![("anthropic".to_string(), "claude-sonnet-5".to_string())],
        ..LintEnv::default()
    };
    let findings = lint(&toml, &env);
    assert_eq!(codes(&findings), ["unknown-model"]);
    assert!(
        findings[0].message.contains("anthropic/claude-sonnet-9"),
        "{findings:?}"
    );
}

/// Ollama serves whatever has been pulled and OpenRouter's catalog runs to
/// hundreds of entries, so a provider with no rows is not checked at all.
#[test]
fn a_provider_with_no_catalog_is_not_checked() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "ollama", model = "qwen3.5:9b" }] }
max_iterations = 10
"#,
    );
    let env = LintEnv {
        known_models: vec![("anthropic".to_string(), "claude-sonnet-5".to_string())],
        ..LintEnv::default()
    };
    assert!(lint(&toml, &env).is_empty());
}

/// A provider that published its whole catalogue and does not list the model:
/// the one case where naming a model is provably a fault in the blueprint
/// rather than a fact about the machine, so it is an error.
///
/// This is what a Rhai provider with a `list_models` answers, and what nothing
/// checked before: `known_models` covers three built-in providers, so a stage
/// pinned to a script provider's model was never looked at, validated clean,
/// spawned clean and ran on whatever the fallback chain reached.
#[test]
fn a_model_outside_a_complete_catalog_is_an_error() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "groq", model = "llama-3.1-70b" }] }
max_iterations = 10
"#,
    );
    let env = LintEnv {
        provider_catalogs: HashMap::from([(
            "groq".to_string(),
            ProviderCatalog::Complete(vec![
                "llama-4-scout".to_string(),
                "llama-4-maverick".to_string(),
            ]),
        )]),
        ..LintEnv::default()
    };
    let findings = lint(&toml, &env);
    assert_eq!(codes(&findings), ["unserved-model"]);
    assert!(findings[0].is_error(), "{findings:?}");
    // The message carries what the provider does list, because "not that one"
    // without "these instead" sends someone back to the same guess.
    assert!(
        findings[0]
            .message
            .contains("llama-4-scout, llama-4-maverick"),
        "{findings:?}"
    );
}

/// A provider that can say *why* it refuses says that instead.
///
/// "Does not serve it" is right for a typo and wrong for a model the route
/// carries and this account cannot reach - Codex carries
/// `gpt-5.3-codex-spark` and a Plus plan cannot reach it. The two send a
/// reader to different places, one to check the spelling and one to change
/// the stage or the plan, so the reason replaces the guess rather than
/// sitting beside it.
#[test]
fn a_refusal_the_provider_can_explain_says_the_reason() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "groq", model = "llama-3.1-70b" }] }
max_iterations = 10
"#,
    );
    let env = LintEnv {
        provider_catalogs: HashMap::from([(
            "groq".to_string(),
            ProviderCatalog::Complete(vec!["llama-4-scout".to_string()]),
        )]),
        provider_refusals: HashMap::from([(
            "groq/llama-3.1-70b".to_string(),
            "your ChatGPT plus plan does not include it".to_string(),
        )]),
        ..LintEnv::default()
    };

    let findings = lint(&toml, &env);
    assert_eq!(codes(&findings), ["unserved-model"]);
    let message = &findings[0].message;
    assert!(message.contains("plus plan does not include"), "{message}");
    assert!(
        !message.contains("does not serve"),
        "the reason should replace the guess, not sit beside it: {message}"
    );
    // The fix still points at the listing, which is where the alternatives
    // are whichever way the refusal was worded.
    assert!(
        findings[0]
            .fix
            .as_deref()
            .is_some_and(|f| f.contains("lev models list")),
        "{findings:?}"
    );
}

/// A catalogue too long to print is summarised with a count, because "it lists
/// 2" and "it lists 340" send someone to different places: the first to a typo
/// in the script's own `list_models`, the second to a typo in the blueprint.
#[test]
fn a_long_catalog_is_summarised_with_a_count() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "gateway", model = "nope" }] }
max_iterations = 10
"#,
    );
    let env = LintEnv {
        provider_catalogs: HashMap::from([(
            "gateway".to_string(),
            ProviderCatalog::Complete(
                (0..10)
                    .map(|i| format!("model-{i}"))
                    .collect::<Vec<String>>(),
            ),
        )]),
        ..LintEnv::default()
    };
    let findings = lint(&toml, &env);
    assert_eq!(codes(&findings), ["unserved-model"]);
    assert!(
        findings[0]
            .message
            .contains("model-0, model-1, model-2 and 7 more"),
        "{findings:?}"
    );
}

/// A gateway namespaces its ids and a blueprint names the model, so the two are
/// compared by model key. Comparing the raw strings would call every gateway
/// route a model the gateway refuses.
#[test]
fn a_namespaced_catalog_id_answers_for_a_bare_model_name() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "openrouter", model = "gpt-5.5" }] }
max_iterations = 10
"#,
    );
    let env = LintEnv {
        provider_catalogs: HashMap::from([(
            "openrouter".to_string(),
            ProviderCatalog::Complete(vec!["openai/gpt-5.5".to_string()]),
        )]),
        ..LintEnv::default()
    };
    assert!(lint(&toml, &env).is_empty());
}

/// A script provider with neither a `list_models` nor a `serves` list has said
/// nothing, and nothing is not a refusal. A warning, because the alternative -
/// staying silent - is what makes "checked and fine" and "never checked" look
/// identical.
#[test]
fn a_script_provider_that_names_no_models_warns_rather_than_errors() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "quiet", model = "anything-at-all" }] }
max_iterations = 10
"#,
    );
    let env = LintEnv {
        provider_catalogs: HashMap::from([(
            "quiet".to_string(),
            ProviderCatalog::ScriptSaidNothing,
        )]),
        ..LintEnv::default()
    };
    let findings = lint(&toml, &env);
    assert_eq!(codes(&findings), ["catalog-unchecked"]);
    assert!(!findings[0].is_error(), "{findings:?}");
    // The fix names both ways out, since the author picks by what their script
    // can do rather than by preference.
    let fix = findings[0].fix.as_deref().unwrap_or_default();
    assert!(
        fix.contains("list_models") && fix.contains("serves"),
        "{fix}"
    );
}

/// The provider's own catalogue is better evidence than a table compiled into
/// this build, so a live answer settles the entry and the compiled table is not
/// asked again. Two findings on one entry, one calling it wrong and one calling
/// it merely unrecognised, is a report nobody can act on.
#[test]
fn a_live_catalogue_supersedes_the_compiled_table() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-9" }] }
max_iterations = 10
"#,
    );
    let env = LintEnv {
        // The table this build ships has not heard of it...
        known_models: vec![("anthropic".to_string(), "claude-sonnet-5".to_string())],
        // ...but the provider itself says it serves it, which is the newer fact.
        provider_catalogs: HashMap::from([(
            "anthropic".to_string(),
            ProviderCatalog::Complete(vec!["claude-sonnet-9".to_string()]),
        )]),
        ..LintEnv::default()
    };
    assert!(
        lint(&toml, &env).is_empty(),
        "the live catalogue answered, so unknown-model has nothing to add"
    );
}

/// A provider absent from the map was never asked, and an unasked question is
/// not a finding. This is the machine that simply does not have that provider,
/// which `no-reachable-provider` speaks to instead.
#[test]
fn a_provider_nobody_asked_about_is_not_checked() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "groq", model = "llama-3.1-70b" }] }
max_iterations = 10
"#,
    );
    assert!(lint(&toml, &LintEnv::default()).is_empty());
}

#[test]
fn a_model_present_in_the_catalog_passes() {
    let env = LintEnv {
        known_models: vec![("anthropic".to_string(), "claude-sonnet-5".to_string())],
        ..LintEnv::default()
    };
    assert!(lint(&manifest(CLEAN_STAGE), &env).is_empty());
}

/// A stage naming models and no providers is not accused of naming none.
///
/// This check asks whether any listed entry names a provider the install has.
/// An entry in the current form names a model and leaves the route open, so a
/// blueprint written entirely that way had no provider to find, failed the check
/// every time, and was told it would "fall back to your default model" having
/// "tried" a list of empty strings: `(tried , )`.
///
/// Which providers serve a bare model is a question for a registry, and this
/// env has none: `unrouted_models` is empty, which is a question nobody asked
/// rather than an answer of "nothing serves these". So an open entry counts as
/// reachable and the check stays quiet - see
/// [`an_open_entry_nothing_serves_is_warned_about`] for what happens once
/// something has actually asked.
#[test]
fn a_stage_naming_models_without_providers_is_not_warned_about() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
model = { models = [{ model = "claude-sonnet-5" }, { model = "gpt-5.5" }] }
max_iterations = 10
"#,
    );
    let env = LintEnv {
        available_providers: Some(known_tools(&["ollama"])),
        ..LintEnv::default()
    };
    let findings = lint(&toml, &env);
    assert!(
        !codes(&findings).contains(&"no-reachable-provider"),
        "an open-route entry is not a provider named and missing: {findings:?}"
    );
}

/// One open entry among pinned ones leaves the stage undecided, as long as
/// nobody has said whether that entry routes. The open entry might be served
/// here; an empty `unrouted_models` is not the claim that it is not.
#[test]
fn a_stage_pinning_only_unreachable_providers_is_still_warned_about() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
model = { models = [{ model = "claude-sonnet-5" }, { provider = "openai", model = "gpt-5.5" }] }
max_iterations = 10
"#,
    );
    let env = LintEnv {
        available_providers: Some(known_tools(&["ollama"])),
        ..LintEnv::default()
    };
    let findings = lint(&toml, &env);
    assert!(
        !codes(&findings).contains(&"no-reachable-provider"),
        "one open entry is enough to leave this undecided: {findings:?}"
    );
}

/// The case an empty `unrouted_models` could never reach: something did ask
/// the registry, and the answer was that nothing here serves the one model the
/// stage names.
///
/// This is the shape a typo makes, and it takes the registry's answer to see:
/// the entry pins no provider, so nothing but `unrouted_models` can say the
/// model is unservable.
#[test]
fn an_open_entry_nothing_serves_is_warned_about() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
model = { models = [{ model = "gorq-turbo-9" }] }
max_iterations = 10
"#,
    );
    let env = LintEnv {
        available_providers: Some(known_tools(&["ollama"])),
        unrouted_models: known_tools(&["gorq-turbo-9"]),
        ..LintEnv::default()
    };
    let findings = lint(&toml, &env);
    assert_eq!(codes(&findings), ["no-reachable-provider"]);
    // Rendered bare, the way the blueprint wrote it. `/gorq-turbo-9` would show
    // a route the entry does not claim to have.
    assert!(
        findings[0].message.contains("tried gorq-turbo-9"),
        "{findings:?}"
    );
}

/// One entry that routes is enough, however the others are written. The list is
/// an ordered set of fallbacks, and a machine declining some of the options is
/// the normal case rather than a fault.
#[test]
fn one_routable_open_entry_keeps_the_stage_quiet() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
model = { models = [{ model = "gorq-turbo-9" }, { model = "qwen3.5:9b" }, { provider = "openai", model = "gpt-5.5" }] }
max_iterations = 10
"#,
    );
    let env = LintEnv {
        available_providers: Some(known_tools(&["ollama"])),
        unrouted_models: known_tools(&["gorq-turbo-9"]),
        ..LintEnv::default()
    };
    let findings = lint(&toml, &env);
    assert!(
        !codes(&findings).contains(&"no-reachable-provider"),
        "qwen3.5:9b routes, so the stage has somewhere to run: {findings:?}"
    );
}

/// A stage with nothing reachable in its whole list is the shape the runtime
/// rejects at spawn, so it is worth saying up front.
#[test]
fn a_stage_with_no_reachable_provider_is_warned_about() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }, { provider = "openai", model = "gpt-5.5" }] }
max_iterations = 10
"#,
    );
    let env = LintEnv {
        available_providers: Some(known_tools(&["ollama"])),
        ..LintEnv::default()
    };
    let findings = lint(&toml, &env);
    assert_eq!(codes(&findings), ["no-reachable-provider"]);
    assert!(
        findings[0]
            .message
            .contains("anthropic/claude-sonnet-5, openai/gpt-5.5"),
        "{findings:?}"
    );
}

/// The models list is an ordered set of fallbacks. A provider the install
/// cannot reach is unremarkable as long as something later in the list can, so
/// only a list that is reachable nowhere is reported.
#[test]
fn an_unreachable_provider_is_fine_when_a_later_one_answers() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }, { provider = "ollama", model = "qwen3.5:9b" }] }
max_iterations = 10
"#,
    );
    let env = LintEnv {
        available_providers: Some(known_tools(&["ollama"])),
        ..LintEnv::default()
    };
    assert!(lint(&toml, &env).is_empty());
}

#[test]
fn every_provider_reachable_reports_nothing() {
    let env = LintEnv {
        available_providers: Some(known_tools(&["anthropic"])),
        ..LintEnv::default()
    };
    assert!(lint(&manifest(CLEAN_STAGE), &env).is_empty());
}

/// A stage with no model entries at all has no list to check, so the
/// reachability question does not arise (the missing-model warning covers it).
#[test]
fn a_stage_with_an_empty_models_list_is_not_checked_for_reachability() {
    let mut bp = parse(&manifest(CLEAN_STAGE));
    bp.graph.stages[0].model.models.clear();
    let env = LintEnv {
        available_providers: Some(HashSet::new()),
        ..LintEnv::default()
    };
    let findings = lint_blueprint(&bp, &env);
    assert_eq!(codes(&findings), ["stage-missing-model"]);
}

// ─── held checkpoints ─────────────────────────────────────────────────────────

/// A checkpoint that holds under `--yolo` is the blueprint working as written,
/// so it is a note. It is worth saying because `--yolo` reads as "run without
/// me", and a run that stops anyway looks like a hang.
#[test]
fn a_checkpoint_that_holds_under_yolo_is_noted() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "plan"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
tools = ["read_file", "ask_user_text"]
required_tools = ["ask_user_text"]
[[graph.stages.mode.interactive_points]]
name = "plan_approval"
prompt = "Review the plan"
style = "confirm"
unattended = "ask"
"#,
    );
    let all = lint(&toml, &LintEnv::default());
    let findings = with_code(&all, "holds-under-yolo");
    assert_eq!(findings.len(), 2, "the point and the kept tool");
    assert!(findings.iter().all(|f| f.severity == LintSeverity::Note));
    assert!(
        findings.iter().any(|f| f.message.contains("plan_approval")),
        "{findings:?}"
    );
    assert!(
        findings.iter().any(|f| f.message.contains("ask_user_text")),
        "{findings:?}"
    );
    assert!(findings.iter().all(|f| f.stage.as_deref() == Some("plan")));
}

/// A blueprint with nothing held says nothing, so the note does not become
/// background noise on every validate.
#[test]
fn a_blueprint_that_holds_nothing_is_not_noted() {
    let found = codes(&lint(&manifest(CLEAN_STAGE), &LintEnv::default()));
    assert!(!found.contains(&"holds-under-yolo"), "{found:?}");
}

/// A stage granting `bash` and keeping `shell` is one decision, not two - the
/// runtime canonicalises both sides, so the lint has to as well.
#[test]
fn the_blocking_tool_check_canonicalises_required_tools() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
tools = ["ask_user_text", "bash"]
required_tools = ["ask_user_text", "bash"]
"#,
    );
    let found = codes(&lint(&toml, &LintEnv::default()));
    assert!(
        !found.contains(&"blocking-tool-in-autonomous-stage"),
        "{found:?}"
    );
}

// ─── safe_commands ────────────────────────────────────────────────────────────

fn with_safe_commands(body: &str) -> String {
    format!("{}\n[graph.safe_commands]\n{body}\n", manifest(CLEAN_STAGE))
}

/// Declaring `safe_commands` is legitimate, so it is a note - but an author
/// who does not know that declaring is not granting ships a block that does
/// nothing on every install but their own.
#[test]
fn a_safe_commands_block_is_noted_as_needing_a_grant() {
    let findings = lint(
        &with_safe_commands("shell = [\"cargo test\"]"),
        &LintEnv::default(),
    );
    assert_eq!(codes(&findings), ["safe-commands-declared"]);
    assert_eq!(findings[0].severity, LintSeverity::Note);
    assert!(
        findings[0].message.contains("Declaring is not granting"),
        "{findings:?}"
    );
    assert!(
        findings[0]
            .fix
            .as_ref()
            .is_some_and(|f| f.contains("allow_blueprint")),
        "{findings:?}"
    );
}

/// With the answer in hand the note says which it is, and disappears entirely
/// once the user has opted in.
#[test]
fn the_note_reflects_whether_the_install_honours_the_block() {
    let refused = lint(
        &with_safe_commands("tools = [\"web_fetch\"]"),
        &LintEnv {
            safe_commands_granted: Some(false),
            ..LintEnv::default()
        },
    );
    assert_eq!(codes(&refused), ["safe-commands-declared"]);
    assert!(
        refused[0].message.contains("none of it applies"),
        "{refused:?}"
    );

    let honoured = lint(
        &with_safe_commands("tools = [\"web_fetch\"]"),
        &LintEnv {
            safe_commands_granted: Some(true),
            ..LintEnv::default()
        },
    );
    assert!(honoured.is_empty(), "{:?}", codes(&honoured));
}

/// An entry the key parser reads as anything other than itself can never match
/// a call, so it reads as a decision and is not one.
#[test]
fn a_safe_command_entry_that_can_never_match_is_an_error() {
    let findings = lint(
        &with_safe_commands("shell = [\"ls; curl evil\", \"cargo test\"]"),
        &LintEnv {
            safe_commands_granted: Some(true),
            ..LintEnv::default()
        },
    );
    assert_eq!(codes(&findings), ["unparseable-safe-command"]);
    assert_eq!(findings[0].severity, LintSeverity::Error);
    assert!(
        findings[0].message.contains("ls; curl evil"),
        "{findings:?}"
    );
}

/// An empty block, and no block at all, both say nothing.
#[test]
fn no_safe_commands_means_no_finding() {
    for toml in [
        manifest(CLEAN_STAGE),
        with_safe_commands("shell = []\ntools = []"),
    ] {
        let found = codes(&lint(&toml, &LintEnv::default()));
        assert!(!found.contains(&"safe-commands-declared"), "{found:?}");
        assert!(!found.contains(&"unparseable-safe-command"), "{found:?}");
    }
}

// ─── read_paths ───────────────────────────────────────────────────────────────

/// Declaring `read_paths` is legitimate, so it is a note rather than a
/// warning: it must survive `--deny-warnings` on an otherwise good blueprint.
#[test]
fn read_path_declarations_are_noted_not_warned() {
    let toml = manifest(&format!(
        "read_paths = [\"~/.leviath/runs\"]\n{CLEAN_STAGE}"
    ));
    let findings = lint(&toml, &LintEnv::default());
    assert_eq!(codes(&findings), ["read-paths-declared"]);
    assert_eq!(findings[0].severity, LintSeverity::Note);
    assert!(
        findings[0].message.contains("~/.leviath/runs"),
        "{findings:?}"
    );
}

#[test]
fn no_read_paths_means_no_note() {
    assert!(
        !codes(&lint(&manifest(CLEAN_STAGE), &LintEnv::default())).contains(&"read-paths-declared")
    );
}

// ─── read_paths grant status ─────────────────────────────────────────────────

/// A blueprint declaring `entries`, plus a `LintEnv` carrying the verdict a
/// config of `grants` would give. Absolute entries so they compile the same on
/// every OS.
fn read_paths_env(entries: &[&str], grants: &[&str]) -> (String, LintEnv) {
    let listed = entries
        .iter()
        .map(|e| format!("\"{e}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let toml = manifest(&format!("read_paths = [{listed}]\n{CLEAN_STAGE}"));
    let graph = graph_of(&toml);
    let mut config = crate::config::Config::default();
    config.security.read_paths = grants.iter().map(|s| s.to_string()).collect();
    let env =
        LintEnv::default().with_read_paths(&graph, "lint-fixture", &config, Path::new("/work"));
    (toml, env)
}

/// The reported bug: with nothing granting them, the declared entries are
/// named as inert and the stanza that would fix it is on the fix line.
#[test]
fn ungranted_read_paths_are_warned_about_with_the_stanza_to_add() {
    let (toml, env) = read_paths_env(&["/data/runs", "glob:/docs/**"], &[]);
    let findings = lint(&toml, &env);
    assert_eq!(
        codes(&findings),
        ["read-paths-not-granted", "read-paths-declared"]
    );
    assert!(findings[0].message.contains("/data/runs"), "{findings:?}");
    assert!(
        findings[0].message.contains("glob:/docs/**"),
        "{findings:?}"
    );
    let fix = findings[0].fix.as_deref().expect("a fix names the stanza");
    assert!(fix.contains("[agent_read_paths.lint-fixture]"), "{fix}");
}

/// A partial grant is reported per entry: the granted paths drop out of the
/// message and the refused ones stay.
#[test]
fn a_partial_grant_names_only_what_is_still_refused() {
    let (toml, env) = read_paths_env(&["/data/runs", "glob:/docs/**"], &["/data/runs"]);
    let findings = lint(&toml, &env);
    assert_eq!(
        codes(&findings),
        ["read-paths-not-granted", "read-paths-declared"]
    );
    assert!(!findings[0].message.contains("/data/runs,"), "{findings:?}");
    assert!(
        findings[0].message.contains("glob:/docs/**"),
        "{findings:?}"
    );
    assert!(
        findings[1].message.contains("2 declared, 1 granted"),
        "{findings:?}"
    );
}

/// Fully granted: the note says so, per entry, and nothing warns.
#[test]
fn granted_read_paths_are_a_note_only() {
    let (toml, env) = read_paths_env(&["/data/runs"], &["/data/runs"]);
    let findings = lint(&toml, &env);
    assert_eq!(codes(&findings), ["read-paths-declared"]);
    assert!(
        findings[0].message.contains("1 declared, 1 granted"),
        "{findings:?}"
    );
    assert!(
        findings[0]
            .fix
            .as_deref()
            .is_some_and(|f| f.contains("/data/runs: granted")),
        "{findings:?}"
    );
}

/// The blanket override grants everything, and says which switch did it.
#[test]
fn the_blanket_override_is_named_on_the_note() {
    let toml = manifest(&format!("read_paths = [\"/data/runs\"]\n{CLEAN_STAGE}"));
    let graph = graph_of(&toml);
    let mut config = crate::config::Config::default();
    config.security.allow_blueprint_read_paths = true;
    let env =
        LintEnv::default().with_read_paths(&graph, "lint-fixture", &config, Path::new("/work"));
    let findings = lint(&toml, &env);
    assert_eq!(codes(&findings), ["read-paths-declared"]);
    assert!(
        findings[0]
            .fix
            .as_deref()
            .is_some_and(|f| f.contains("allow_blueprint_read_paths")),
        "{findings:?}"
    );
}

/// An entry no representative path can be built from is reported as unchecked
/// rather than as inert, and is not offered up for granting.
#[test]
fn an_uncheckable_entry_is_not_called_ungranted() {
    let (toml, env) = read_paths_env(&["glob:/docs/[ab]/**"], &["/data/runs"]);
    let findings = lint(&toml, &env);
    assert_eq!(codes(&findings), ["read-paths-declared"]);
    assert!(
        findings[0]
            .fix
            .as_deref()
            .is_some_and(|f| f.contains("cannot be checked")),
        "{findings:?}"
    );
}

/// A grant list of the user's own that will not compile is a hard spawn error;
/// `lev validate` is where it should surface first.
#[test]
fn a_malformed_config_grant_is_warned_about() {
    let (toml, env) = read_paths_env(&["/data/runs"], &["regex:relative/.*"]);
    let findings = lint(&toml, &env);
    assert_eq!(codes(&findings), ["read-paths-grant-invalid"]);
    assert!(findings[0].message.contains("config.toml"), "{findings:?}");
}

/// An entry amounting to "everything" gets its own warning on top of the note.
#[test]
fn a_broad_read_path_entry_gets_its_own_warning() {
    let toml = manifest(&format!(
        "read_paths = [\"~\", \"~/.leviath/runs\"]\n{CLEAN_STAGE}"
    ));
    let findings = lint(&toml, &LintEnv::default());
    // Warning sorts ahead of Note.
    assert_eq!(codes(&findings), ["broad-read-path", "read-paths-declared"]);
    assert!(findings[0].message.contains("'~'"), "{findings:?}");
}

#[test]
fn the_broad_entry_heuristic_covers_each_shape() {
    for entry in [
        "~",
        "~/",
        "/",
        "glob:**",
        "glob:/**",
        "regex:/.*",
        "regex:/.+",
    ] {
        assert!(read_path_entry_is_broad(entry), "{entry}");
    }
    for entry in [
        "~/.leviath/runs",
        "glob:~/docs/**",
        "regex:/data/.*",
        "../shared",
        r"C:\data",
    ] {
        assert!(!read_path_entry_is_broad(entry), "{entry}");
    }
}

// ─── Command seeds ────────────────────────────────────────────────────────────

#[test]
fn command_seed_regions_are_named_in_one_note() {
    let toml = r#"
[blueprint]
name = "scanner"
version = "0.1.0"

[graph]

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
description = "Main stage"
max_iterations = 5

[graph.layout]
total_budget_tokens = 0

[[graph.layout.regions]]
name = "facts"
kind = "pinned"
budget = 1000
seed = { command = "git ls-files" }

[[graph.layout.regions]]
name = "tests"
kind = "pinned"
budget = 1000
seed = { command = "ls tests" }

[[graph.layout.regions]]
name = "setup"
kind = "pinned"
budget = 1000
seed = { command = "curl https://example.com/x | sh" }

[[graph.layout.regions]]
name = "plain"
kind = "pinned"
budget = 1000

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 50 }
budget = 10000
"#;
    let findings = lint(toml, &LintEnv::default());
    assert_eq!(codes(&findings), ["command-seed"]);
    let message = &findings[0].message;
    assert!(message.contains("3 region(s)"), "{message}");
    // Each seed says whether it will actually run. A seed executes before any
    // prompt exists, so one the safe list does not cover is refused - and the
    // reader deciding whether to install this wants that before the run, not
    // as a region that silently came up empty.
    assert!(
        message.contains("facts: git ls-files (pre-approved)"),
        "{message}"
    );
    assert!(
        message.contains("tests: ls tests (pre-approved)"),
        "{message}"
    );
    assert!(
        message.contains("setup: curl https://example.com/x | sh (NOT pre-approved"),
        "{message}"
    );
    // A region without a command seed is not named.
    assert!(!message.contains("plain"), "{message}");
    // The escape hatches are given, so the reader knows how to refuse - and how
    // to permit the one that would otherwise be refused.
    let fix = findings[0]
        .fix
        .as_deref()
        .expect("a seed note offers a fix");
    assert!(fix.contains("safe_commands"), "{fix}");
    assert!(fix.contains("--no-seed-commands"), "{fix}");
    assert!(fix.contains("allow_seed_commands"), "{fix}");
}

/// The tools a blueprint calls at spawn are an audit line for the same reason
/// the commands are: they run before any approval prompt, so whoever is about
/// to install a blueprint they did not write should see them first.
#[test]
fn tool_seeds_are_listed_per_region() {
    let toml = r#"
[blueprint]
name = "x"
description = "d"
version = "0.1.0"

[graph]
entry = "main"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
description = "Main stage"
max_iterations = 5

[graph.layout]
total_budget_tokens = 0

[[graph.layout.regions]]
name = "environment"
kind = "pinned"
budget = 1000
seed = { tools = { calls = [{ tool = "current_time", args = {} }, { tool = "system_info", args = {} }] } }

[[graph.layout.regions]]
name = "toolchain"
kind = "pinned"
budget = 1000
seed = { tools = { calls = [{ tool = "which_command", args = {} }], refresh = "each_stage" } }

[[graph.layout.regions]]
name = "plain"
kind = "pinned"
budget = 1000

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 50 }
budget = 10000
"#;
    let findings = lint(toml, &LintEnv::default());
    assert_eq!(codes(&findings), ["tool-seed"]);
    let message = &findings[0].message;
    assert!(message.contains("2 region(s)"), "{message}");
    // Named per region, and every tool in it, so the reader sees what runs and
    // where its output lands.
    assert!(
        message.contains("environment: current_time, system_info"),
        "{message}"
    );
    // A refreshing seed says so: it is a tool call per stage for the life of
    // the run, not one at spawn, and that is the part a reader should weigh.
    assert!(
        message.contains("toolchain: which_command (on every stage entry)"),
        "{message}"
    );
    // While a seed that runs once carries no such note - the suffix appears
    // exactly once in the message, on the entry that earned it.
    assert_eq!(
        message.matches("(on every stage entry)").count(),
        1,
        "{message}"
    );
    // A region with no tool seed is not named.
    assert!(!message.contains("plain"), "{message}");
    // Unlike a command seed there is no separate switch to name; the answer to
    // "will this run" is the permission table, so the fix says so.
    let fix = findings[0]
        .fix
        .as_deref()
        .expect("a seed note offers a fix");
    assert!(fix.contains("tool_permissions"), "{fix}");
    assert!(fix.contains("ask"), "{fix}");
}

#[test]
fn no_tool_seeds_means_no_note() {
    assert!(!codes(&lint(&manifest(CLEAN_STAGE), &LintEnv::default())).contains(&"tool-seed"));
}

#[test]
fn no_command_seeds_means_no_note() {
    assert!(!codes(&lint(&manifest(CLEAN_STAGE), &LintEnv::default())).contains(&"command-seed"));
}

// ─── Graph shape ──────────────────────────────────────────────────────────────

/// A graph-shaped blueprint with the entry reaching everything reports nothing,
/// including through a diamond where a shared target is queued twice.
#[test]
fn a_fully_reachable_graph_reports_nothing() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "entry"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5

[[graph.stages]]
name = "b"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5

[[graph.stages]]
name = "c"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5

[[graph.stages]]
name = "d"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5

[[graph.edges]]
name = "b"
from = "entry"
to = "b"

[[graph.edges]]
name = "c"
from = "entry"
to = "c"

[[graph.edges]]
name = "d"
from = "b"
to = "d"

[[graph.edges]]
name = "d"
from = "c"
to = "d"
"#,
    );
    let findings = lint(&toml, &LintEnv::default());
    assert!(findings.is_empty(), "{:?}", codes(&findings));
}

/// A fan_out stage reaches its worker and merge stages through its own
/// settings rather than an edge. Following only edges would report both as
/// orphans in a correctly wired blueprint.
#[test]
fn fan_out_worker_and_merge_stages_are_reachable() {
    let toml = manifest_taking_task(
        r#"

[[graph.stages]]
name = "split"
mode = { fan_out = { worker = { stage = "work" }, merge_stage = "merge" } }
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }

[[graph.stages]]
name = "work"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
allow_as_worker = true

[[graph.stages]]
name = "merge"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5

[[graph.edges]]
name = "merge"
from = "split"
to = "merge"
when = "error"
"#,
    );
    let findings = lint(&toml, &LintEnv::default());
    assert!(findings.is_empty(), "{:?}", codes(&findings));
}

#[test]
fn a_stage_the_entry_cannot_reach_is_warned_about() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "a"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5

[[graph.stages]]
name = "b"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5

[[graph.stages]]
name = "orphan"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5

[[graph.edges]]
name = "b"
from = "a"
to = "b"
"#,
    );
    let findings = lint(&toml, &LintEnv::default());
    assert_eq!(codes(&findings), ["unreachable-stage"]);
    assert_eq!(findings[0].stage.as_deref(), Some("orphan"));
}

#[test]
fn a_cycle_with_no_revisit_cap_is_warned_about() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "a"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5

[[graph.stages]]
name = "b"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5

[[graph.edges]]
name = "b"
from = "a"
to = "b"

[[graph.edges]]
name = "a"
from = "b"
to = "a"
"#,
    );
    // Each stage is the "target" of the other's edge, and neither caps
    // revisits, so both ends of the loop are named.
    let findings = lint(&toml, &LintEnv::default());
    assert_eq!(
        codes(&findings),
        ["cycle-without-max-revisits", "cycle-without-max-revisits"]
    );
}

#[test]
fn a_capped_cycle_no_longer_trips_the_cycle_lint_but_can_dead_end() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "a"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
max_revisits = 2

[[graph.stages]]
name = "b"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
max_revisits = 3

[[graph.edges]]
name = "b"
from = "a"
to = "b"

[[graph.edges]]
name = "a"
from = "b"
to = "a"
"#,
    );
    // Capping both ends satisfies the cycle lint - but now EVERY exit of each
    // stage is exhaustible, so once both budgets are spent the run dead-ends
    // (an error at runtime). The dead-end lint says so for both stages.
    assert_eq!(
        codes(&lint(&toml, &LintEnv::default())),
        ["dead-end-possible", "dead-end-possible"]
    );
}

/// A self-loop is not a two-stage cycle, and a terminal stage has no edges
/// out. Both are shapes the walk has to step over without complaining.
#[test]
fn self_loops_and_terminal_stages_are_not_cycles() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "a"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5

[[graph.stages]]
name = "b"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5

[[graph.edges]]
name = "a"
from = "a"
to = "a"

[[graph.edges]]
name = "b"
from = "a"
to = "b"
"#,
    );
    assert!(lint(&toml, &LintEnv::default()).is_empty());
}

/// A graph with no edges anywhere is linear: there is nothing to walk, so the
/// graph checks return before doing anything.
#[test]
fn a_linear_blueprint_has_no_graph_findings() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "a"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5

[[graph.stages]]
name = "b"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
"#,
    );
    assert!(lint(&toml, &LintEnv::default()).is_empty());
}

/// Graph validation refuses an entry or an edge target that names no stage,
/// so the walk only meets those in a graph nothing has validated yet. It has
/// to step over them rather than panic.
#[test]
fn the_graph_walk_steps_over_names_that_are_not_stages() {
    let stage = "[[graph.stages]]\nname = \"a\"\n\
                 model = { models = [{ provider = \"anthropic\", model = \"claude-sonnet-5\" }] }\n\
                 max_iterations = 5\n";

    // The entry names no stage: the walk pops "ghost" and finds nothing to
    // walk from, so "a" is unreachable, which is the only thing worth saying.
    let ghost_entry = manifest(&format!(
        "entry = \"ghost\"\n{stage}\n[[graph.edges]]\nname = \"again\"\nfrom = \"a\"\nto = \"a\"\n"
    ));
    assert_eq!(
        codes(&lint(&ghost_entry, &LintEnv::default())),
        ["unreachable-stage"]
    );

    // An edge into no stage: the cycle check finds nothing to ask about the
    // other end, but an edge nothing can ever follow is a guaranteed strand,
    // which the dead-end lint reports.
    let dangling = manifest(&format!(
        "{stage}\n[[graph.edges]]\nname = \"ghost\"\nfrom = \"a\"\nto = \"ghost\"\n"
    ));
    assert_eq!(
        codes(&lint(&dangling, &LintEnv::default())),
        ["dead-end-possible"]
    );
}

// ─── Finding rendering ────────────────────────────────────────────────────────

#[test]
fn severity_labels_are_the_same_width() {
    assert_eq!(
        LintSeverity::Error.label().len(),
        LintSeverity::Warning.label().len()
    );
    assert_eq!(LintSeverity::Error.label().trim(), "ERR");
    assert_eq!(LintSeverity::Warning.label().trim(), "WARN");
}

#[test]
fn one_line_names_the_stage_when_there_is_one() {
    let stageless = LintFinding::new(LintSeverity::Warning, "c", "something".to_string());
    assert_eq!(stageless.one_line(), "something");
    assert_eq!(
        stageless.in_stage("main").one_line(),
        "stage 'main': something"
    );
}

#[test]
fn is_error_distinguishes_the_severities() {
    assert!(LintFinding::new(LintSeverity::Error, "c", String::new()).is_error());
    assert!(!LintFinding::new(LintSeverity::Warning, "c", String::new()).is_error());
}

// ─── Several defects at once ──────────────────────────────────────────────────

/// The reported blueprint, reconstructed: no model, no iteration cap, an unattended stage that can ask a human, a shell grant with no policy,
/// and a typo. Every finding lands, and only the typo is fatal.
#[test]
fn a_thoroughly_broken_stage_reports_each_defect_once() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "scope"
tools = ["read_file", "bash", "ask_user_text", "raed_file"]
"#,
    );
    let env = LintEnv {
        known_tools: known_tools(&["read_file", "bash", "shell", "ask_user_text"]),
        ..LintEnv::default()
    };
    let findings = lint(&toml, &env);
    for code in [
        "stage-missing-model",
        "stage-missing-max-iterations",
        "unknown-tool",
        "blocking-tool-in-autonomous-stage",
        "implicit-shell-policy",
    ] {
        assert_eq!(with_code(&findings, code).len(), 1, "{code}: {findings:?}");
    }
    assert_eq!(findings.iter().filter(|f| f.is_error()).count(), 1);
    assert_eq!(findings.len(), 5, "{:?}", codes(&findings));
}

// ─── Final-output stages ──────────────────────────────────────────────────────

/// The env every output fixture needs: `submit_output` is a real built-in, so
/// the unknown-tool check must not also fire and drown the finding under test.
fn output_env() -> LintEnv {
    LintEnv {
        known_tools: known_tools(&["read_file", "write_file", "edit_file", "submit_output"]),
        ..LintEnv::default()
    }
}

const REACHABLE_OUTPUT: &str = r#"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
description = "Main"
max_iterations = 10
tools = ["read_file"]

[[graph.stages]]
name = "summary"
mode = "output"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
description = "Report"
max_iterations = 4

[[graph.edges]]
name = "summary"
from = "main"
to = "summary"
hint = "done"
"#;

#[test]
fn a_reachable_output_stage_reports_nothing() {
    let findings = lint(&manifest(REACHABLE_OUTPUT), &output_env());
    assert!(findings.is_empty(), "{:?}", codes(&findings));
}

/// An output stage nothing routes to means the run can never produce one - and
/// the blueprint still validates, because the graph is otherwise well-formed.
#[test]
fn an_output_stage_no_edge_reaches_is_an_error() {
    let toml = REACHABLE_OUTPUT.replace(
        "[[graph.edges]]\nname = \"summary\"\nfrom = \"main\"\nto = \"summary\"\nhint = \"done\"\n",
        "",
    );
    let findings = lint(&manifest(&toml), &output_env());
    let unreachable = with_code(&findings, "output-unreachable");
    assert_eq!(unreachable.len(), 1, "{:?}", codes(&findings));
    assert_eq!(unreachable[0].severity, LintSeverity::Error);
}

/// The quiet one. `allow_complete` offers the model a DONE it may pick instead
/// of routing onward - and it is appended even to a custom transition_prompt,
/// so a stage can offer an exit its own prompt never mentions.
#[test]
fn an_upstream_allow_complete_that_could_skip_the_output_stage_is_flagged() {
    let toml = REACHABLE_OUTPUT.replace(
        "tools = [\"read_file\"]\n",
        "tools = [\"read_file\"]\nallow_complete = true\n",
    );
    let findings = lint(&manifest(&toml), &output_env());
    let skipped = with_code(&findings, "allow-complete-skips-output");
    assert_eq!(skipped.len(), 1, "{:?}", codes(&findings));
    assert_eq!(skipped[0].stage.as_deref(), Some("main"));
}

/// The output stage's own `allow_complete` (set for it by the mode) is the
/// point, not a defect.
#[test]
fn the_output_stages_own_allow_complete_is_not_flagged() {
    let findings = lint(&manifest(REACHABLE_OUTPUT), &output_env());
    assert!(with_code(&findings, "allow-complete-skips-output").is_empty());
}

/// A blueprint with no output stage at all is not nagged: plenty of agents
/// legitimately produce files and nothing else.
#[test]
fn a_blueprint_with_no_output_stage_is_left_alone() {
    let toml = CLEAN_STAGE.replace(
        "tools = [\"read_file\"]\n",
        "tools = [\"read_file\"]\nallow_complete = true\n",
    );
    let findings = lint(&manifest(&toml), &LintEnv::default());
    assert!(with_code(&findings, "allow-complete-skips-output").is_empty());
    assert!(with_code(&findings, "output-unreachable").is_empty());
}

/// An output stage that can also write files invites the model to keep working
/// where it was meant to report.
#[test]
fn an_output_stage_that_can_modify_files_is_flagged() {
    let toml = REACHABLE_OUTPUT.replace(
        "description = \"Report\"",
        "description = \"Report\"\ntools = [\"write_file\"]",
    );
    let findings = lint(&manifest(&toml), &output_env());
    let modifies = with_code(&findings, "output-stage-can-modify");
    assert_eq!(modifies.len(), 1, "{:?}", codes(&findings));
    assert_eq!(modifies[0].stage.as_deref(), Some("summary"));
}

/// A declared shape nobody is obliged to produce is a wish, not a contract.
#[test]
fn a_declared_shape_without_require_output_is_flagged() {
    let toml = CLEAN_STAGE.to_string() + "\n[graph.stages.output]\nformat = \"a2ui\"\n";
    let findings = lint(&manifest(&toml), &output_env());
    let unrequired = with_code(&findings, "output-shape-not-required");
    assert_eq!(unrequired.len(), 1, "{:?}", codes(&findings));
}

/// The same shape on a stage that must submit is exactly right.
#[test]
fn a_declared_shape_on_a_requiring_stage_reports_nothing() {
    let toml = REACHABLE_OUTPUT.to_string()
        + "\n[graph.stages.output]\nformat = \"a2ui\"\ninstructions = \"one card per finding\"\n";
    let findings = lint(&manifest(&toml), &output_env());
    assert!(findings.is_empty(), "{:?}", codes(&findings));
}

/// `require_output` by hand, without the tool: the stage is told to hand back
/// an answer it has no way to submit.
#[test]
fn requiring_an_output_without_the_submit_tool_is_an_error() {
    let toml = manifest(
        "[[graph.stages]]\nname = \"summary\"\n\
         model = { models = [{ provider = \"anthropic\", model = \"m\" }] }\n\
         max_iterations = 4\ntools = [\"read_file\"]\nrequire_output = true\n",
    );
    let findings = lint_output_stage(&graph_of(&toml).stages[0]);
    let missing = with_code(&findings, "output-missing-submit-tool");
    assert_eq!(missing.len(), 1, "{:?}", codes(&findings));
    assert_eq!(missing[0].severity, LintSeverity::Error);
}

// ─── compact-summarizes-deliverable ──────────────────────────────────────────

/// The shape that warns: a `required` region and a bare `compact` edge, which
/// together mean the stage's own deliverable is paraphrased on the way out.
#[test]
fn a_bare_compact_over_a_required_region_is_warned_about() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "verify"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5

[[graph.stages]]
name = "answer"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5

[[graph.edges]]
name = "answer"
from = "verify"
to = "answer"
carry = { compact = {} }
"#,
    ) + "\n[[graph.layout.regions]]\nname = \"results\"\n\
           kind = { kind = \"sliding_window\", max_items = 20 }\nbudget = 8000\nrequired = true\n";
    let findings = lint(&toml, &LintEnv::default());
    let found = with_code(&findings, "compact-summarizes-deliverable");
    assert_eq!(found.len(), 1, "{:?}", codes(&findings));
    assert!(found[0].message.contains("results"), "{}", found[0].message);
}

/// Silenced by the flag that fixes it, or the warning would be advice nobody
/// can act on.
#[test]
fn a_region_declared_not_summarizable_is_not_warned_about() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "verify"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5

[[graph.stages]]
name = "answer"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5

[[graph.edges]]
name = "answer"
from = "verify"
to = "answer"
carry = { compact = {} }
"#,
    ) + "\n[[graph.layout.regions]]\nname = \"results\"\n\
           kind = { kind = \"sliding_window\", max_items = 20 }\nbudget = 8000\nrequired = true\nsummarizable = false\n";
    let findings = lint(&toml, &LintEnv::default());
    assert!(
        with_code(&findings, "compact-summarizes-deliverable").is_empty(),
        "{:?}",
        codes(&findings)
    );
}

/// A pinned region is never handed to the summarizer in the first place, so
/// warning about one would be noise.
#[test]
fn a_pinned_required_region_is_not_warned_about() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "verify"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5

[[graph.stages]]
name = "answer"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5

[[graph.edges]]
name = "answer"
from = "verify"
to = "answer"
carry = { compact = {} }
"#,
    )
    .replacen("budget = 1000\n", "budget = 1000\nrequired = true\n", 1);
    let findings = lint(&toml, &LintEnv::default());
    assert!(
        with_code(&findings, "compact-summarizes-deliverable").is_empty(),
        "{:?}",
        codes(&findings)
    );
}

/// No compact edge, nothing to say - so the check is about the pairing rather
/// than about declaring a required region at all.
#[test]
fn a_required_region_with_no_compact_edge_is_not_warned_about() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "verify"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5

[[graph.stages]]
name = "answer"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5

[[graph.edges]]
name = "answer"
from = "verify"
to = "answer"
"#,
    ) + "\n[[graph.layout.regions]]\nname = \"results\"\n\
           kind = { kind = \"sliding_window\", max_items = 20 }\nbudget = 8000\nrequired = true\n";
    let findings = lint(&toml, &LintEnv::default());
    assert!(
        with_code(&findings, "compact-summarizes-deliverable").is_empty(),
        "{:?}",
        codes(&findings)
    );
}

/// A blueprint whose region layout is spelled out, so a budget can be varied.
fn manifest_with_regions(regions: &str) -> String {
    format!(
        r#"
[blueprint]
name = "lint-fixture"
version = "0.1.0"
description = "a fixture"

[graph]

[[graph.stages]]
name = "work"
model = {{ models = [{{ provider = "anthropic", model = "claude-sonnet-5" }}] }}
max_iterations = 10
allow_complete = true

[graph.layout]
total_budget_tokens = 0

[[graph.layout.regions]]
name = "system"
kind = "pinned"
budget = 1000

[[graph.layout.regions]]
name = "conversation"
kind = {{ kind = "sliding_window", max_items = 50 }}
budget = 10000

[[graph.layout.regions]]
{regions}
"#
    )
}

/// The reported case, reproduced with the bundled researcher's own numbers.
///
/// `raw_findings = {{ kind = "temporary", budget = "38%" }}` means "hold the last
/// ~76k of raw source material" against a 200k window. Against a 1M window the
/// same line is a 380k ceiling that oldest-first eviction never reaches, so the
/// region hoards. A measured run grew 3k -> 196k tokens per request and burned
/// 3.3M cache-write tokens without finishing.
#[test]
fn a_percentage_budget_on_an_evicting_region_is_warned_with_the_resolved_ceiling() {
    let toml =
        manifest_with_regions("name = \"raw_findings\"\nkind = \"temporary\"\nbudget = \"38%\"");
    let findings = lint(&toml, &LintEnv::default_with_windows());
    let found = findings
        .iter()
        .find(|f| f.code == "unbounded-percentage-budget")
        .expect("the region is warned about");
    assert_eq!(found.severity, LintSeverity::Warning);
    // The number is the whole point: "38%" is not alarming until the
    // denominator is named.
    assert!(found.message.contains("380000"), "{}", found.message);
    assert!(
        found.message.contains("claude-sonnet-5"),
        "{}",
        found.message
    );
    assert!(found.message.contains("1000000"), "{}", found.message);
    assert!(
        found
            .fix
            .as_deref()
            .unwrap_or_default()
            .contains("max = ..."),
        "{found:?}"
    );
}

#[test]
fn a_percentage_budget_with_a_max_guard_is_left_alone() {
    // The fix the issue reports as working completely.
    let toml = manifest_with_regions(
        "name = \"raw_findings\"\nkind = \"temporary\"\nbudget = { percent = \"38%\", max = 24000 }",
    );
    assert!(
        !codes(&lint(&toml, &LintEnv::default_with_windows()))
            .contains(&"unbounded-percentage-budget")
    );
}

#[test]
fn an_absolute_budget_is_never_warned_about() {
    let toml =
        manifest_with_regions("name = \"raw_findings\"\nkind = \"temporary\"\nbudget = 24000");
    assert!(
        !codes(&lint(&toml, &LintEnv::default_with_windows()))
            .contains(&"unbounded-percentage-budget")
    );
}

/// A region that holds what it is given has no bound to fail to reach, so a
/// percentage there means exactly what its author intended.
#[test]
fn a_pinned_region_with_a_percentage_budget_is_fine() {
    let toml = manifest_with_regions("name = \"notes\"\nkind = \"pinned\"\nbudget = \"38%\"");
    assert!(
        !codes(&lint(&toml, &LintEnv::default_with_windows()))
            .contains(&"unbounded-percentage-budget")
    );
}

#[test]
fn every_evicting_kind_is_covered_not_just_temporary() {
    for decl in [
        "name = \"r\"\nkind = \"clearable\"\nbudget = \"38%\"",
        "name = \"r\"\nkind = { kind = \"sliding_window\", max_items = 20 }\nbudget = \"38%\"",
        "name = \"r\"\nkind = \"compacting\"\nbudget = \"38%\"\ncompact_at = 0.8",
    ] {
        let toml = manifest_with_regions(decl);
        assert!(
            codes(&lint(&toml, &LintEnv::default_with_windows()))
                .contains(&"unbounded-percentage-budget"),
            "{decl}"
        );
    }
}

/// An output stage on a model that cannot call tools has one way to answer:
/// a declared artifact fed by a routed produced part. Each half missing is
/// named; a stage with both, a tool-capable model, an open route, or no
/// output duty at all is left alone.
#[test]
fn an_output_stage_whose_models_cannot_call_tools_must_route_a_declared_file() {
    let manifest = |model: &str, mode: &str, extra: &str| {
        format!(
            r#"
[blueprint]
name = "builder"
version = "0.1.0"
description = "d"

[graph]

[[graph.stages]]
name = "build"
mode = "{mode}"
model = {{ models = [{model}] }}
description = "Builds"
max_iterations = 3

[graph.layout]
total_budget_tokens = 0

[[graph.layout.regions]]
name = "task"
kind = "pinned"
budget = 1000

[[graph.layout.regions]]
name = "model"
kind = "pinned"
budget = 1000
accepts = ["model/*"]

[[graph.layout.regions]]
name = "conversation"
kind = {{ kind = "sliding_window", max_items = 50 }}
budget = 10000
{extra}
"#
        )
    };
    let meshy = r#"{ provider = "meshy", model = "image-to-3d" }"#;
    let routing = "[graph.stages.output_routing]\n\"model/*\" = \"model\"\n";
    let artifact = "[[graph.stages.output.artifacts]]\nname = \"mesh\"\nmime_type = \"model/*\"\n";
    let code = "output-stage-cannot-answer";

    let bare = lint(&manifest(meshy, "output", ""), &LintEnv::default());
    let found = with_code(&bare, code);
    assert_eq!(found.len(), 1, "{:?}", codes(&bare));
    assert_eq!(found[0].severity, LintSeverity::Error);
    assert_eq!(found[0].stage.as_deref(), Some("build"));
    assert!(
        found[0].message.contains("meshy/image-to-3d")
            && found[0]
                .message
                .contains("declares no artifact and routes no produced part"),
        "{}",
        found[0].message
    );
    assert!(
        found[0]
            .fix
            .as_deref()
            .unwrap_or_default()
            .contains("output_routing")
    );

    let routed = lint(&manifest(meshy, "output", routing), &LintEnv::default());
    assert!(
        with_code(&routed, code)[0]
            .message
            .contains("declares no artifact for it"),
        "{:?}",
        codes(&routed)
    );
    let declared = lint(&manifest(meshy, "output", artifact), &LintEnv::default());
    assert!(
        with_code(&declared, code)[0]
            .message
            .contains("routes no produced part"),
        "{:?}",
        codes(&declared)
    );

    let both = format!("{routing}{artifact}");
    for (label, text) in [
        ("both halves", manifest(meshy, "output", &both)),
        (
            "a tool-capable model",
            manifest(
                r#"{ provider = "anthropic", model = "claude-sonnet-5" }"#,
                "output",
                "",
            ),
        ),
        (
            "an open route",
            manifest(r#"{ model = "something" }"#, "output", ""),
        ),
        ("no output duty", manifest(meshy, "autonomous", "")),
    ] {
        let findings = lint(&text, &LintEnv::default());
        assert!(
            with_code(&findings, code).is_empty(),
            "{label}: {:?}",
            codes(&findings)
        );
    }
}

/// A 3D generator's "window" is the ceiling its REST call takes a mesh under,
/// not a context window: one Meshy stage made every percentage region resolve
/// against 64 million tokens. The widest window is the widest among the models
/// that write text.
#[test]
fn a_model_that_does_not_write_text_is_not_the_widest_window() {
    let toml = format!(
        "{}\n[[graph.stages]]\nname = \"build\"\nmode = \"output\"\nmodel = {{ models = [{{ provider = \"meshy\", model = \"image-to-3d\" }}] }}\ndescription = \"Builds\"\nmax_iterations = 3\n",
        manifest_with_regions("name = \"raw_findings\"\nkind = \"temporary\"\nbudget = \"38%\"")
    );
    let findings = lint(&toml, &LintEnv::default_with_windows());
    let found = findings
        .iter()
        .find(|f| f.code == "unbounded-percentage-budget")
        .expect("the region is still warned about, against the text model");
    assert!(
        found.message.contains("claude-sonnet-5") && found.message.contains("380000"),
        "{}",
        found.message
    );
    assert!(!found.message.contains("64000000"), "{}", found.message);
}

/// Without a window there is no number to report, and a warning that cannot say
/// what "38%" comes to is one nobody acts on.
#[test]
fn nothing_is_said_when_no_declared_model_has_a_known_window() {
    let toml =
        manifest_with_regions("name = \"raw_findings\"\nkind = \"temporary\"\nbudget = \"38%\"");
    assert!(!codes(&lint(&toml, &LintEnv::default())).contains(&"unbounded-percentage-budget"));
}

/// One warning per region, not per layout that declares it: the fix is on the
/// declaration.
#[test]
fn a_region_declared_in_two_layouts_is_named_once() {
    let toml = r#"
[blueprint]
name = "lint-fixture"
version = "0.1.0"
description = "a fixture"

[graph]

[[graph.stages]]
name = "work"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
allow_complete = true
[graph.stages.layout]
total_budget_tokens = 0
[[graph.stages.layout.regions]]
name = "raw_findings"
kind = "temporary"
budget = "38%"

[graph.layout]
total_budget_tokens = 0

[[graph.layout.regions]]
name = "system"
kind = "pinned"
budget = 1000

[[graph.layout.regions]]
name = "raw_findings"
kind = "temporary"
budget = "38%"
"#;
    let hits = codes(&lint(toml, &LintEnv::default_with_windows()))
        .into_iter()
        .filter(|c| *c == "unbounded-percentage-budget")
        .count();
    assert_eq!(hits, 1);
}

/// A stage that routes tool output into a region and can read files, but has no
/// `context_read`, leaves the model one read verb and it is the wrong one.
///
/// This is the authoring shape behind 90 of 168 failed `read_file` calls across
/// 152 local runs: the pointer says the output is in `raw_findings`, the only
/// tool that could act on that is not granted, and `read_file("raw_findings")`
/// is what the model reaches for.
#[test]
fn routing_into_a_region_without_context_read_is_flagged() {
    let manifest = r#"
[blueprint]
name = "router"
version = "0.1.0"

[graph]
entry = "gather"

[[graph.stages]]
name = "gather"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
tools = ["read_file", "web_fetch"]
system_prompt = "gather"
[graph.stages.tool_routing]
default_region = "raw_findings"

[graph.layout]
total_budget_tokens = 0

[[graph.layout.regions]]
name = "raw_findings"
kind = "temporary"
budget = "30%"

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 20 }
budget = "12%"
"#;
    let findings = lint(manifest, &LintEnv::default());
    assert!(
        findings
            .iter()
            .any(|f| f.code == "routing-without-region-read"),
        "expected the routing warning, got: {findings:?}"
    );
}

/// Granting `context_read` settles it, and so does routing to `conversation`,
/// where no pointer is written and there is nothing to go and read.
#[test]
fn routing_with_context_read_or_to_conversation_is_not_flagged() {
    let with_grant = r#"
[blueprint]
name = "router"
version = "0.1.0"

[graph]
entry = "gather"

[[graph.stages]]
name = "gather"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
tools = ["read_file", "context_read"]
system_prompt = "gather"
[graph.stages.tool_routing]
default_region = "raw_findings"

[graph.layout]
total_budget_tokens = 0

[[graph.layout.regions]]
name = "raw_findings"
kind = "temporary"
budget = "30%"

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 20 }
budget = "12%"
"#;
    let to_conversation = with_grant
        .replace("\"read_file\", \"context_read\"", "\"read_file\"")
        .replace(
            "default_region = \"raw_findings\"",
            "default_region = \"conversation\"",
        );

    for (label, manifest) in [
        ("granted", with_grant),
        ("conversation", to_conversation.as_str()),
    ] {
        let findings = lint(manifest, &LintEnv::default());
        assert!(
            !findings
                .iter()
                .any(|f| f.code == "routing-without-region-read"),
            "{label}: unexpected warning in {findings:?}"
        );
    }
}

// ─── required-region-unenforceable ───────────────────────────────────────────

/// The shape that made `required` inert: the flag is set, and every stage that
/// could satisfy it lacks the tool to write context. The runtime gate skips
/// such a stage by design, so nothing anywhere enforces the region - and the
/// stage downstream, told to build its deliverable from it, invents one instead.
#[test]
fn a_required_region_no_stage_can_write_is_warned_about() {
    let toml = manifest_with_regions(
        "name = \"sources_index\"\nkind = \"pinned\"\nbudget = 2000\nrequired = true",
    );
    let findings = lint(&toml, &LintEnv::default());
    let found = with_code(&findings, "required-region-unenforceable");
    assert_eq!(found.len(), 1, "{:?}", codes(&findings));
    assert_eq!(found[0].severity, LintSeverity::Warning);
    assert!(
        found[0].message.contains("sources_index"),
        "{}",
        found[0].message
    );
    assert!(
        found[0]
            .fix
            .as_deref()
            .is_some_and(|f| f.contains("context_append")),
        "{:?}",
        found[0].fix
    );
}

/// One stage able to write context is enough: that is where the gate binds.
#[test]
fn a_required_region_some_stage_can_write_is_not_warned_about() {
    let toml = manifest_with_regions(
        "name = \"sources_index\"\nkind = \"pinned\"\nbudget = 2000\nrequired = true",
    )
    .replace(
        "allow_complete = true",
        "allow_complete = true\ntools = [\"context_append\"]",
    );
    let findings = lint(&toml, &LintEnv::default());
    assert!(
        with_code(&findings, "required-region-unenforceable").is_empty(),
        "{:?}",
        codes(&findings)
    );
}

/// Regions an input fills are exempt, the same exemption the runtime gate
/// makes: the caller owns them and they are checked at spawn, so no stage ever
/// owed one. Without this every bundled agent's `query` region would warn.
#[test]
fn a_required_region_an_input_fills_is_not_warned_about() {
    let toml = manifest_with_regions(
        "name = \"query\"\nkind = \"pinned\"\nbudget = 2000\nrequired = true",
    ) + "\n[[graph.inputs]]\nname = \"task\"\ntype = \"text\"\nbinds = [{ region = \"query\" }]\n";
    let findings = lint(&toml, &LintEnv::default());
    assert!(
        with_code(&findings, "required-region-unenforceable").is_empty(),
        "{:?}",
        codes(&findings)
    );
}

/// Named once per region however many stages share the layout: the fix is on
/// the declaration, so repeating it per stage is noise.
#[test]
fn an_unenforceable_required_region_is_named_once_across_stages() {
    let toml = manifest_with_regions(
        "name = \"sources_index\"\nkind = \"pinned\"\nbudget = 2000\nrequired = true",
    )
    .replace(
        "[graph.layout]\n",
        "[[graph.stages]]\nname = \"second\"\n\
         model = { models = [{ provider = \"anthropic\", model = \"claude-sonnet-5\" }] }\n\
         max_iterations = 10\nallow_complete = true\n\n[graph.layout]\n",
    );
    let hits = codes(&lint(&toml, &LintEnv::default()))
        .into_iter()
        .filter(|c| *c == "required-region-unenforceable")
        .count();
    assert_eq!(hits, 1);
}

// ─── eviction-order-unevictable ──────────────────────────────────────────────

/// The fixture's layout with `extra` as one more region, its eviction order
/// set to `order`.
fn manifest_evicting(extra: &str, order: &str) -> String {
    manifest_with_regions(extra).replace(
        "total_budget_tokens = 0\n",
        &format!("total_budget_tokens = 0\neviction_order = {order}\n"),
    )
}

/// Naming a pinned region asks it to give way first, and nothing ever takes
/// from it; naming a temporary one is what the order is for.
#[test]
fn an_eviction_order_naming_a_region_eviction_never_takes_from_is_warned_about() {
    let toml = manifest_evicting(
        "name = \"scratch\"\nkind = \"temporary\"\nbudget = 2000",
        "[\"scratch\", \"system\"]",
    );
    let findings = lint(&toml, &LintEnv::default());
    let found = with_code(&findings, "eviction-order-unevictable");
    assert_eq!(found.len(), 1, "{:?}", codes(&findings));
    assert_eq!(found[0].severity, LintSeverity::Warning);
    assert!(
        found[0].message.contains("'system'"),
        "{}",
        found[0].message
    );
    assert!(
        found[0]
            .fix
            .as_deref()
            .is_some_and(|f| f.contains("out of eviction_order")),
        "{:?}",
        found[0].fix
    );
}

/// `admission = "reject"` keeps eviction away from a region whatever its kind.
#[test]
fn an_eviction_order_naming_a_rejecting_region_is_warned_about() {
    let toml = manifest_evicting(
        "name = \"scratch\"\nkind = \"temporary\"\nbudget = 2000\nadmission = \"reject\"",
        "[\"scratch\"]",
    );
    let findings = lint(&toml, &LintEnv::default());
    let found = with_code(&findings, "eviction-order-unevictable");
    assert_eq!(found.len(), 1, "{:?}", codes(&findings));
    assert!(
        found[0].message.contains("'scratch'"),
        "{}",
        found[0].message
    );
}

/// A region two layouts both name is one mistake, said once.
#[test]
fn an_unevictable_region_two_layouts_name_is_warned_about_once() {
    let toml = manifest_evicting(
        "name = \"scratch\"\nkind = \"temporary\"\nbudget = 2000",
        "[\"system\"]",
    )
    .replace(
        "allow_complete = true\n",
        "allow_complete = true\nlayout = { total_budget_tokens = 0, eviction_order = [\"system\"], \
         regions = [{ name = \"system\", kind = \"pinned\", budget = 1000 }] }\n",
    );
    let hits = codes(&lint(&toml, &LintEnv::default()))
        .into_iter()
        .filter(|c| *c == "eviction-order-unevictable")
        .count();
    assert_eq!(hits, 1);
}

// ─── with_provider_catalogs ───────────────────────────────────────────────────

/// A blueprint pinning `<provider>/<model>` on its one stage, for the builder
/// tests below.
fn blueprint_pinning(pairs: &[(&str, &str)]) -> String {
    let listed = pairs
        .iter()
        .map(|(p, m)| format!("{{ provider = \"{p}\", model = \"{m}\" }}"))
        .collect::<Vec<_>>()
        .join(", ");
    manifest(&format!(
        "[[graph.stages]]\nname = \"main\"\n\
         model = {{ models = [{listed}] }}\nmax_iterations = 10\n"
    ))
}

/// A blueprint naming models and leaving every route open.
fn blueprint_open(models: &[&str]) -> String {
    let listed = models
        .iter()
        .map(|m| format!("{{ model = \"{m}\" }}"))
        .collect::<Vec<_>>()
        .join(", ");
    manifest(&format!(
        "[[graph.stages]]\nname = \"main\"\n\
         model = {{ models = [{listed}] }}\nmax_iterations = 10\n"
    ))
}

/// A natively registered provider serving a fixed set of models, for the open
/// entries: a script provider is resolved on demand and so is never in
/// `native_providers`, which is the list the resolver asks first.
struct NativeProvider(Vec<String>);

/// The same, but it publishes what it serves and can say why it refuses the
/// rest - the shape of a provider whose catalogue depends on the account.
struct ExplainingProvider {
    serves: Vec<String>,
    reason: String,
}

#[async_trait::async_trait]
impl leviath_providers::Provider for ExplainingProvider {
    async fn infer(
        &self,
        _r: &leviath_providers::InferenceRequest,
    ) -> leviath_providers::Result<leviath_providers::InferenceResponse> {
        Err(leviath_providers::ProviderError::Other("t".to_string()))
    }
    async fn count_tokens(&self, _t: &str, _m: &str) -> usize {
        1
    }
    fn max_context_tokens(&self, _m: &str) -> usize {
        1000
    }
    fn name(&self) -> &str {
        "explaining"
    }
    fn capabilities(&self, _m: &str) -> leviath_providers::ModelCapabilities {
        leviath_providers::ModelCapabilities::default()
    }
    fn served_catalog(&self) -> Option<Vec<String>> {
        Some(self.serves.clone())
    }
    fn refusal_reason(&self, model_key: &str) -> Option<String> {
        (!self.serves.iter().any(|m| m == model_key)).then(|| self.reason.clone())
    }
}

#[async_trait::async_trait]
impl leviath_providers::Provider for NativeProvider {
    async fn infer(
        &self,
        _r: &leviath_providers::InferenceRequest,
    ) -> leviath_providers::Result<leviath_providers::InferenceResponse> {
        Err(leviath_providers::ProviderError::Other("t".to_string()))
    }
    async fn count_tokens(&self, _t: &str, _m: &str) -> usize {
        1
    }
    fn max_context_tokens(&self, _m: &str) -> usize {
        1000
    }
    fn name(&self) -> &str {
        "native"
    }
    fn capabilities(&self, _m: &str) -> leviath_providers::ModelCapabilities {
        leviath_providers::ModelCapabilities::default()
    }
    fn serves_model(&self, model_key: &str) -> Option<String> {
        self.0
            .iter()
            .any(|m| m == model_key)
            .then(|| model_key.to_string())
    }
}

/// A registry holding one script provider, written to disk so the layer
/// compiles it the way it would in production.
///
/// `list_models` returns a fixed array rather than calling out, so the
/// catalogue is real without the test touching the network.
fn script_registry(
    name: &str,
    serves: Option<&[&str]>,
) -> (leviath_runtime::ProviderRegistry, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("a temp dir");
    let body = match serves {
        Some(models) => {
            let entries = models
                .iter()
                .map(|m| {
                    format!(
                        "#{{ id: \"{m}\", display_name: \"{m}\", \
                         max_context_tokens: 8192, max_output_tokens: 1024 }}"
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("fn list_models(state) {{ [{entries}] }}\n")
        }
        None => String::new(),
    };
    std::fs::write(
        dir.path().join(format!("{name}.rhai")),
        format!(
            "fn initialize(config) {{ #{{}} }}\n\
             fn inference(state, request) {{ #{{ content: \"x\" }} }}\n{body}"
        ),
    )
    .expect("the script writes");
    let layer = leviath_runtime::script_provider::ScriptProviderLayer::new(
        dir.path().to_path_buf(),
        HashMap::new(),
        HashMap::new(),
        None,
        Vec::new(),
    );
    let registry =
        leviath_runtime::ProviderRegistry::new().with_script_layer(std::sync::Arc::new(layer));
    (registry, dir)
}

/// A registry of providers that publish what they serve, under the names
/// the retention table knows.
fn registry_serving(providers: &[(&str, &str)]) -> leviath_runtime::ProviderRegistry {
    let mut registry = leviath_runtime::ProviderRegistry::new();
    for (name, model) in providers {
        registry.register(
            (*name).to_string(),
            std::sync::Arc::new(ExplainingProvider {
                serves: vec![(*model).to_string()],
                reason: "not here".to_string(),
            }),
        );
    }
    registry
}

fn zero_retention_config() -> crate::config::Config {
    let mut config = crate::config::Config::default();
    config.providers.zero_retention = true;
    config
}

/// Under the switch, the model a stage would start on is judged the way the
/// spawn gate judges it: a provider that keeps an abuse log is an error
/// carrying the reason; local inference passes. Off, nothing is recorded.
#[test]
fn a_retaining_head_model_is_an_error_under_zero_retention() {
    let registry = registry_serving(&[("openai", "gpt-5.5"), ("ollama", "q")]);
    let bp = blueprint_pinning(&[("openai", "gpt-5.5"), ("ollama", "q")]);

    let env =
        LintEnv::default().with_retention(&graph_of(&bp), &zero_retention_config(), &registry);
    let refusals = &env.retention_refusals["main"];
    assert_eq!(refusals.len(), 1, "{refusals:?}");
    let refusal = &refusals[0];
    assert!(refusal.head);
    assert_eq!(refusal.route, "openai/gpt-5.5");
    let reason = refusal.reason.as_str();
    assert!(reason.contains("30 days"), "{reason}");

    let findings = lint(&bp, &env);
    assert_eq!(codes(&findings), ["retention-not-zero"]);
    let message = findings[0].message.as_str();
    assert!(message.contains("openai/gpt-5.5"), "{message}");
    assert!(message.contains("30 days"), "{message}");
    let fix = findings[0].fix.clone().unwrap_or_default();
    assert!(fix.contains("zero_retention_agreements"), "{fix}");

    let off = LintEnv::default().with_retention(
        &graph_of(&bp),
        &crate::config::Config::default(),
        &registry,
    );
    assert!(off.retention_refusals.is_empty());
    assert!(lint(&bp, &off).is_empty());
}

/// A fallback that keeps something is a warning, since failover drops it
/// rather than the spawn refusing it; a fallback on a provider this install
/// does not have is not judged, because it is not in the failover list
/// either; a declared agreement clears the provider.
#[test]
fn a_retaining_fallback_is_a_warning_under_zero_retention() {
    let registry = registry_serving(&[("openai", "gpt-5.5"), ("ollama", "q")]);
    let bp = blueprint_pinning(&[
        ("ollama", "q"),
        ("openai", "gpt-5.5"),
        ("anthropic", "claude-sonnet-5"),
    ]);

    let env =
        LintEnv::default().with_retention(&graph_of(&bp), &zero_retention_config(), &registry);
    let refusals = &env.retention_refusals["main"];
    assert_eq!(refusals.len(), 1, "{refusals:?}");
    let refusal = &refusals[0];
    assert!(!refusal.head);
    assert_eq!(refusal.route, "openai/gpt-5.5");

    let findings = lint(&bp, &env);
    assert_eq!(codes(&findings), ["retention-fallback-dropped"]);
    assert_eq!(findings[0].severity, LintSeverity::Warning);
    let message = findings[0].message.as_str();
    assert!(message.contains("failover skips it"), "{message}");

    let mut config = zero_retention_config();
    config.providers.zero_retention_agreements = vec!["openai".to_string()];
    let cleared = LintEnv::default().with_retention(&graph_of(&bp), &config, &registry);
    assert!(
        cleared.retention_refusals.is_empty(),
        "{:?}",
        cleared.retention_refusals
    );
}

/// The case the whole check exists for: a Rhai provider that answers
/// `list_models` publishes a complete catalogue, and the blueprint's model is
/// then checkable.
#[tokio::test]
async fn a_script_providers_catalogue_reaches_the_lint() {
    let (registry, _dir) = script_registry("groq", Some(&["llama-4-scout"]));
    registry
        .prime_capabilities(std::time::Duration::from_secs(5), &["groq"])
        .await;
    let bp = blueprint_pinning(&[("groq", "llama-3.1-70b")]);

    let env = LintEnv::default().with_provider_catalogs(
        &graph_of(&bp),
        &crate::config::Config::default(),
        &registry,
    );

    assert_eq!(
        env.provider_catalogs.get("groq"),
        Some(&ProviderCatalog::Complete(vec![
            "llama-4-scout".to_string()
        ]))
    );
    assert!(
        lint(&bp, &env).iter().any(|f| f.code == "unserved-model"),
        "the catalogue is what makes the model checkable"
    );
}

/// The env collects the provider's own reason for refusing an entry, so the
/// check has it without holding a registry.
///
/// The lint runs over plain data, and the provider is only in hand while the
/// env is built - so a reason not gathered here is a reason nobody can print.
#[tokio::test]
async fn a_providers_reason_for_refusing_reaches_the_lint() {
    let mut registry = leviath_runtime::ProviderRegistry::new();
    registry.register(
        "codexish".to_string(),
        std::sync::Arc::new(ExplainingProvider {
            serves: vec!["gpt-5.5".to_string()],
            reason: "your ChatGPT plus plan does not include it".to_string(),
        }),
    );
    let bp = blueprint_pinning(&[("codexish", "gpt-5.3-spark")]);

    let env = LintEnv::default().with_provider_catalogs(
        &graph_of(&bp),
        &crate::config::Config::default(),
        &registry,
    );

    assert_eq!(
        env.provider_refusals
            .get("codexish/gpt-5.3-spark")
            .map(String::as_str),
        Some("your ChatGPT plus plan does not include it")
    );
    // And the finding carries it rather than the generic wording.
    let message = &lint(&bp, &env)
        .into_iter()
        .find(|f| f.code == "unserved-model")
        .expect("the catalogue makes it checkable")
        .message;
    assert!(message.contains("plus plan does not include"), "{message}");
}

/// A provider with nothing to add contributes no entry, so the check keeps its
/// own wording.
#[tokio::test]
async fn a_provider_with_no_reason_adds_none() {
    let mut registry = leviath_runtime::ProviderRegistry::new();
    registry.register(
        "plain".to_string(),
        std::sync::Arc::new(NativeProvider(vec!["gpt-5.5".to_string()])),
    );
    let bp = blueprint_pinning(&[("plain", "nope")]);

    let env = LintEnv::default().with_provider_catalogs(
        &graph_of(&bp),
        &crate::config::Config::default(),
        &registry,
    );

    assert!(env.provider_refusals.is_empty());
}

/// A script provider with no `list_models` is recorded as having said nothing,
/// which is a warning rather than a refusal.
#[tokio::test]
async fn a_silent_script_provider_is_recorded_as_such() {
    let (registry, _dir) = script_registry("quiet", None);
    registry
        .prime_capabilities(std::time::Duration::from_secs(5), &["quiet"])
        .await;
    let bp = blueprint_pinning(&[("quiet", "anything")]);

    let env = LintEnv::default().with_provider_catalogs(
        &graph_of(&bp),
        &crate::config::Config::default(),
        &registry,
    );

    assert_eq!(
        env.provider_catalogs.get("quiet"),
        Some(&ProviderCatalog::ScriptSaidNothing)
    );
}

/// A provider this install cannot reach is left out of the map entirely, so its
/// entries go unchecked. That is `no-reachable-provider`'s question, and
/// answering it here as well would tell a machine that simply lacks a provider
/// that its blueprint is wrong.
#[tokio::test]
async fn an_unreachable_provider_is_left_out_of_the_map() {
    let (registry, _dir) = script_registry("groq", Some(&["llama-4-scout"]));
    let bp = blueprint_pinning(&[("nobody-has-this", "some-model")]);

    let env = LintEnv::default().with_provider_catalogs(
        &graph_of(&bp),
        &crate::config::Config::default(),
        &registry,
    );

    assert!(
        env.provider_catalogs.is_empty(),
        "{:?}",
        env.provider_catalogs
    );
}

/// The same provider named twice in a stage's list is asked once. Asking again
/// would compile the script a second time for an answer already in hand.
#[tokio::test]
async fn a_provider_named_twice_is_asked_once() {
    let (registry, _dir) = script_registry("groq", Some(&["llama-4-scout"]));
    registry
        .prime_capabilities(std::time::Duration::from_secs(5), &["groq"])
        .await;
    let bp = blueprint_pinning(&[("groq", "llama-4-scout"), ("groq", "llama-3.1-70b")]);

    let env = LintEnv::default().with_provider_catalogs(
        &graph_of(&bp),
        &crate::config::Config::default(),
        &registry,
    );

    assert_eq!(env.provider_catalogs.len(), 1);
}

/// An open entry is answered the way the resolver answers it, so the machine's
/// default script provider is asked too. Without that, a local box serving
/// exactly the model the blueprint named would be reported as unrouted.
#[tokio::test]
async fn an_open_entry_is_routed_through_the_default_script_provider() {
    let (registry, _dir) = script_registry("spark", Some(&["local-fast"]));
    registry
        .prime_capabilities(std::time::Duration::from_secs(5), &["spark"])
        .await;
    let bp = blueprint_open(&["local-fast", "nobody-serves-this"]);
    let config = crate::config::Config {
        default_provider: "spark".to_string(),
        ..crate::config::Config::default()
    };

    let env = LintEnv::default().with_provider_catalogs(&graph_of(&bp), &config, &registry);

    assert_eq!(
        env.unrouted_models,
        known_tools(&["nobody-serves-this"]),
        "the default script provider serves local-fast, so only the other is unrouted"
    );
}

/// The ordinary shape: a natively registered provider answers the open-route
/// question, exactly as `resolve_stage_candidates` asks it.
///
/// The script-provider tests above cannot reach this path at all - a script
/// provider is compiled on demand and so is never in `native_providers` - so
/// without a native provider in the registry the loop that asks them runs zero
/// times.
#[test]
fn a_native_provider_answers_the_open_route_question() {
    let mut registry = leviath_runtime::ProviderRegistry::new();
    registry.register(
        "anthropic".to_string(),
        std::sync::Arc::new(NativeProvider(vec!["claude-sonnet-5".to_string()])),
    );
    let bp = blueprint_open(&["claude-sonnet-5", "nobody-serves-this"]);

    let env = LintEnv::default().with_provider_catalogs(
        &graph_of(&bp),
        &crate::config::Config::default(),
        &registry,
    );

    assert_eq!(
        env.unrouted_models,
        known_tools(&["nobody-serves-this"]),
        "anthropic serves one of the two, so only the other is unrouted"
    );
    assert!(
        env.provider_catalogs.is_empty(),
        "an open entry pins no provider, so there is no catalogue to record"
    );
}

/// A native provider that publishes nothing is left unchecked rather than
/// reported. Only a script provider's silence is worth naming: every built-in
/// is either covered by the compiled-in table `unknown-model` reads or has a
/// genuinely open catalogue.
#[test]
fn a_silent_native_provider_is_not_reported_as_unchecked() {
    let mut registry = leviath_runtime::ProviderRegistry::new();
    registry.register(
        "anthropic".to_string(),
        std::sync::Arc::new(NativeProvider(vec!["claude-sonnet-5".to_string()])),
    );
    let bp = blueprint_pinning(&[("anthropic", "claude-sonnet-5")]);

    let env = LintEnv::default().with_provider_catalogs(
        &graph_of(&bp),
        &crate::config::Config::default(),
        &registry,
    );

    assert!(
        env.provider_catalogs.is_empty(),
        "a native provider that publishes nothing is not `catalog-unchecked`"
    );
}

// ─── Mime a stage takes but its models cannot see ───────────────────────────

#[test]
fn a_stage_taking_mime_its_models_cannot_see_is_warned_once() {
    let manifest = r#"
[blueprint]
name = "artist"
version = "0.1.0"
description = "d"

[graph]

[[graph.stages]]
name = "look"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
description = "Main"
max_iterations = 10
tools = ["read_file"]

[[graph.stages]]
name = "listen"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
description = "Hears"
max_iterations = 10
tools = ["read_file"]
input_accepts = ["audio/*"]

[[graph.stages]]
name = "hears_anyway"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }, { provider = "gemini", model = "gemini-2.5-pro" }] }
description = "Hears"
max_iterations = 10
tools = ["read_file"]
input_accepts = ["audio/*"]

[[graph.stages]]
name = "open_route"
model = { models = [{ model = "something" }] }
description = "Unknown"
max_iterations = 10
tools = ["read_file"]
input_accepts = ["audio/*"]

[[graph.stages]]
name = "mixed"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
description = "Sees the pictures, not the sound"
max_iterations = 10
tools = ["read_file"]
input_accepts = ["audio/*", "image/*"]

[graph.layout]
total_budget_tokens = 0

[[graph.layout.regions]]
name = "task"
kind = "pinned"
budget = 1000

[[graph.layout.regions]]
name = "storyboard"
kind = "pinned"
budget = 1000
accepts = ["image/*"]

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 50 }
budget = 10000
"#;
    let findings = lint(manifest, &LintEnv::default());
    assert!(with_code(&findings, "tool-accepts-ungranted").is_empty());
    let unseen = with_code(&findings, "mime-unseen");
    assert_eq!(unseen.len(), 2, "{:?}", codes(&findings));
    // A stage whose model sees some of what it takes is a pipeline working as
    // designed: said as a note, naming only what is unseen.
    let mixed = unseen
        .iter()
        .find(|f| f.stage.as_deref() == Some("mixed"))
        .expect("the mixed stage is noted");
    assert_eq!(mixed.severity, LintSeverity::Note);
    assert!(mixed.message.contains("takes audio/*"), "{}", mixed.message);
    assert!(!mixed.message.contains("image/*"), "{}", mixed.message);
    let unseen: Vec<&&LintFinding> = unseen
        .iter()
        .filter(|f| f.stage.as_deref() == Some("listen"))
        .collect();
    assert_eq!(unseen[0].severity, LintSeverity::Warning);
    assert_eq!(unseen[0].stage.as_deref(), Some("listen"));
    assert!(
        unseen[0].message.contains("takes audio/*"),
        "{}",
        unseen[0].message
    );
    assert!(
        unseen[0].message.contains("anthropic/claude-sonnet-5"),
        "{}",
        unseen[0].message
    );
    assert!(
        unseen[0]
            .fix
            .as_deref()
            .unwrap_or_default()
            .contains("as_text")
    );
}

/// A `tool_accepts` limit on a tool the stage never grants is said once,
/// unless a group grant makes the question one for the install.
#[test]
fn a_tool_limit_on_an_ungranted_tool_is_said_once() {
    let text = manifest(
        r#"

[[graph.stages]]
name = "named"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
description = "Named"
max_iterations = 10
tools = ["read_file", "spawn_agent"]
[graph.stages.tool_accepts]
spawn_agent = ["image/*"]
ghost = ["audio/*", "video/mp4"]

[[graph.stages]]
name = "grouped"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
description = "Grouped"
max_iterations = 10
tools = ["@builtin"]
[graph.stages.tool_accepts]
ghost = ["audio/*"]
"#,
    );
    let findings = lint(&text, &LintEnv::default());
    let said = with_code(&findings, "tool-accepts-ungranted");
    assert_eq!(said.len(), 1, "{:?}", codes(&findings));
    assert_eq!(said[0].stage.as_deref(), Some("named"));
    assert!(
        said[0]
            .message
            .contains("limits 'ghost' to audio/*, video/mp4"),
        "{}",
        said[0].message
    );
    assert!(
        said[0]
            .fix
            .as_deref()
            .is_some_and(|f| f.contains("the stage's tools"))
    );
}

/// A blueprint that sets a tool more permissive than its built-in default is
/// warned that the runtime clamps it; a tool a blueprint may pre-approve, and
/// one set no looser than the default, are left alone.
#[test]
fn a_blueprint_that_loosens_a_tool_is_told_the_runtime_clamps_it() {
    let toml = manifest(
        r#"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
tools = ["shell", "write_file", "read_file", "web_search"]
[graph.stages.tool_permissions]
shell = "allow"
write_file = "allow"
read_file = "allow"
web_search = "allow"
"#,
    );
    let findings = lint(&toml, &LintEnv::default());
    let clamped = with_code(&findings, "blueprint-permission-clamped");
    // shell and write_file default to ask, so allow is clamped; read_file
    // already defaults to allow (nothing loosened); web_search is on the
    // pre-approvable list, so a blueprint may grant it.
    assert_eq!(clamped.len(), 2, "{:?}", codes(&findings));
    assert!(
        clamped.iter().any(|f| f.message.contains("shell")),
        "{clamped:?}"
    );
    assert!(
        clamped.iter().any(|f| f.message.contains("write_file")),
        "{clamped:?}"
    );
    assert!(clamped.iter().all(|f| f.stage.as_deref() == Some("main")));
    assert!(
        clamped.iter().all(|f| f.message.contains("clamps it back")
            && f.fix.as_deref().is_some_and(|x| x.contains("--yolo"))),
        "{clamped:?}"
    );

    // A stage that denies a tool (stricter than the default) is not loosening.
    let strict = lint(
        &toml.replace("shell = \"allow\"", "shell = \"deny\""),
        &LintEnv::default(),
    );
    assert!(
        !with_code(&strict, "blueprint-permission-clamped")
            .iter()
            .any(|f| f.message.contains("shell")),
        "{strict:?}"
    );

    // The graph-level table is checked too, once per tool.
    let graph_level = manifest(
        r#"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
tools = ["write_file"]

[graph.tool_permissions]
write_file = "allow"
"#,
    );
    let findings = lint(&graph_level, &LintEnv::default());
    let clamped = with_code(&findings, "blueprint-permission-clamped");
    assert_eq!(clamped.len(), 1, "{:?}", codes(&findings));
    assert!(
        clamped[0].message.contains("the graph's tool_permissions"),
        "{clamped:?}"
    );
}

/// A blueprint whose stage runs on `provider/model` with regions totalling
/// `conversation_tokens` beside a small pinned one.
fn manifest_on(provider: &str, model: &str, conversation_tokens: usize) -> String {
    format!(
        r#"
[blueprint]
name = "lint-fixture"
version = "0.1.0"
description = "a fixture"

[graph]

[[graph.stages]]
name = "work"
model = {{ models = [{{ provider = "{provider}", model = "{model}" }}] }}
max_iterations = 10
allow_complete = true

[graph.layout]
total_budget_tokens = 0

[[graph.layout.regions]]
name = "system"
kind = "pinned"
budget = 1000

[[graph.layout.regions]]
name = "conversation"
kind = {{ kind = "sliding_window", max_items = 50 }}
budget = {conversation_tokens}
"#
    )
}

#[test]
fn a_stage_that_can_reach_a_long_context_tier_gets_a_note_and_nothing_more() {
    let big = manifest_on("google", "gemini-2.5-pro", 300_000);
    let findings = lint(&big, &LintEnv::default_with_windows());
    let found = findings
        .iter()
        .find(|f| f.code == "long-context-price")
        .expect("the tier is noted");
    assert_eq!(found.severity, LintSeverity::Note);
    assert!(
        found.message.contains("google/gemini-2.5-pro"),
        "{}",
        found.message
    );
    assert!(found.message.contains("200000"), "{}", found.message);
    assert!(found.message.contains("301000"), "{}", found.message);
    assert!(
        found.fix.as_deref().unwrap_or_default().contains("200000"),
        "{found:?}"
    );

    let small = manifest_on("google", "gemini-2.5-pro", 100_000);
    assert!(
        !codes(&lint(&small, &LintEnv::default_with_windows())).contains(&"long-context-price")
    );

    // No tier in the price table, no window known, or no model priced: silent.
    let untiered = manifest_on("anthropic", "claude-sonnet-5", 900_000);
    assert!(
        !codes(&lint(&untiered, &LintEnv::default_with_windows())).contains(&"long-context-price")
    );
    assert!(!codes(&lint(&big, &LintEnv::default())).contains(&"long-context-price"));
    let unknown = manifest_on("google", "gemini-unlisted", 900_000);
    let mut env = LintEnv::default_with_windows();
    env.model_windows
        .insert(("google".into(), "gemini-unlisted".into()), 1_000_000);
    assert!(!codes(&lint(&unknown, &env)).contains(&"long-context-price"));
}

// ─── Graph helpers ────────────────────────────────────────────────────────────

/// A region named in both the graph's layout and a stage's is listed once,
/// from whichever layout names it first, and a seed only a stage's layout
/// declares is listed too.
#[test]
fn seeds_in_stage_layouts_are_listed_once_per_region() {
    let toml = manifest(
        r#"
[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
[graph.stages.layout]
total_budget_tokens = 0
[[graph.stages.layout.regions]]
name = "facts"
kind = "pinned"
budget = 1000
seed = { command = "ls stage" }
[[graph.stages.layout.regions]]
name = "env"
kind = "pinned"
budget = 1000
seed = { tools = { calls = [{ tool = "current_time", args = {} }] } }
"#,
    ) + "\n[[graph.layout.regions]]\nname = \"facts\"\nkind = \"pinned\"\nbudget = 1000\n\
         seed = { command = \"ls graph\" }\n";
    let findings = lint(&toml, &LintEnv::default());
    let commands = with_code(&findings, "command-seed");
    assert_eq!(commands.len(), 1, "{findings:?}");
    assert!(commands[0].message.contains("1 region(s)"), "{commands:?}");
    assert!(commands[0].message.contains("ls graph"), "{commands:?}");
    assert!(!commands[0].message.contains("ls stage"), "{commands:?}");
    let tools = with_code(&findings, "tool-seed");
    assert!(tools[0].message.contains("env: current_time"), "{tools:?}");
}

#[test]
fn every_group_has_its_token_and_all_covers_the_rest() {
    let tokens: Vec<&str> = [
        ToolGroup::All,
        ToolGroup::Builtin,
        ToolGroup::Subagent,
        ToolGroup::Scripts,
        ToolGroup::Mcp,
    ]
    .into_iter()
    .map(group_token)
    .collect();
    assert_eq!(
        tokens,
        ["@all", "@builtin", "@subagent", "@scripts", "@mcp"]
    );
    assert!(covers(ToolGroup::All, ToolGroup::Mcp));
    assert!(covers(ToolGroup::Mcp, ToolGroup::Mcp));
    assert!(!covers(ToolGroup::Mcp, ToolGroup::Builtin));
}

/// A stage's own `input_accepts` wins; without one it takes what the regions
/// it sees accept, a hidden region and text left out, and a region accepting
/// anything reported as `*/*`.
#[test]
fn a_stage_takes_what_its_visible_regions_accept() {
    let toml = manifest(
        r#"
[[graph.stages]]
name = "sees"
hide = ["hidden"]

[[graph.stages]]
name = "own"
input_accepts = ["audio/*"]
"#,
    ) + "\n[[graph.layout.regions]]\nname = \"pics\"\nkind = \"pinned\"\nbudget = 10\n\
         accepts = [\"image/*\", \"text/markdown\"]\n\
         \n[[graph.layout.regions]]\nname = \"hidden\"\nkind = \"pinned\"\nbudget = 10\n\
         accepts = [\"video/*\"]\n";
    let graph = graph_of(&toml);
    // The two fixture regions accept anything, so `*/*` comes first.
    assert_eq!(
        stage_inputs(&graph, &graph.stages[0]),
        ["*/*", "image/*"],
        "{toml}"
    );
    assert_eq!(stage_inputs(&graph, &graph.stages[1]), ["audio/*"]);
}

/// An input bound to something other than a region gives a work item nowhere
/// to land either.
#[test]
fn an_input_bound_only_to_a_stage_setting_does_not_hold_work_items() {
    let toml = manifest(
        r#"
[[graph.stages]]
name = "split"
mode = { fan_out = { worker = { stage = "work" } } }
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }

[[graph.stages]]
name = "work"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
allow_as_worker = true

[[graph.inputs]]
name = "rounds"
type = "int"
binds = [{ stage_max_iterations = "work" }]
"#,
    );
    assert_eq!(
        codes(&lint(&toml, &LintEnv::default())),
        ["fanout-worker-task-unheld"]
    );
}

/// A graph with edges and no stages has no entry to walk from, and two edges
/// joining the same pair of stages are judged once.
#[test]
fn the_graph_walk_needs_an_entry_and_judges_each_pair_once() {
    let empty = manifest("stages = []\n[[graph.edges]]\nname = \"x\"\nfrom = \"a\"\nto = \"b\"\n");
    assert!(lint_graph(&graph_of(&empty)).is_empty());

    let stage = |name: &str| {
        format!(
            "[[graph.stages]]\nname = \"{name}\"\n\
             model = {{ models = [{{ provider = \"anthropic\", model = \"claude-sonnet-5\" }}] }}\n\
             max_iterations = 5\n"
        )
    };
    let edge = |name: &str, from: &str, to: &str| {
        format!("[[graph.edges]]\nname = \"{name}\"\nfrom = \"{from}\"\nto = \"{to}\"\n")
    };
    let toml = manifest(&format!(
        "{}{}{}{}{}",
        stage("a"),
        stage("b"),
        edge("b", "a", "b"),
        edge("again", "a", "b"),
        edge("a", "b", "a"),
    ));
    let cycles = with_code(
        &lint(&toml, &LintEnv::default()),
        "cycle-without-max-revisits",
    )
    .len();
    assert_eq!(cycles, 2);
}

#[test]
fn a_policy_reads_as_a_blueprint_writes_it() {
    use leviath_core::policy::ToolPolicy;
    assert_eq!(policy_word(ToolPolicy::Allow), "allow");
    assert_eq!(policy_word(ToolPolicy::Ask), "ask");
    assert_eq!(policy_word(ToolPolicy::Deny), "deny");
}

/// A stage whose head model cannot be chosen at all here is left to the
/// spawn, which says why; the retention check has nothing to judge.
#[test]
fn a_stage_with_no_choosable_model_is_not_judged_for_retention() {
    let env = LintEnv::default().with_retention(
        &graph_of(&blueprint_pinning(&[("nobody-has-this", "m")])),
        &zero_retention_config(),
        &leviath_runtime::ProviderRegistry::new(),
    );
    assert!(env.retention_refusals.is_empty());
}

/// Which pinned providers this install can reach is asked of the registry
/// the config builds; one it does not have is left out.
#[test]
fn the_reachable_providers_come_from_the_config() {
    let env = LintEnv::default().with_providers(
        &graph_of(&blueprint_pinning(&[("nobody-has-this", "m")])),
        &crate::config::Config::default(),
    );
    assert_eq!(env.available_providers, Some(HashSet::new()));
}
