//! `lev create` - Create a new agent blueprint

use clap::Args;
use std::fs;
use std::path::Path;

/// Arguments for `lev create`.
#[derive(Args)]
pub struct CreateArgs {
    /// Blueprint name
    #[arg(value_name = "NAME")]
    pub name: String,

    /// Starting template: `coder` for the multi-stage shape, anything else for a
    /// single-stage starting point
    #[arg(short, long, default_value = "default")]
    pub template: String,
}

/// Run `lev create`: scaffold a new agent from a template.
pub async fn execute(args: CreateArgs) -> anyhow::Result<()> {
    execute_with(args, &|path, contents| fs::write(path, contents))
}

/// Core of `execute()`, parameterized over the file-write primitive so tests
/// can force any individual write's error arm deterministically - without a
/// process-global umask mutation (which is rejected here, for good reason:
/// `cargo test`'s default thread-based parallelism means a restrictive umask
/// can't be scoped to one test the way an env var or CWD lock can, so ANY
/// other test creating a file/directory on another thread during that window
/// would silently get the same zero-permission treatment). Each real call site
/// still goes through the exact same
/// `std::fs::write` in production (`execute` above passes it directly, with
/// zero indirection cost); only tests substitute a fake.
fn execute_with(
    args: CreateArgs,
    write_file: &dyn Fn(&Path, &[u8]) -> std::io::Result<()>,
) -> anyhow::Result<()> {
    tracing::info!("Creating agent blueprint");

    let blueprint_dir = Path::new(&args.name);

    if blueprint_dir.exists() {
        anyhow::bail!("Directory '{}' already exists", args.name);
    }

    fs::create_dir_all(blueprint_dir)?;

    let manifest = create_manifest(&args.name, &args.template);
    write_file(&blueprint_dir.join("agent.leviath"), manifest.as_bytes())?;

    let gitignore_content = ".env\n*.leviath-bundle\n.leviath/\n";
    write_file(
        &blueprint_dir.join(".gitignore"),
        gitignore_content.as_bytes(),
    )?;

    let env_example_content = "# Copy this to .env and fill in your API key\n# ANTHROPIC_API_KEY=sk-ant-...\n# OPENAI_API_KEY=sk-...\n# OPENROUTER_API_KEY=sk-or-...\n";
    write_file(
        &blueprint_dir.join(".env.example"),
        env_example_content.as_bytes(),
    )?;

    println!("Created blueprint: {}", args.name);
    println!("\nNext steps:");
    println!("  cd {}", args.name);
    println!("  lev run . --task \"Your task here\"");
    println!(
        "  lev add . && lev run {} --task \"Your task here\"",
        args.name
    );

    Ok(())
}

