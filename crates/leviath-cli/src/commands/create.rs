//! `lev create` - Create a new agent blueprint

use clap::Args;
use std::fs;
use std::path::Path;

#[derive(Args)]
pub struct CreateArgs {
    /// Blueprint name
    #[arg(value_name = "NAME")]
    pub name: String,

    /// Starting template (software-engineer, coder, researcher)
    #[arg(short, long, default_value = "software-engineer")]
    pub template: String,
}

pub async fn execute(args: CreateArgs) -> anyhow::Result<()> {
    tracing::info!("Creating agent blueprint");

    let blueprint_dir = Path::new(&args.name);

    if blueprint_dir.exists() {
        anyhow::bail!("Directory '{}' already exists", args.name);
    }

    fs::create_dir_all(blueprint_dir)?;

    let manifest = create_manifest(&args.name, &args.template);
    fs::write(blueprint_dir.join("agent.leviath"), manifest)?;

    fs::write(
        blueprint_dir.join(".gitignore"),
        ".env\n*.leviath-bundle\n.leviath/\n",
    )?;

    fs::write(
        blueprint_dir.join(".env.example"),
        "# Copy this to .env and fill in your API key\n# ANTHROPIC_API_KEY=sk-ant-...\n# OPENAI_API_KEY=sk-...\n# OPENROUTER_API_KEY=sk-or-...\n",
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

[stages.analyze]
mode = "autonomous"
model = {{ provider = "anthropic", model = "claude-sonnet-4-6" }}
description = "Understand the task and plan the implementation"
available_tools = ["read_file", "list_dir"]
max_iterations = 15
system_prompt = """
Analyze the coding task and produce a concise implementation plan.
"""

[stages.implement]
mode = "autonomous"
model = {{ provider = "anthropic", model = "claude-sonnet-4-6" }}
description = "Write code according to the plan"
available_tools = ["write_file", "read_file", "edit_file", "list_dir", "bash"]
max_iterations = 50
system_prompt = """
Implement the plan. Create all necessary files and verify with bash.
"""

[context.regions]
task         = {{ kind = "pinned",          max_tokens = 2000 }}
codebase     = {{ kind = "temporary",       max_tokens = 30000 }}
conversation = {{ kind = "sliding_window",  max_items = 20, max_tokens = 15000 }}
scratch      = {{ kind = "clearable",       max_tokens = 10000 }}
"#,
            name = name
        ),

        "researcher" => format!(
            r#"[agent]
name = "{name}"
version = "0.1.0"
description = "A research assistant blueprint"

[stages.gather]
mode = "autonomous"
model = {{ provider = "anthropic", model = "claude-sonnet-4-6" }}
description = "Gather relevant information"
available_tools = ["read_file", "list_dir", "bash"]
max_iterations = 20

[stages.synthesize]
mode = "interactive"
model = {{ provider = "anthropic", model = "claude-sonnet-4-6" }}
description = "Synthesize findings and discuss with user"
available_tools = ["read_file", "list_dir"]
max_iterations = 15

[context.regions]
objective    = {{ kind = "pinned",          max_tokens = 2000 }}
sources      = {{ kind = "temporary",       max_tokens = 40000 }}
findings     = {{ kind = "compacting",      threshold_tokens = 8000, max_tokens = 15000 }}
conversation = {{ kind = "sliding_window",  max_items = 15, max_tokens = 12000 }}
scratch      = {{ kind = "clearable",       max_tokens = 8000 }}
"#,
            name = name
        ),

        _ => format!(
            r#"[agent]
name = "{name}"
version = "0.1.0"
description = "A simple agent blueprint"

[stages.main]
mode = "autonomous"
model = {{ provider = "anthropic", model = "claude-sonnet-4-6" }}
description = "Main execution stage"
available_tools = ["read_file", "list_dir", "write_file", "bash"]
max_iterations = 30
system_prompt = """
You are a helpful agent. Complete the task thoroughly.
"""

[context.regions]
system       = {{ kind = "pinned",         max_tokens = 2000 }}
conversation = {{ kind = "sliding_window", max_items = 10, max_tokens = 10000 }}
scratch      = {{ kind = "clearable",      max_tokens = 5000 }}
"#,
            name = name
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal no-op `Subscriber` that reports every callsite as enabled.
    ///
    /// Without an active subscriber, `tracing::warn!`/`info!`/`debug!` calls
    /// short-circuit their field-argument evaluation before ever reaching it
    /// (no subscriber means the "is this level enabled" check fails first) --
    /// so a multi-line `tracing::warn!(...)` call's field-list lines show as
    /// uncovered by `cargo llvm-cov` even when the surrounding branch
    /// genuinely executes and is asserted on. `tracing_subscriber::fmt()`'s
    /// default builder was tried first and did *not* fix this (its default
    /// filtering still suppressed these callsites); this bare `Subscriber`
    /// impl is the proven-working pattern (see `leviath-runtime/src/systems.rs`).
    struct AlwaysOnSubscriber;

    impl tracing::Subscriber for AlwaysOnSubscriber {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn register_callsite(
            &self,
            _metadata: &'static tracing::Metadata<'static>,
        ) -> tracing::subscriber::Interest {
            tracing::subscriber::Interest::always()
        }
        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
        fn event(&self, _event: &tracing::Event<'_>) {}
        fn enter(&self, _span: &tracing::span::Id) {}
        fn exit(&self, _span: &tracing::span::Id) {}
        fn max_level_hint(&self) -> Option<tracing::metadata::LevelFilter> {
            Some(tracing::metadata::LevelFilter::TRACE)
        }
    }

    fn with_tracing<T>(f: impl FnOnce() -> T) -> T {
        static INSTALLED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        INSTALLED.get_or_init(|| {
            // set_global_default registers AlwaysOnSubscriber in LOCKED_DISPATCHERS
            // (the global dispatcher registry). rebuild_interest_cache then re-evaluates
            // every callsite against the global subscriber, setting interest to "always".
            // Without this, tracing macro inner blocks are unreachable in tests because
            // with_default (thread-local) is NOT consulted during callsite registration,
            // leaving every callsite cached as interest=never (no global dispatcher).
            let _ = tracing::subscriber::set_global_default(AlwaysOnSubscriber);
            tracing::callsite::rebuild_interest_cache();
        });
        f()
    }

    #[test]
    fn always_on_subscriber_span_methods_are_all_no_ops() {
        // This file only ever uses `tracing::info!` event macros (no field
        // list, no `tracing::span!`), so the span-related trait methods above
        // are otherwise dead code from `with_tracing`'s callers. Exercise
        // them directly via a real span so they're not left uncovered
        // themselves.
        with_tracing(|| {
            let span = tracing::info_span!("test-span", field = tracing::field::Empty);
            span.record("field", 1);
            let other = tracing::info_span!("other-span");
            span.follows_from(&other);
            let _enter = span.enter();
            tracing::info!(parent: &span, "inside span");
        });
    }

    #[test]
    fn default_template_is_valid_toml() {
        let manifest = create_manifest("test-agent", "software-engineer");
        let parsed: toml::Value = toml::from_str(&manifest).unwrap();
        let agent = parsed.get("agent").expect("should have [agent] section");
        assert_eq!(agent.get("name").unwrap().as_str().unwrap(), "test-agent");
        assert_eq!(agent.get("version").unwrap().as_str().unwrap(), "0.1.0");
    }

    #[test]
    fn name_with_windows_style_backslashes_produces_valid_toml() {
        // Regression test: `lev create` accepts a full path as the blueprint
        // name (used directly as the target directory), and on Windows that
        // path contains backslashes -- e.g. `C:\Users\RUNNER~1\...\my-agent`.
        // Before escaping, `\U` in the raw TOML string was parsed as the
        // start of an (invalid) 8-digit-hex unicode escape, breaking every
        // template. Confirmed this exact failure on real Windows CI.
        let name = r"C:\Users\RUNNER~1\AppData\Local\Temp\.tmpmAlPt3\default-template-agent";
        for template in ["software-engineer", "coder", "researcher"] {
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
        let manifest = create_manifest(name, "software-engineer");
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

    #[test]
    fn all_templates_have_context_regions() {
        for template in &["software-engineer", "coder", "researcher"] {
            let manifest = create_manifest("test", template);
            let parsed: toml::Value = toml::from_str(&manifest).unwrap();
            assert!(
                parsed.get("context").is_some(),
                "template '{}' missing [context]",
                template
            );
        }
    }

    // ─── execute ─────────────────────────────────────────────────────────
    //
    // `args.name` is used directly as a Path — passing an absolute tempdir
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
            template: "software-engineer".to_string(),
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
}
