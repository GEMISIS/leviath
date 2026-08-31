use super::*;

/// Wrap `stages_toml` in the smallest manifest that parses and validates.
fn manifest(stages_toml: &str) -> String {
    format!(
        r#"
[agent]
name = "lint-fixture"
version = "0.1.0"
description = "a fixture"

{stages_toml}

[context.regions]
system = {{ kind = "pinned", max_tokens = 1000 }}
conversation = {{ kind = "sliding_window", max_items = 50, max_tokens = 10000 }}
"#
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

/// Lint a manifest whose text and blueprint come from the same source, which is
/// the only pairing production ever passes.
fn lint(content: &str, env: &LintEnv) -> Vec<LintFinding> {
    let bp = leviath_core::manifest::parse_manifest(content).expect("fixture parses");
    lint_manifest(content, &bp, env)
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

/// A stage that declares everything the linter looks for, so a test can add a
/// single defect and see only that.
const CLEAN_STAGE: &str = r#"
[stages.main]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
description = "Main"
max_iterations = 10
available_tools = ["read_file"]
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
[stages.main]
mode = "autonomous"
model = { models = [{ provider = "madeup", model = "no-such-model" }] }
max_iterations = 10
available_tools = ["raed_file"]
"#,
    );
    let findings = lint(&toml, &LintEnv::default());
    assert!(findings.is_empty(), "{:?}", codes(&findings));
}

// ─── Declaration checks ───────────────────────────────────────────────────────

#[test]
fn a_stage_with_no_mode_is_warned_about() {
    let toml = manifest(
        r#"
[stages.main]
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
"#,
    );
    let findings = lint(&toml, &LintEnv::default());
    assert_eq!(codes(&findings), ["stage-missing-mode"]);
    assert_eq!(findings[0].stage.as_deref(), Some("main"));
    assert_eq!(findings[0].severity, LintSeverity::Warning);
    assert!(findings[0].message.contains("autonomous"), "{findings:?}");
}

/// The defect from the report: no model block, so the engine invents one and
/// the stage silently runs on the user's default provider.
#[test]
fn a_stage_with_no_model_block_is_warned_about() {
    let toml = manifest(
        r#"
[stages.main]
mode = "autonomous"
max_iterations = 10
"#,
    );
    let findings = lint(&toml, &LintEnv::default());
    assert_eq!(codes(&findings), ["stage-missing-model"]);
    assert!(
        findings[0].message.contains("default_provider"),
        "{findings:?}"
    );
    let fix = findings[0].fix.as_deref().expect("the fix names the block");
    assert!(fix.contains("[stages.main]"), "{fix}");
}

/// The old single-model spelling (`provider`/`model` at the top of the table)
/// still counts as declaring one.
#[test]
fn the_legacy_inline_model_spelling_counts_as_declared() {
    let toml = manifest(
        r#"
[stages.main]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-5" }
max_iterations = 10
"#,
    );
    let findings = lint(&toml, &LintEnv::default());
    assert!(findings.is_empty(), "{:?}", codes(&findings));
}

#[test]
fn a_stage_with_no_max_iterations_is_warned_about() {
    let toml = manifest(
        r#"
[stages.main]
mode = "autonomous"
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
    let toml = manifest(
        r#"
[stages.split]
mode = "fan_out"
worker_stage = "work"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
[stages.split.transitions.recover]
condition = "error"

[stages.work]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
allow_as_worker = true

[stages.recover]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
"#,
    );
    let findings = lint(&toml, &LintEnv::default());
    assert!(findings.is_empty(), "{:?}", codes(&findings));
}

/// `fail_all` with nowhere to go means one flaky worker ends the run.
#[test]
fn a_fail_all_fan_out_without_an_escape_is_warned_about() {
    let toml = manifest(
        r#"
[stages.split]
mode = "fan_out"
worker_stage = "work"
on_worker_failure = "fail_all"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }

[stages.work]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
allow_as_worker = true
"#,
    );
    let findings = lint(&toml, &LintEnv::default());
    assert_eq!(codes(&findings), ["fanout-no-escape"]);
}

/// The default policy merges what succeeded, so there is nothing to escape from
/// and nothing to say.
#[test]
fn a_continuing_fan_out_needs_no_escape() {
    let toml = manifest(
        r#"
[stages.split]
mode = "fan_out"
worker_stage = "work"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }

[stages.work]
mode = "autonomous"
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
    let toml = manifest(
        r#"
[stages.split]
mode = "fan_out"
worker_stage = "work"
on_worker_failure = "fail_all"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
[stages.split.transitions.recover]
condition = "dead_end"

[stages.work]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
allow_as_worker = true

[stages.recover]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
"#,
    );
    let findings = lint(&toml, &LintEnv::default());
    assert!(findings.is_empty(), "{:?}", codes(&findings));
}

/// An agent-level `[model]` block reads like it sets the agent's model. Nothing
/// looks at it.
#[test]
fn an_agent_level_model_block_is_warned_about() {
    let toml = format!(
        "{}\n[model]\nprovider = \"anthropic\"\nmodel = \"claude-opus-5\"\n",
        manifest(CLEAN_STAGE)
    );
    let findings = lint(&toml, &LintEnv::default());
    assert_eq!(codes(&findings), ["agent-model-block-ignored"]);
    assert!(findings[0].stage.is_none(), "{findings:?}");
}

// ─── dead-end-possible ────────────────────────────────────────────────────────

/// A graph whose only way on is a stage with a spendable revisit budget.
fn strandable(extra_edge: &str) -> String {
    format!(
        r#"
[agent]
name = "strandable"
version = "0.1.0"
description = "a fixture"

[stages.work]
mode = "autonomous"
model = {{ models = [{{ provider = "anthropic", model = "claude-sonnet-5" }}] }}
description = "Work"
max_iterations = 10
available_tools = ["read_file"]
[stages.work.transitions.review]
transform = "direct"
{extra_edge}

[stages.review]
mode = "autonomous"
model = {{ models = [{{ provider = "anthropic", model = "claude-sonnet-5" }}] }}
description = "Review"
max_iterations = 10
max_revisits = 2
available_tools = ["read_file"]

[stages.answer]
mode = "output"
model = {{ models = [{{ provider = "anthropic", model = "claude-sonnet-5" }}] }}
description = "Answer"
max_iterations = 10

[context.regions]
system = {{ kind = "pinned", max_tokens = 1000 }}
conversation = {{ kind = "sliding_window", max_items = 50, max_tokens = 10000 }}
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
        "\n[stages.work.transitions.answer]\ncondition = \"dead_end\"\ntransform = \"direct\"\n",
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
        "\n[stages.work.transitions.answer]\ncondition = \"error\"\ntransform = \"direct\"\n",
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
        "\n[stages.work.transitions.answer]\ncondition = \"max_iterations\"\ntransform = \"direct\"\n",
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
[agent]
name = "strandable"
version = "0.1.0"
description = "a fixture"

[stages.work]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
description = "Work"
max_iterations = 10
available_tools = ["read_file"]
[stages.work.transitions.review]
transform = "direct"
[stages.work.transitions.fallback]
condition = "dead_end"
transform = "direct"

[stages.review]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
description = "Review"
max_iterations = 10
max_revisits = 2
available_tools = ["read_file"]

# The escape's own target can run out too, so following it only defers the
# strand rather than resolving it.
[stages.fallback]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
description = "Fallback"
max_iterations = 10
max_revisits = 1
available_tools = ["read_file"]

[context.regions]
system = { kind = "pinned", max_tokens = 1000 }
conversation = { kind = "sliding_window", max_items = 50, max_tokens = 10000 }
"#;
    assert!(codes(&lint(toml, &LintEnv::default())).contains(&"dead-end-possible"));
}

// ─── Seeds the parser threw away ──────────────────────────────────────────────

/// A seed table with no key the parser recognizes leaves the region empty.
///
/// This is what a one-character typo looks like, and it is what a "coder-shaped"
/// fixture in this repo shipped with for months: `caller_input` instead of
/// `caller`, silently seeding nothing.
#[test]
fn a_seed_table_with_no_recognized_key_is_warned_about() {
    let toml = format!(
        "{}\n[context.regions.notes]\nkind = \"pinned\"\nmax_tokens = 100\n\
         seed = {{ caller_input = \"task\" }}\n",
        manifest(CLEAN_STAGE)
    );
    let findings = lint(&toml, &LintEnv::default());
    assert_eq!(codes(&findings), ["region-seed-not-understood"]);
    assert!(findings[0].message.contains("notes"), "{findings:?}");
}

/// A seed that is neither a string nor a table goes the same way.
#[test]
fn a_seed_of_the_wrong_type_is_warned_about() {
    let toml = format!(
        "{}\n[context.regions.notes]\nkind = \"pinned\"\nmax_tokens = 100\nseed = 42\n",
        manifest(CLEAN_STAGE)
    );
    assert_eq!(
        codes(&lint(&toml, &LintEnv::default())),
        ["region-seed-not-understood"]
    );
}

/// Every recognized form stays silent, so the check cannot become noise.
#[test]
fn the_recognized_seed_forms_are_not_warned_about() {
    for seed in [
        r#""task""#,
        r#""input""#,
        r#"{ caller = "extra" }"#,
        r#"{ literal = "x" }"#,
        r#"{ files = ["a.txt"] }"#,
        r#"{ glob = "*.rs" }"#,
        r#"{ rhai = "\"x\"" }"#,
        r#"{ command = "git ls-files" }"#,
    ] {
        let toml = format!(
            "{}\n[context.regions.notes]\nkind = \"pinned\"\nmax_tokens = 100\nseed = {seed}\n",
            manifest(CLEAN_STAGE)
        );
        let codes = codes(&lint(&toml, &LintEnv::default()));
        assert!(
            !codes.contains(&"region-seed-not-understood"),
            "seed {seed} was reported as unreadable: {codes:?}"
        );
    }
}

/// A region with no `seed` key at all is not a dropped seed.
#[test]
fn a_region_declaring_no_seed_is_not_warned_about() {
    let findings = lint(&manifest(CLEAN_STAGE), &LintEnv::default());
    assert!(
        !codes(&findings).contains(&"region-seed-not-understood"),
        "{findings:?}"
    );
}

// ─── Declared: the fallbacks ──────────────────────────────────────────────────

/// Text the linter cannot re-read yields no declaration findings at all, rather
/// than a full set of false ones. Production never hits this (the caller only
/// lints a manifest that already parsed) so it is asserted directly.
#[test]
fn unreadable_text_reports_every_key_as_declared() {
    let declared = Declared::from_text("not valid toml [[[");
    assert!(!declared.agent_model_block);
    let keys = declared.stage("anything");
    assert!(keys.mode);
    assert!(keys.model);
}

/// Same for a stage the text has no entry for, which is how a blueprint built
/// in code rather than parsed would arrive.
#[test]
fn a_stage_absent_from_the_text_reports_as_declared() {
    let keys = Declared::from_text("[agent]\nname = \"x\"\n").stage("ghost");
    assert!(keys.mode);
    assert!(keys.model);
}

/// A manifest that parses to something other than a table (a bare TOML document
/// cannot, but the parse is fallible in principle) takes the same path.
#[test]
fn text_with_no_stages_table_has_no_stage_keys() {
    let declared = Declared::from_text("[agent]\nname = \"x\"\n");
    assert!(declared.stages.is_empty());
    assert!(!declared.opaque);
}

// ─── Tool checks ──────────────────────────────────────────────────────────────

#[test]
fn a_misspelled_tool_is_an_error() {
    let toml = manifest(
        r#"
[stages.main]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
available_tools = ["read_file", "raed_file"]
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
/// installed. That is not a property of the manifest, so it is never flagged.
#[test]
fn an_mcp_tool_name_is_never_unknown() {
    let toml = manifest(
        r#"
[stages.main]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
available_tools = ["github__create_issue"]
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
[stages.main]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
available_tools = ["read_file"]
available_connectors = ["github"]

[stages.main.tool_permissions]
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
[stages.main]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
available_tools = ["read_file"]

[stages.main.tool_permissions]
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
[stages.main]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
available_tools = ["read_file"]

[stages.main.tool_permissions]
read_file = "allow"
"#,
    );
    assert!(lint(&toml, &LintEnv::default()).is_empty());
}

// ─── Blocking tools ───────────────────────────────────────────────────────────

#[test]
fn an_autonomous_stage_granting_an_ask_tool_is_warned_about() {
    let toml = manifest(
        r#"
[stages.main]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
available_tools = ["ask_user_text"]
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
[stages.main]
mode = "autonomous"
model = {{ models = [{{ provider = "anthropic", model = "claude-sonnet-5" }}] }}
max_iterations = 10
available_tools = ["{tool}"]
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
[stages.main]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
available_tools = ["ask_user_text"]
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
[stages.main]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
available_tools = ["ask_user_text", "ask_user_confirm"]
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
[stages.main]
mode = "interactive"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
available_tools = ["ask_user_text"]
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
[stages.main]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
available_tools = ["bash"]
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
[stages.main]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
available_tools = ["shell"]
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
[stages.main]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
available_tools = ["bash"]

[stages.main.tool_permissions]
bash = "allow"
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
            "{}\n[tool_permissions]\n{written} = \"ask\"\n",
            manifest(&format!(
                r#"
[stages.main]
mode = "autonomous"
model = {{ models = [{{ provider = "anthropic", model = "claude-sonnet-5" }}] }}
max_iterations = 10
available_tools = ["{granted}"]
"#
            ))
        );
        let found = codes(&lint(&toml, &LintEnv::default()));
        assert!(found.is_empty(), "{granted}/{written}: {found:?}");
    }
}

#[test]
fn an_agent_level_shell_policy_settles_it() {
    let toml = format!(
        "{}\n[tool_permissions]\nbash = \"deny\"\n",
        manifest(
            r#"
[stages.main]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
available_tools = ["bash"]
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
[stages.main]
mode = "autonomous"
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
[stages.main]
mode = "autonomous"
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
[stages.main]
mode = "autonomous"
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
[stages.main]
mode = "autonomous"
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
[stages.main]
mode = "autonomous"
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
[stages.main]
mode = "autonomous"
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
[stages.main]
mode = "autonomous"
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
[stages.main]
mode = "autonomous"
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
[stages.main]
mode = "autonomous"
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
[stages.main]
mode = "autonomous"
model = { models = ["claude-sonnet-5", "gpt-5.5"] }
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
[stages.main]
mode = "autonomous"
model = { models = ["claude-sonnet-5", { provider = "openai", model = "gpt-5.5" }] }
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
[stages.main]
mode = "autonomous"
model = { models = ["gorq-turbo-9"] }
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
[stages.main]
mode = "autonomous"
model = { models = ["gorq-turbo-9", "qwen3.5:9b", { provider = "openai", model = "gpt-5.5" }] }
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
[stages.main]
mode = "autonomous"
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
[stages.main]
mode = "autonomous"
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
    let mut bp =
        leviath_core::manifest::parse_manifest(&manifest(CLEAN_STAGE)).expect("the fixture parses");
    bp.stages[0].model.models.clear();
    let env = LintEnv {
        available_providers: Some(HashSet::new()),
        ..LintEnv::default()
    };
    let findings = lint_manifest(&manifest(CLEAN_STAGE), &bp, &env);
    assert!(findings.is_empty(), "{:?}", codes(&findings));
}

// ─── held checkpoints ─────────────────────────────────────────────────────────

/// A checkpoint that holds under `--yolo` is the blueprint working as written,
/// so it is a note. It is worth saying because `--yolo` reads as "run without
/// me", and a run that stops anyway looks like a hang.
#[test]
fn a_checkpoint_that_holds_under_yolo_is_noted() {
    let toml = manifest(
        r#"
[stages.plan]
mode = "interactive_points"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
available_tools = ["read_file", "ask_user_text"]
required_tools = ["ask_user_text"]

[[stages.plan.interaction_points]]
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
[stages.main]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
available_tools = ["ask_user_text", "bash"]
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
    format!("{}\n[safe_commands]\n{body}\n", manifest(CLEAN_STAGE))
}

/// Declaring `[safe_commands]` is legitimate, so it is a note - but an author
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

/// Declaring `[read_paths]` is legitimate, so it is a note rather than a
/// warning: it must survive `--deny-warnings` on an otherwise good blueprint.
#[test]
fn read_path_declarations_are_noted_not_warned() {
    let toml = format!(
        "{}\n[read_paths]\nallow = [\"~/.leviath/runs\"]\n",
        manifest(CLEAN_STAGE)
    );
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
    let toml = format!(
        "{}\n[read_paths]\nallow = [{listed}]\n",
        manifest(CLEAN_STAGE)
    );
    let blueprint = leviath_core::manifest::parse_manifest(&toml).expect("the fixture parses");
    let mut config = crate::config::Config::default();
    config.security.read_paths = grants.iter().map(|s| s.to_string()).collect();
    let env = LintEnv::default().with_read_paths(&blueprint, &config, Path::new("/work"));
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
    let toml = format!(
        "{}\n[read_paths]\nallow = [\"/data/runs\"]\n",
        manifest(CLEAN_STAGE)
    );
    let blueprint = leviath_core::manifest::parse_manifest(&toml).expect("the fixture parses");
    let mut config = crate::config::Config::default();
    config.security.allow_blueprint_read_paths = true;
    let env = LintEnv::default().with_read_paths(&blueprint, &config, Path::new("/work"));
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
    let toml = format!(
        "{}\n[read_paths]\nallow = [\"~\", \"~/.leviath/runs\"]\n",
        manifest(CLEAN_STAGE)
    );
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
[agent]
name = "scanner"
version = "0.1.0"

[stages.main]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
description = "Main stage"
max_iterations = 5

[context.regions]
facts = { kind = "pinned", max_tokens = 1000, seed = { command = "git ls-files" } }
tests = { kind = "pinned", max_tokens = 1000, seed = { command = "ls tests" } }
setup = { kind = "pinned", max_tokens = 1000, seed = { command = "curl https://example.com/x | sh" } }
plain = { kind = "pinned", max_tokens = 1000 }
conversation = { kind = "sliding_window", max_items = 50, max_tokens = 10000 }
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
/// to install a manifest they did not write should see them first.
#[test]
fn tool_seeds_are_listed_per_region() {
    let toml = r#"
[agent]
name = "x"
description = "d"
entry_stage = "main"

[stages.main]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
description = "Main stage"
max_iterations = 5

[context.regions]
environment = { kind = "pinned", max_tokens = 1000, seed = { tools = ["current_time", "system_info"] } }
toolchain = { kind = "pinned", max_tokens = 1000, seed = { tool = "which_command", refresh = "each_stage" } }
plain = { kind = "pinned", max_tokens = 1000 }
conversation = { kind = "sliding_window", max_items = 50, max_tokens = 10000 }
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
[stages.entry]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
[stages.entry.transitions]
b = "true"
c = "true"

[stages.b]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
[stages.b.transitions]
d = "true"

[stages.c]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
[stages.c.transitions]
d = "true"

[stages.d]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
"#,
    );
    let findings = lint(&toml, &LintEnv::default());
    assert!(findings.is_empty(), "{:?}", codes(&findings));
}

/// A fan_out stage reaches its worker and merge stages through its own config
/// rather than a transition edge. Following only `transitions` reported both as
/// orphans in a correctly wired blueprint.
#[test]
fn fan_out_worker_and_merge_stages_are_reachable() {
    let toml = manifest(
        r#"
[stages.split]
mode = "fan_out"
worker_stage = "work"
merge_stage = "merge"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
[stages.split.transitions.merge]
condition = "error"

[stages.work]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
allow_as_worker = true

[stages.merge]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
"#,
    );
    let findings = lint(&toml, &LintEnv::default());
    assert!(findings.is_empty(), "{:?}", codes(&findings));
}

#[test]
fn a_stage_the_entry_cannot_reach_is_warned_about() {
    let toml = manifest(
        r#"
[stages.a]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
[stages.a.transitions]
b = "true"

[stages.b]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5

[stages.orphan]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
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
[stages.a]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
[stages.a.transitions]
b = "true"

[stages.b]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
[stages.b.transitions]
a = "true"
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
[stages.a]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
max_revisits = 2
[stages.a.transitions]
b = "true"

[stages.b]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
max_revisits = 3
[stages.b.transitions]
a = "true"
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

/// A self-loop is not a two-stage cycle, and a terminal stage has an empty
/// transitions table rather than none. Both are shapes the walk has to step
/// over without complaining.
#[test]
fn self_loops_and_terminal_stages_are_not_cycles() {
    let toml = manifest(
        r#"
[stages.a]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
[stages.a.transitions]
a = "true"
b = "true"

[stages.b]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
[stages.b.transitions]
"#,
    );
    assert!(lint(&toml, &LintEnv::default()).is_empty());
}

/// A blueprint with no transitions anywhere is linear: there is no graph to
/// walk, so the graph checks return before doing anything.
#[test]
fn a_linear_blueprint_has_no_graph_findings() {
    let toml = manifest(
        r#"
[stages.a]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5

[stages.b]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
"#,
    );
    assert!(lint(&toml, &LintEnv::default()).is_empty());
}

/// `Blueprint::validate` rejects an entry_stage or transition target that names
/// no real stage, so the graph walk only meets those through the struct's own
/// public fields. It has to step over them rather than panic.
#[test]
fn the_graph_walk_steps_over_names_that_are_not_stages() {
    use leviath_core::{Blueprint, ContextLayout, Stage, TransitionEdge};

    let model = leviath_core::blueprint::ModelConfig::new(
        "anthropic".to_string(),
        "claude-sonnet-5".to_string(),
    );

    // entry_stage points at a name with no Stage: the BFS pops "ghost" and
    // finds nothing to walk from.
    let mut only = Stage::new("a".to_string(), model.clone());
    only.transitions = Some(HashMap::new());
    only.max_iterations = Some(5);
    let mut bp = Blueprint::new(
        "t".to_string(),
        "t".to_string(),
        vec![only],
        ContextLayout::new(Vec::new(), 1000),
    );
    bp.entry_stage = Some("ghost".to_string());
    // "a" is now unreachable, which is the only thing worth saying.
    assert_eq!(
        codes(&lint_manifest("", &bp, &LintEnv::default())),
        ["unreachable-stage"]
    );

    // A transition target with no Stage: the cycle check finds nothing to ask
    // about the other end of the edge.
    let mut dangling = Stage::new("a".to_string(), model);
    dangling.max_iterations = Some(5);
    dangling.transitions = Some(HashMap::from([(
        "ghost".to_string(),
        TransitionEdge {
            target: "ghost".to_string(),
            condition: Default::default(),
            hint: None,
            transform: Default::default(),
            gate: None,
            stuck: None,
        },
    )]));
    let bp = Blueprint::new(
        "t".to_string(),
        "t".to_string(),
        vec![dangling],
        ContextLayout::new(Vec::new(), 1000),
    );
    // The cycle walk has nothing to say, but an edge nothing can ever follow
    // is a guaranteed strand, which the dead-end lint reports.
    assert_eq!(
        codes(&lint_manifest("", &bp, &LintEnv::default())),
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

/// The reported blueprint, reconstructed: no mode, no model block, no iteration
/// cap, an unattended stage that can ask a human, a shell grant with no policy,
/// and a typo. Every finding lands, and only the typo is fatal.
#[test]
fn a_thoroughly_broken_stage_reports_each_defect_once() {
    let toml = manifest(
        r#"
[stages.scope]
available_tools = ["read_file", "bash", "ask_user_text", "raed_file"]
"#,
    );
    let env = LintEnv {
        known_tools: known_tools(&["read_file", "bash", "shell", "ask_user_text"]),
        ..LintEnv::default()
    };
    let findings = lint(&toml, &env);
    for code in [
        "stage-missing-mode",
        "stage-missing-model",
        "stage-missing-max-iterations",
        "unknown-tool",
        "blocking-tool-in-autonomous-stage",
        "implicit-shell-policy",
    ] {
        assert_eq!(with_code(&findings, code).len(), 1, "{code}: {findings:?}");
    }
    assert_eq!(findings.iter().filter(|f| f.is_error()).count(), 1);
    assert_eq!(findings.len(), 6, "{:?}", codes(&findings));
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
[stages.main]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
description = "Main"
max_iterations = 10
available_tools = ["read_file"]

[stages.main.transitions.summary]
hint = "done"

[stages.summary]
mode = "output"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
description = "Report"
max_iterations = 4

[stages.summary.transitions]
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
    let toml = REACHABLE_OUTPUT.replace("[stages.main.transitions.summary]\nhint = \"done\"\n", "");
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
        "available_tools = [\"read_file\"]\n",
        "available_tools = [\"read_file\"]\nallow_complete = true\n",
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
        "available_tools = [\"read_file\"]\n",
        "available_tools = [\"read_file\"]\nallow_complete = true\n",
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
        "description = \"Report\"\navailable_tools = [\"write_file\"]",
    );
    let findings = lint(&manifest(&toml), &output_env());
    let modifies = with_code(&findings, "output-stage-can-modify");
    assert_eq!(modifies.len(), 1, "{:?}", codes(&findings));
    assert_eq!(modifies[0].stage.as_deref(), Some("summary"));
}

/// A declared shape nobody is obliged to produce is a wish, not a contract.
#[test]
fn a_declared_shape_without_require_output_is_flagged() {
    let toml = CLEAN_STAGE.to_string() + "\n[stages.main.output]\nformat = \"a2ui\"\n";
    let findings = lint(&manifest(&toml), &output_env());
    let unrequired = with_code(&findings, "output-shape-not-required");
    assert_eq!(unrequired.len(), 1, "{:?}", codes(&findings));
}

/// The same shape on a stage that must submit is exactly right.
#[test]
fn a_declared_shape_on_a_requiring_stage_reports_nothing() {
    let toml = REACHABLE_OUTPUT.to_string()
        + "\n[stages.summary.output]\nformat = \"a2ui\"\ninstructions = \"one card per finding\"\n";
    let findings = lint(&manifest(&toml), &output_env());
    assert!(findings.is_empty(), "{:?}", codes(&findings));
}

/// `require_output` by hand, without the tool. The manifest parser's hard error
/// normally catches this first; the check exists so `lev validate` still says
/// something useful if a blueprint reaches it another way.
#[test]
fn requiring_an_output_without_the_submit_tool_is_an_error() {
    let mut stage = leviath_core::Stage::new(
        "summary".to_string(),
        leviath_core::blueprint::ModelConfig::new("anthropic".to_string(), "m".to_string()),
    );
    stage.available_tools = vec!["read_file".to_string()];
    stage.require_output = true;
    let findings = lint_output_stage(&stage);
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
[stages.verify]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
[stages.verify.transitions.answer]
transform = "compact"

[stages.answer]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
"#,
    )
    .replace(
        "conversation = { kind = \"sliding_window\", max_items = 50, max_tokens = 10000 }",
        "conversation = { kind = \"sliding_window\", max_items = 50, max_tokens = 10000 }\n\
         results = { kind = \"sliding_window\", max_items = 20, max_tokens = 8000, required = true }",
    );
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
[stages.verify]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
[stages.verify.transitions.answer]
transform = "compact"

[stages.answer]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
"#,
    )
    .replace(
        "conversation = { kind = \"sliding_window\", max_items = 50, max_tokens = 10000 }",
        "conversation = { kind = \"sliding_window\", max_items = 50, max_tokens = 10000 }\n\
         results = { kind = \"sliding_window\", max_items = 20, max_tokens = 8000, required = true, summarizable = false }",
    );
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
[stages.verify]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
[stages.verify.transitions.answer]
transform = "compact"

[stages.answer]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
"#,
    )
    .replace(
        "system = { kind = \"pinned\", max_tokens = 1000 }",
        "system = { kind = \"pinned\", max_tokens = 1000, required = true }",
    );
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
[stages.verify]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
[stages.verify.transitions.answer]
transform = "direct"

[stages.answer]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
"#,
    )
    .replace(
        "conversation = { kind = \"sliding_window\", max_items = 50, max_tokens = 10000 }",
        "conversation = { kind = \"sliding_window\", max_items = 50, max_tokens = 10000 }\n\
         results = { kind = \"sliding_window\", max_items = 20, max_tokens = 8000, required = true }",
    );
    let findings = lint(&toml, &LintEnv::default());
    assert!(
        with_code(&findings, "compact-summarizes-deliverable").is_empty(),
        "{:?}",
        codes(&findings)
    );
}

/// A manifest whose region layout is spelled out, so a budget can be varied.
fn manifest_with_regions(regions: &str) -> String {
    format!(
        r#"
[agent]
name = "lint-fixture"
version = "0.1.0"
description = "a fixture"

[stages.work]
mode = "autonomous"
model = {{ models = [{{ provider = "anthropic", model = "claude-sonnet-5" }}] }}
max_iterations = 10
allow_complete = true

[context.regions]
system = {{ kind = "pinned", max_tokens = 1000 }}
conversation = {{ kind = "sliding_window", max_items = 50, max_tokens = 10000 }}
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
    let toml = manifest_with_regions(r#"raw_findings = { kind = "temporary", budget = "38%" }"#);
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
            .contains("max_tokens"),
        "{found:?}"
    );
}

#[test]
fn a_percentage_budget_with_a_max_guard_is_left_alone() {
    // The fix the issue reports as working completely.
    let toml = manifest_with_regions(
        r#"raw_findings = { kind = "temporary", budget = "38%", max_tokens = 24000 }"#,
    );
    assert!(
        !codes(&lint(&toml, &LintEnv::default_with_windows()))
            .contains(&"unbounded-percentage-budget")
    );
}

#[test]
fn an_absolute_budget_is_never_warned_about() {
    let toml =
        manifest_with_regions(r#"raw_findings = { kind = "temporary", max_tokens = 24000 }"#);
    assert!(
        !codes(&lint(&toml, &LintEnv::default_with_windows()))
            .contains(&"unbounded-percentage-budget")
    );
}

/// A region that holds what it is given has no bound to fail to reach, so a
/// percentage there means exactly what its author intended.
#[test]
fn a_pinned_region_with_a_percentage_budget_is_fine() {
    let toml = manifest_with_regions(r#"notes = { kind = "pinned", budget = "38%" }"#);
    assert!(
        !codes(&lint(&toml, &LintEnv::default_with_windows()))
            .contains(&"unbounded-percentage-budget")
    );
}

#[test]
fn every_evicting_kind_is_covered_not_just_temporary() {
    for decl in [
        r#"r = { kind = "clearable", budget = "38%" }"#,
        r#"r = { kind = "sliding_window", max_items = 20, budget = "38%" }"#,
        r#"r = { kind = "compacting", compact_at = 0.8, budget = "38%" }"#,
    ] {
        let toml = manifest_with_regions(decl);
        assert!(
            codes(&lint(&toml, &LintEnv::default_with_windows()))
                .contains(&"unbounded-percentage-budget"),
            "{decl}"
        );
    }
}

/// Without a window there is no number to report, and a warning that cannot say
/// what "38%" comes to is one nobody acts on.
#[test]
fn nothing_is_said_when_no_declared_model_has_a_known_window() {
    let toml = manifest_with_regions(r#"raw_findings = { kind = "temporary", budget = "38%" }"#);
    assert!(!codes(&lint(&toml, &LintEnv::default())).contains(&"unbounded-percentage-budget"));
}

/// One warning per region, not per layout that declares it: the fix is on the
/// declaration.
#[test]
fn a_region_declared_in_two_layouts_is_named_once() {
    let toml = r#"
[agent]
name = "lint-fixture"
version = "0.1.0"
description = "a fixture"

[stages.work]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
allow_complete = true

[stages.work.context.regions]
raw_findings = { kind = "temporary", budget = "38%" }

[context.regions]
system = { kind = "pinned", max_tokens = 1000 }
raw_findings = { kind = "temporary", budget = "38%" }
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
[agent]
name = "router"
version = "0.1.0"
entry_stage = "gather"

[stages.gather]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-5" }
max_iterations = 5
available_tools = ["read_file", "web_fetch"]
system_prompt = "gather"

[stages.gather.tool_routing]
default_region = "raw_findings"

[context.regions]
raw_findings = { kind = "temporary", budget = "30%" }
conversation = { kind = "sliding_window", max_items = 20, budget = "12%" }
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
[agent]
name = "router"
version = "0.1.0"
entry_stage = "gather"

[stages.gather]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-5" }
max_iterations = 5
available_tools = ["read_file", "context_read"]
system_prompt = "gather"

[stages.gather.tool_routing]
default_region = "raw_findings"

[context.regions]
raw_findings = { kind = "temporary", budget = "30%" }
conversation = { kind = "sliding_window", max_items = 20, budget = "12%" }
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
        r#"sources_index = { kind = "pinned", max_tokens = 2000, required = true }"#,
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
        r#"sources_index = { kind = "pinned", max_tokens = 2000, required = true }"#,
    )
    .replace(
        "allow_complete = true",
        "allow_complete = true\navailable_tools = [\"context_append\"]",
    );
    let findings = lint(&toml, &LintEnv::default());
    assert!(
        with_code(&findings, "required-region-unenforceable").is_empty(),
        "{:?}",
        codes(&findings)
    );
}

/// Caller-seeded regions are exempt, the same exemption the runtime gate makes:
/// the caller owns them and they are validated at spawn, so no stage ever owed
/// one. Without this every bundled agent's `query` region would warn.
#[test]
fn a_required_caller_seeded_region_is_not_warned_about() {
    let toml = manifest_with_regions(
        r#"query = { kind = "pinned", max_tokens = 2000, required = true, seed = "task" }"#,
    );
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
        r#"sources_index = { kind = "pinned", max_tokens = 2000, required = true }"#,
    )
    .replace(
        "[context.regions]",
        "[stages.second]\nmode = \"autonomous\"\n\
         model = { models = [{ provider = \"anthropic\", model = \"claude-sonnet-5\" }] }\n\
         max_iterations = 10\nallow_complete = true\n\n[context.regions]",
    );
    let hits = codes(&lint(&toml, &LintEnv::default()))
        .into_iter()
        .filter(|c| *c == "required-region-unenforceable")
        .count();
    assert_eq!(hits, 1);
}

// ─── with_provider_catalogs ───────────────────────────────────────────────────

/// A blueprint pinning `<provider>/<model>` on its one stage, for the builder
/// tests below.
fn blueprint_pinning(pairs: &[(&str, &str)]) -> leviath_core::Blueprint {
    let listed = pairs
        .iter()
        .map(|(p, m)| format!("{{ provider = \"{p}\", model = \"{m}\" }}"))
        .collect::<Vec<_>>()
        .join(", ");
    let toml = manifest(&format!(
        "[stages.main]\nmode = \"autonomous\"\n\
         model = {{ models = [{listed}] }}\nmax_iterations = 10\n"
    ));
    leviath_core::manifest::parse_manifest(&toml).expect("the fixture parses")
}

/// A blueprint naming models and leaving every route open.
fn blueprint_open(models: &[&str]) -> leviath_core::Blueprint {
    let listed = models
        .iter()
        .map(|m| format!("\"{m}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let toml = manifest(&format!(
        "[stages.main]\nmode = \"autonomous\"\n\
         model = {{ models = [{listed}] }}\nmax_iterations = 10\n"
    ));
    leviath_core::manifest::parse_manifest(&toml).expect("the fixture parses")
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
        &bp,
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
        lint_manifest("", &bp, &env)
            .iter()
            .any(|f| f.code == "unserved-model"),
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
        &bp,
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
    let message = &lint_manifest("", &bp, &env)
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
        &bp,
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
        &bp,
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
        &bp,
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
        &bp,
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

    let env = LintEnv::default().with_provider_catalogs(&bp, &config, &registry);

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
        &bp,
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
        &bp,
        &crate::config::Config::default(),
        &registry,
    );

    assert!(
        env.provider_catalogs.is_empty(),
        "a native provider that publishes nothing is not `catalog-unchecked`"
    );
}