/// Escapes a string for embedding inside a TOML basic (double-quoted)
/// string literal. Without this, a blueprint name containing a backslash
/// (e.g. a Windows path like `C:\Users\...\my-agent`, which `lev create`
/// accepts directly as the blueprint name/directory) breaks TOML parsing:
/// `\U` is interpreted as the start of an 8-digit-hex unicode escape, not a
/// literal backslash-U.
fn toml_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn create_manifest(name: &str, template: &str) -> String {
    let name = &toml_escape(name);
    match template {
        "coder" => format!(
            r#"[agent]
name = "{name}"
version = "0.1.0"
description = "A coding assistant blueprint"

# Global tool permissions: write/exec require approval unless overridden.
[tool_permissions]
read_file = "allow"
list_dir = "allow"
write_file = "ask"
edit_file = "ask"
bash = "ask"

[stages.analyze]
mode = "autonomous"
model = {{ provider = "anthropic", model = "claude-sonnet-4-6" }}
description = "Understand the task and plan the implementation"
available_tools = ["read_file", "list_dir"]
max_iterations = 15
system_prompt = """
Analyze the coding task in the `task` region and produce a concise implementation
plan: which files to create/modify, what each does, and the key decisions.
"""
# Large file reads persist in the `codebase` region (a short pointer stays in the
# conversation); action-tool results stay inline. Never route to a sliding_window
# other than `conversation`.
[stages.analyze.tool_routing]
default_region = "conversation"
[stages.analyze.tool_routing.overrides]
read_file = "codebase"
list_dir = "codebase"
[stages.analyze.transitions.implement]
transform = "direct"

[stages.implement]
mode = "autonomous"
model = {{ provider = "anthropic", model = "claude-sonnet-4-6" }}
description = "Write code according to the plan"
available_tools = ["write_file", "read_file", "edit_file", "list_dir", "bash"]
max_iterations = 50
system_prompt = """
Implement the plan. Create all necessary files, then use bash to run tests and
verify the build. Read existing code from the `codebase` region.
"""
[stages.implement.tool_routing]
default_region = "conversation"
[stages.implement.tool_routing.overrides]
read_file = "codebase"
list_dir = "codebase"

# Region budgets are percentages of the model's context window (ceilings, may sum
# past 100%); the absolute max_tokens is an optional guard-rail cap. Every
# blueprint needs an explicit `conversation` sliding_window - it holds the message
# stream and is carried across stage transitions.
[context.regions]
task         = {{ kind = "pinned",          budget = "2%",  max_tokens = 2000, required = true, seed = "task", required_message = "Describe the coding task via --task." }}
codebase     = {{ kind = "temporary",       budget = "20%", max_tokens = 30000 }}
conversation = {{ kind = "sliding_window",  max_items = 20, budget = "15%", max_tokens = 15000, strategy = "bulk", overflow = 10 }}
scratch      = {{ kind = "clearable",       budget = "8%",  max_tokens = 10000 }}
"#,
            name = name
        ),

        "researcher" => format!(
            r#"[agent]
name = "{name}"
version = "0.1.0"
description = "A research assistant blueprint"

[tool_permissions]
read_file = "allow"
list_dir = "allow"
bash = "ask"

[stages.gather]
mode = "autonomous"
model = {{ provider = "anthropic", model = "claude-sonnet-4-6" }}
description = "Gather relevant information"
available_tools = ["read_file", "list_dir", "bash"]
max_iterations = 20
system_prompt = """
Gather source material on the topic in the `query` region. Use read_file/list_dir
for local material and bash for anything else; raw content lands in `sources`.
Note where each item came from and the claims it supports.
"""
# (Tip: drop web_search.rhai / web_fetch.rhai into a `tools/` dir beside this file
# and add them to available_tools for real web research - see the researcher agent.)
[stages.gather.tool_routing]
default_region = "conversation"
[stages.gather.tool_routing.overrides]
read_file = "sources"
list_dir = "sources"
bash = "sources"
[stages.gather.transitions.synthesize]
transform = "compact"

[stages.synthesize]
mode = "interactive"
model = {{ provider = "anthropic", model = "claude-sonnet-4-6" }}
description = "Synthesize findings and discuss with user"
available_tools = ["read_file", "list_dir"]
max_iterations = 15
system_prompt = """
Synthesize the `sources` into `findings`: themes, agreements/disagreements, and
well-supported vs speculative claims. Cite specific sources.
"""

# Region budgets are percentages of the model's context window (ceilings, may sum
# past 100%); absolute max_tokens / threshold_tokens are guard-rail caps. A
# `compacting` region needs a paired `compact_history` region for its summaries.
[context.regions]
query           = {{ kind = "pinned",          budget = "2%",  max_tokens = 2000, required = true, seed = "task", required_message = "State the research question via --task." }}
sources         = {{ kind = "temporary",       budget = "25%", max_tokens = 40000 }}
findings        = {{ kind = "compacting",      budget = "12%", compact_at = "80%", threshold_tokens = 12000, max_tokens = 15000 }}
findings_history = {{ kind = "compact_history", source_region = "findings", budget = "3%", max_tokens = 6000 }}
conversation    = {{ kind = "sliding_window",  max_items = 15, budget = "12%", max_tokens = 12000, strategy = "bulk", overflow = 10 }}
scratch         = {{ kind = "clearable",       budget = "6%",  max_tokens = 8000 }}
"#,
            name = name
        ),

        _ => format!(
            r#"[agent]
name = "{name}"
version = "0.1.0"
description = "A simple agent blueprint"

[tool_permissions]
read_file = "allow"
list_dir = "allow"
write_file = "ask"
bash = "ask"

[stages.main]
mode = "autonomous"
model = {{ provider = "anthropic", model = "claude-sonnet-4-6" }}
description = "Main execution stage"
available_tools = ["read_file", "list_dir", "write_file", "bash"]
max_iterations = 30
system_prompt = """
You are a helpful agent. Complete the task described in the `task` region
thoroughly.
"""

# Region budgets are percentages of the model's context window (ceilings, may sum
# past 100%); the absolute max_tokens is an optional guard-rail cap. Every
# blueprint needs an explicit `conversation` sliding_window region.
[context.regions]
task         = {{ kind = "pinned",         budget = "2%",  max_tokens = 2000, required = true, seed = "task", required_message = "Describe the task via --task." }}
conversation = {{ kind = "sliding_window", max_items = 10, budget = "12%", max_tokens = 10000, strategy = "bulk", overflow = 10 }}
scratch      = {{ kind = "clearable",      budget = "6%",  max_tokens = 5000 }}
"#,
            name = name
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::with_tracing;

    #[test]
    fn default_template_is_valid_toml() {
        let manifest = create_manifest("test-agent", "default");
        let parsed: toml::Value = toml::from_str(&manifest).unwrap();
        let agent = parsed.get("agent").expect("should have [agent] section");
        assert_eq!(agent.get("name").unwrap().as_str().unwrap(), "test-agent");
        assert_eq!(agent.get("version").unwrap().as_str().unwrap(), "0.1.0");
    }

    #[test]
    fn name_with_windows_style_backslashes_produces_valid_toml() {
        // Regression test: `lev create` accepts a full path as the blueprint
        // name (used directly as the target directory), and on Windows that
        // path contains backslashes - e.g. `C:\Users\RUNNER~1\...\my-agent`.
        // Before escaping, `\U` in the raw TOML string was parsed as the
        // start of an (invalid) 8-digit-hex unicode escape, breaking every
        // template. Confirmed this exact failure on real Windows CI.
        let name = r"C:\Users\RUNNER~1\AppData\Local\Temp\.tmpmAlPt3\default-template-agent";
        for template in ["default", "coder", "researcher"] {
            let manifest = create_manifest(name, template);
            let parsed: toml::Value =
                toml::from_str(&manifest).expect("template produced invalid TOML");
            let agent = parsed.get("agent").unwrap();
            assert_eq!(agent.get("name").unwrap().as_str().unwrap(), name);
        }
    }

    #[test]
    fn name_with_embedded_quote_produces_valid_toml() {
        let name = r#"my"agent"#;
        let manifest = create_manifest(name, "default");
        let parsed: toml::Value = toml::from_str(&manifest).unwrap();
        let agent = parsed.get("agent").unwrap();
        assert_eq!(agent.get("name").unwrap().as_str().unwrap(), name);
    }

    #[test]
    fn coder_template_is_valid_toml() {
        let manifest = create_manifest("my-coder", "coder");
        let parsed: toml::Value = toml::from_str(&manifest).unwrap();
        let agent = parsed.get("agent").unwrap();
        assert_eq!(agent.get("name").unwrap().as_str().unwrap(), "my-coder");
        assert!(parsed.get("stages").is_some());
    }

    #[test]
    fn researcher_template_is_valid_toml() {
        let manifest = create_manifest("my-researcher", "researcher");
        let parsed: toml::Value = toml::from_str(&manifest).unwrap();
        let agent = parsed.get("agent").unwrap();
        assert_eq!(
            agent.get("name").unwrap().as_str().unwrap(),
            "my-researcher"
        );
    }

    #[test]
    fn templates_use_percentage_budgets_and_parse_via_manifest() {
        // Every generated template ships percentage budgets and must parse under
        // the real manifest parser (which validates `budget`/`compact_at`).
        for template in ["default", "coder", "researcher", "other"] {
            let manifest = create_manifest("pct-agent", template);
            assert!(
                manifest.contains("budget = \""),
                "{template} template should use percentage budgets"
            );
            let bp = leviath_core::manifest::parse_manifest(&manifest)
                .expect("generated template should parse");
            assert!(
                bp.context_layout.has_percent_budgets(),
                "{template} layout should have percentage budgets"
            );
        }
    }

    #[test]
    fn every_template_satisfies_context_layout_invariants() {
        use leviath_core::RegionKind;
        for template in ["default", "coder", "researcher", "other"] {
            let manifest = create_manifest("inv-agent", template);
            let bp = leviath_core::manifest::parse_manifest(&manifest).unwrap();
            let regions = &bp.context_layout.regions;

            // Explicit conversation sliding_window. (matches! is the FIRST operand
            // so it's evaluated for every region - non-sliding regions exercise its
            // false arm, the conversation region its true arm.)
            let has_conv_sliding = regions.iter().any(|r| {
                matches!(r.kind, RegionKind::SlidingWindow { .. }) && r.name == "conversation"
            });
            assert!(
                has_conv_sliding,
                "{template} template needs an explicit conversation sliding_window"
            );

            // No routing targets a non-conversation sliding_window.
            let sliding: std::collections::HashSet<&str> = regions
                .iter()
                .filter(|r| matches!(r.kind, RegionKind::SlidingWindow { .. }))
                .map(|r| r.name.as_str())
                .collect();
            for stage in &bp.stages {
                if let Some(routing) = &stage.tool_result_routing {
                    let mut targets = vec![routing.default_region.as_str()];
                    targets.extend(routing.tool_overrides.values().map(String::as_str));
                    for t in targets {
                        assert!(
                            t == "conversation" || !sliding.contains(t),
                            "{template} stage '{}' routes to non-conversation sliding_window '{t}'",
                            stage.name
                        );
                    }
                }
            }

            // Every compacting region has a compact_history pair.
            let hist: std::collections::HashSet<&str> = regions
                .iter()
                .filter_map(|r| match &r.kind {
                    RegionKind::CompactHistory { source_region } => Some(source_region.as_str()),
                    _ => None,
                })
                .collect();
            for r in regions {
                if matches!(r.kind, RegionKind::Compacting { .. }) {
                    assert!(
                        hist.contains(r.name.as_str()),
                        "{template} compacting region '{}' has no compact_history pair",
                        r.name
                    );
                }
            }
        }
    }

    #[test]
    fn unknown_template_falls_back_to_default() {
        let manifest = create_manifest("x", "nonexistent-template");
        let parsed: toml::Value = toml::from_str(&manifest).unwrap();
        let stages = parsed.get("stages").unwrap().as_table().unwrap();
        // Default template has a single "main" stage
        assert!(stages.contains_key("main"));
    }

    #[test]
    fn coder_template_has_analyze_and_implement_stages() {
        let manifest = create_manifest("x", "coder");
        let parsed: toml::Value = toml::from_str(&manifest).unwrap();
        let stages = parsed.get("stages").unwrap().as_table().unwrap();
        assert!(stages.contains_key("analyze"));
        assert!(stages.contains_key("implement"));
    }

    #[test]
    fn researcher_template_has_gather_and_synthesize_stages() {
        let manifest = create_manifest("x", "researcher");
        let parsed: toml::Value = toml::from_str(&manifest).unwrap();
        let stages = parsed.get("stages").unwrap().as_table().unwrap();
        assert!(stages.contains_key("gather"));
        assert!(stages.contains_key("synthesize"));
    }

    #[test]
    fn template_embeds_agent_name() {
        let manifest = create_manifest("special-name-123", "coder");
        assert!(manifest.contains("special-name-123"));
    }

    fn assert_has_context(template: &str, parsed: &toml::Value) {
        assert!(
            parsed.get("context").is_some(),
            "template '{}' missing [context]",
            template
        );
    }

    #[test]
    fn all_templates_have_context_regions() {
        for template in &["default", "coder", "researcher"] {
            let manifest = create_manifest("test", template);
            let parsed: toml::Value = toml::from_str(&manifest).unwrap();
            assert_has_context(template, &parsed);
        }
    }

    #[test]
    #[should_panic(expected = "template 'bogus' missing [context]")]
    fn all_templates_have_context_regions_panics_when_missing() {
        let parsed: toml::Value = toml::from_str("").unwrap();
        assert_has_context("bogus", &parsed);
    }

    // ─── execute ─────────────────────────────────────────────────────────
    //
    // `args.name` is used directly as a Path - passing an absolute tempdir
    // path makes this testable without touching the real CWD.

    #[tokio::test]
    async fn execute_creates_blueprint_dir_with_expected_files() {
        let dir = tempfile::tempdir().unwrap();
        let blueprint_path = dir.path().join("my-new-agent");
        let args = CreateArgs {
            name: blueprint_path.to_str().unwrap().to_string(),
            template: "coder".to_string(),
        };

        with_tracing(|| execute(args)).await.unwrap();

        assert!(blueprint_path.join("agent.leviath").exists());
        assert!(blueprint_path.join(".gitignore").exists());
        assert!(blueprint_path.join(".env.example").exists());

        let manifest = fs::read_to_string(blueprint_path.join("agent.leviath")).unwrap();
        assert!(manifest.contains("analyze"));
    }

    #[tokio::test]
    async fn execute_default_template_is_software_engineer_shape() {
        let dir = tempfile::tempdir().unwrap();
        let blueprint_path = dir.path().join("default-template-agent");
        let args = CreateArgs {
            name: blueprint_path.to_str().unwrap().to_string(),
            template: "default".to_string(),
        };

        with_tracing(|| execute(args)).await.unwrap();

        let manifest = fs::read_to_string(blueprint_path.join("agent.leviath")).unwrap();
        let parsed: toml::Value = toml::from_str(&manifest).unwrap();
        assert_eq!(
            parsed["agent"]["name"].as_str().unwrap(),
            blueprint_path.to_str().unwrap()
        );
    }

    #[tokio::test]
    async fn execute_existing_directory_errors() {
        let dir = tempfile::tempdir().unwrap();
        let blueprint_path = dir.path().join("already-exists");
        fs::create_dir_all(&blueprint_path).unwrap();

        let args = CreateArgs {
            name: blueprint_path.to_str().unwrap().to_string(),
            template: "coder".to_string(),
        };

        let err = with_tracing(|| execute(args)).await.unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[tokio::test]
    async fn execute_create_dir_all_fails_when_ancestor_is_a_file() {
        // `blueprint_dir.exists()` (the early bail check) returns `false` for
        // this path - `Path::exists()` can't stat through a non-directory
        // path component - so execution reaches `fs::create_dir_all(...)?`,
        // which then genuinely fails (ancestor isn't a directory).
        let dir = tempfile::tempdir().unwrap();
        let blocking_file = dir.path().join("not-a-directory");
        fs::write(&blocking_file, "x").unwrap();
        let blueprint_path = blocking_file.join("nested-blueprint");

        let args = CreateArgs {
            name: blueprint_path.to_str().unwrap().to_string(),
            template: "coder".to_string(),
        };

        let result = with_tracing(|| execute(args)).await;
        assert!(result.is_err());
    }

    // ─── execute_with: injected write-failure arms ─────────────────────────
    //
    // These exercise the 3 `write_file(...)?` error arms deterministically,
    // without any process-global umask mutation - each test injects a plain
    // local closure that fails for one specific target filename, leaving the
    // others to succeed exactly as production would.

    fn args_for(dir: &std::path::Path, name: &str) -> CreateArgs {
        CreateArgs {
            name: dir.join(name).to_str().unwrap().to_string(),
            template: "coder".to_string(),
        }
    }

    #[test]
    fn execute_with_agent_manifest_write_failure_propagates() {
        let dir = tempfile::tempdir().unwrap();
        let args = args_for(dir.path(), "manifest-write-fails");

        // `agent.leviath` is unconditionally the *first* write `execute_with`
        // attempts, so failing on every call (rather than branching on the
        // path) is sufficient here and avoids an else-arm that could never
        // actually run: the `?` on this first failure returns before any
        // other path is ever passed to this closure.
        let result = execute_with(args, &|_path, _contents| {
            Err(std::io::Error::other(
                "injected agent.leviath write failure",
            ))
        });

        let err = result.unwrap_err();
        assert!(
            err.to_string()
                .contains("injected agent.leviath write failure")
        );
    }

    #[test]
    fn execute_with_gitignore_write_failure_propagates() {
        let dir = tempfile::tempdir().unwrap();
        let args = args_for(dir.path(), "gitignore-write-fails");

        let result = execute_with(args, &|path, contents| {
            if path.file_name().and_then(|n| n.to_str()) == Some(".gitignore") {
                Err(std::io::Error::other("injected .gitignore write failure"))
            } else {
                fs::write(path, contents)
            }
        });

        let err = result.unwrap_err();
        assert!(
            err.to_string()
                .contains("injected .gitignore write failure")
        );
        // The manifest write before it genuinely happened.
        assert!(
            dir.path()
                .join("gitignore-write-fails")
                .join("agent.leviath")
                .exists()
        );
    }

    #[test]
    fn execute_with_env_example_write_failure_propagates() {
        let dir = tempfile::tempdir().unwrap();
        let args = args_for(dir.path(), "env-example-write-fails");

        let result = execute_with(args, &|path, contents| {
            if path.file_name().and_then(|n| n.to_str()) == Some(".env.example") {
                Err(std::io::Error::other("injected .env.example write failure"))
            } else {
                fs::write(path, contents)
            }
        });

        let err = result.unwrap_err();
        assert!(
            err.to_string()
                .contains("injected .env.example write failure")
        );
        // The two writes before it genuinely happened.
        let created = dir.path().join("env-example-write-fails");
        assert!(created.join("agent.leviath").exists());
        assert!(created.join(".gitignore").exists());
    }
}
