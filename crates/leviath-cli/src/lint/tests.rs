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
/// a mismatch to warn about. It used to be: only the name the model calls was
/// looked up, so `bash = "ask"` against a stage granting `shell` was dead, and
/// this check told the author so. Policy resolution now accepts both, and a
/// warning here would send them to fix something that works.
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

// ─── read_paths grant status (issue #209) ────────────────────────────────────

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

/// A partial grant is the case the old spawn warning missed entirely: it only
/// fired when the grant list was empty.
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
fn a_capped_cycle_no_longer_trips_the_cycle_lint_but_can_dead_end() {
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
