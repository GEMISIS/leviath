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

/// The bug this check exists for, found in a shipped blueprint: the stage
/// grants `shell` and the agent writes `bash = "ask"`. Policy is matched on the
/// name the model calls, so the entry is dead.
#[test]
fn a_permission_written_under_the_other_spelling_is_warned_about() {
    let toml = format!(
        "{}\n[tool_permissions]\nbash = \"ask\"\n",
        manifest(
            r#"
[stages.main]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 10
available_tools = ["shell"]
"#,
        )
    );
    let findings = lint(&toml, &LintEnv::default());
    assert_eq!(codes(&findings), ["permission-name-mismatch"]);
    assert!(
        findings[0].message.contains("grants 'shell'"),
        "{findings:?}"
    );
    assert!(findings[0].message.contains("'bash'"), "{findings:?}");
}

/// And the reverse: granted as `bash`, written as `shell`.
#[test]
fn the_mismatch_is_caught_in_either_direction() {
    let toml = format!(
        "{}\n[tool_permissions]\nshell = \"allow\"\n",
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
    assert_eq!(
        codes(&lint(&toml, &LintEnv::default())),
        ["permission-name-mismatch"]
    );
}

#[test]
fn alias_siblings_never_include_the_name_itself() {
    assert_eq!(alias_siblings("bash"), ["shell"]);
    assert_eq!(alias_siblings("shell"), ["bash"]);
    // A tool with no aliases has no siblings at all.
    assert!(alias_siblings("read_file").is_empty());
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

#[test]
fn a_model_present_in_the_catalog_passes() {
    let env = LintEnv {
        known_models: vec![("anthropic".to_string(), "claude-sonnet-5".to_string())],
        ..LintEnv::default()
    };
    assert!(lint(&manifest(CLEAN_STAGE), &env).is_empty());
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
        findings[0].message.contains("anthropic, openai"),
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
plain = { kind = "pinned", max_tokens = 1000 }
conversation = { kind = "sliding_window", max_items = 50, max_tokens = 10000 }
"#;
    let findings = lint(toml, &LintEnv::default());
    assert_eq!(codes(&findings), ["command-seed"]);
    let message = &findings[0].message;
    assert!(message.contains("2 region(s)"), "{message}");
    assert!(message.contains("facts: git ls-files"), "{message}");
    assert!(message.contains("tests: ls tests"), "{message}");
    // A region without a command seed is not named.
    assert!(!message.contains("plain"), "{message}");
    // The escape hatches are given, so the reader knows how to refuse.
    let fix = findings[0]
        .fix
        .as_deref()
        .expect("a seed note offers a fix");
    assert!(fix.contains("--no-seed-commands"), "{fix}");
    assert!(fix.contains("allow_seed_commands"), "{fix}");
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
entry = true
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
entry = true
[stages.split.transitions]

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
entry = true
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
entry = true
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
fn a_cycle_with_max_revisits_is_fine() {
    let toml = manifest(
        r#"
[stages.a]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
max_revisits = 2
entry = true
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
    assert!(lint(&toml, &LintEnv::default()).is_empty());
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
entry = true
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
    assert!(lint_manifest("", &bp, &LintEnv::default()).is_empty());
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
