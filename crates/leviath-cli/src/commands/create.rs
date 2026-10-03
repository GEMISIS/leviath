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
pub(crate) async fn execute(args: CreateArgs) -> anyhow::Result<()> {
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
    write_file(
        &blueprint_dir.join(leviath_blueprint::FILE_NAME),
        manifest.as_bytes(),
    )?;

    let gitignore_content = ".env\n*.leviath-bundle\n.leviath/\n";
    write_file(
        &blueprint_dir.join(".gitignore"),
        gitignore_content.as_bytes(),
    )?;

    let env_example_content = "# Copy this to .env and fill in your API key.\n# Leviath reads it only with load_dotenv = true in ~/.leviath/config.toml,\n# or LEVIATH_LOAD_DOTENV=1 for one command.\n# ANTHROPIC_API_KEY=sk-ant-...\n# OPENAI_API_KEY=sk-...\n# OPENROUTER_API_KEY=sk-or-...\n";
    write_file(
        &blueprint_dir.join(".env.example"),
        env_example_content.as_bytes(),
    )?;

    println!("Created blueprint: {}", args.name);
    println!("\nNext steps:");
    println!("  cd {}", args.name);
    println!("  lev run . --task \"Your task here\"");
    println!(
        "  lev add . && {}",
        crate::commands::run::run_line(&args.name, "Your task here")
    );

    Ok(())
}

/// Escapes a string for embedding inside a TOML basic (double-quoted)
/// string literal, so a name holding a backslash or a quote still writes a
/// file that reads.
fn toml_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The blueprint name for a `lev create` argument: its last path component.
///
/// The argument is the directory to create, and may be a whole path (on
/// Windows, `C:\Users\...\my-agent`). The blueprint is installed and run by
/// the directory's own name, and an installed blueprint must call itself by
/// the name of the directory it is in, so that is the name it is given.
fn blueprint_name(path: &str) -> &str {
    path.rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(path)
}

/// The `agent.toml` a template writes for a blueprint called `name`.
fn create_manifest(name: &str, template: &str) -> String {
    let text = match template {
        "coder" => CODER_TEMPLATE,
        "researcher" => RESEARCHER_TEMPLATE,
        _ => DEFAULT_TEMPLATE,
    };
    text.replace("__NAME__", &toml_escape(blueprint_name(name)))
}

const CODER_TEMPLATE: &str = r#"[blueprint]
name = "__NAME__"
version = "0.1.0"
description = "A coding assistant blueprint"

# analyze -> implement

[graph]
# Write and shell tools ask for approval unless the run is started with
# `--yolo`.
tool_permissions = { read_file = "allow", list_dir = "allow", write_file = "ask", edit_file = "ask", bash = "ask" }

[[graph.inputs]]
name = "task"
type = { kind = "text", multiline = true }
required = true
binds = [{ region = "task" }]

# Region budgets are percentages of the model's context window (ceilings, may
# sum past 100%), so a region scales with whatever model the stage runs. Every
# blueprint needs a `conversation` sliding window: it holds the message stream
# and is carried across stage edges.
[graph.layout]
total_budget_tokens = 0
regions = [
    { name = "task", kind = "pinned", budget = "2%", required = true, required_message = "Describe the coding task via --task." },
    { name = "codebase", kind = "temporary", budget = "20%" },
    { name = "conversation", kind = { kind = "sliding_window", max_items = 20, eviction = { bulk = 10 } }, budget = "15%" },
    { name = "scratch", kind = "clearable", budget = "8%" },
]

[[graph.stages]]
name = "analyze"
description = "Understand the task and plan the implementation"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-4-6" }] }
tools = ["read_file", "list_dir"]
max_iterations = 15
# Large file reads land in the `codebase` region (a short pointer stays in the
# conversation); everything else stays inline. Never route to a sliding window
# other than `conversation`.
tool_routing = { default_region = "conversation", tool_regions = { read_file = "codebase", list_dir = "codebase" } }
system_prompt = """
Analyze the coding task in the `task` region and produce a concise implementation
plan: which files to create/modify, what each does, and the key decisions.
"""

[[graph.stages]]
name = "implement"
description = "Write code according to the plan"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-4-6" }] }
tools = ["write_file", "read_file", "edit_file", "list_dir", "bash"]
max_iterations = 50
tool_routing = { default_region = "conversation", tool_regions = { read_file = "codebase", list_dir = "codebase" } }
system_prompt = """
Implement the plan. Create all necessary files, then use bash to run tests and
verify the build. Read existing code from the `codebase` region.
"""

[[graph.edges]]
name = "implement"
from = "analyze"
to = "implement"
"#;

const RESEARCHER_TEMPLATE: &str = r#"[blueprint]
name = "__NAME__"
version = "0.1.0"
description = "A research assistant blueprint"

# gather -> synthesize

[graph]
tool_permissions = { read_file = "allow", list_dir = "allow", bash = "ask" }

[[graph.inputs]]
name = "task"
type = { kind = "text", multiline = true }
required = true
binds = [{ region = "query" }]

# Region budgets are percentages of the model's context window (ceilings, may
# sum past 100%), so a region scales with whatever model the stage runs. Every
# blueprint needs a `conversation` sliding window: it holds the message stream
# and is carried across stage edges. A `compacting` region needs a
# `compact_history` region for its summaries.
[graph.layout]
total_budget_tokens = 0
regions = [
    { name = "query", kind = "pinned", budget = "2%", required = true, required_message = "State the research question via --task." },
    { name = "sources", kind = "temporary", budget = "25%" },
    { name = "findings", kind = "compacting", budget = "12%", compact_at = 0.8 },
    { name = "findings_history", kind = { kind = "compact_history", source = "findings" }, budget = "3%" },
    { name = "conversation", kind = { kind = "sliding_window", max_items = 15, eviction = { bulk = 10 } }, budget = "12%" },
    { name = "scratch", kind = "clearable", budget = "6%" },
]

[[graph.stages]]
name = "gather"
description = "Gather relevant information"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-4-6" }] }
# For real web research, drop web_search.rhai / web_fetch.rhai into a `tools/`
# directory beside this file and add them here (see the bundled researcher).
tools = ["read_file", "list_dir", "bash"]
max_iterations = 20
tool_routing = { default_region = "conversation", tool_regions = { read_file = "sources", list_dir = "sources", bash = "sources" } }
system_prompt = """
Gather source material on the topic in the `query` region. Use read_file/list_dir
for local material and bash for anything else; raw content lands in `sources`.
Note where each item came from and the claims it supports.
"""

[[graph.stages]]
name = "synthesize"
description = "Synthesize findings and discuss with user"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-4-6" }] }
mode = "interactive"
tools = ["read_file", "list_dir"]
max_iterations = 15
system_prompt = """
Synthesize the `sources` into `findings`: themes, agreements/disagreements, and
well-supported vs speculative claims. Cite specific sources.
"""

# The conversation is summarized on the way, so synthesis starts from what was
# found rather than from every raw tool call.
[[graph.edges]]
name = "synthesize"
from = "gather"
to = "synthesize"
carry = { compact = {} }
"#;

const DEFAULT_TEMPLATE: &str = r#"[blueprint]
name = "__NAME__"
version = "0.1.0"
description = "A simple agent blueprint"

[graph]
tool_permissions = { read_file = "allow", list_dir = "allow", write_file = "ask", bash = "ask" }

[[graph.inputs]]
name = "task"
type = { kind = "text", multiline = true }
required = true
binds = [{ region = "task" }]

# Region budgets are percentages of the model's context window (ceilings, may
# sum past 100%), so a region scales with whatever model the stage runs. Every
# blueprint needs a `conversation` sliding window: it holds the message stream
# and is carried across stage edges.
[graph.layout]
total_budget_tokens = 0
regions = [
    { name = "task", kind = "pinned", budget = "2%", required = true, required_message = "Describe the task via --task." },
    { name = "conversation", kind = { kind = "sliding_window", max_items = 10, eviction = { bulk = 10 } }, budget = "12%" },
    { name = "scratch", kind = "clearable", budget = "6%" },
]

[[graph.stages]]
name = "main"
description = "Main execution stage"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-4-6" }] }
tools = ["read_file", "list_dir", "write_file", "bash"]
max_iterations = 30
system_prompt = """
You are a helpful agent. Complete the task described in the `task` region
thoroughly.
"""
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::with_tracing;
    use leviath_blueprint::BlueprintFile;
    use leviath_runtime::spec::graph::{Budget, RegionKind};

    const TEMPLATES: [&str; 4] = ["default", "coder", "researcher", "other"];

    /// A template's file, read.
    fn read(name: &str, template: &str) -> BlueprintFile {
        BlueprintFile::parse(&create_manifest(name, template)).expect("a template reads")
    }

    #[test]
    fn default_template_names_and_versions_the_blueprint() {
        let file = read("test-agent", "default");
        assert_eq!(file.blueprint.name.as_str(), "test-agent");
        assert_eq!(file.blueprint.version, "0.1.0");
    }

    /// `lev create` takes the directory to create, which may be a whole path,
    /// and on Windows one holding backslashes. The blueprint is named after
    /// the directory itself, the name it is installed and run by.
    #[test]
    fn a_path_names_the_blueprint_after_its_last_component() {
        let name = r"C:\Users\RUNNER~1\AppData\Local\Temp\.tmpmAlPt3\default-template-agent";
        for template in TEMPLATES {
            assert_eq!(
                read(name, template).blueprint.name.as_str(),
                "default-template-agent"
            );
        }
        assert_eq!(blueprint_name("/tmp/agents/coder/"), "coder");
        assert_eq!(blueprint_name("plain"), "plain");
        // Nothing but separators has no last component, and stands as written.
        assert_eq!(blueprint_name("/"), "/");
    }

    #[test]
    fn name_with_embedded_quote_produces_valid_toml() {
        let name = r#"my"agent"#;
        assert_eq!(read(name, "default").blueprint.name.as_str(), name);
        assert_eq!(toml_escape(r"a\b"), r"a\\b");
    }

    /// Every template is a blueprint `lev validate` and a spawn accept: it
    /// reads, and its graph holds together.
    #[test]
    fn every_template_validates() {
        for template in TEMPLATES {
            let dir = tempfile::tempdir().unwrap();
            crate::test_support::write_test_agent(
                dir.path(),
                create_manifest("valid-agent", template),
            );
            let loaded = leviath_blueprint::validate(dir.path()).expect("a template validates");
            assert_eq!(loaded.reference.name.as_str(), "valid-agent");
            // Budgets are shares of the window, so the template scales with
            // whatever model it runs on.
            assert!(
                loaded
                    .graph
                    .layout
                    .regions
                    .iter()
                    .all(|r| matches!(r.budget, Budget::Percent { .. })),
                "{template} template should use percentage budgets"
            );
        }
    }

    #[test]
    fn every_template_satisfies_context_layout_invariants() {
        for template in TEMPLATES {
            let graph = read("inv-agent", template).run_graph();
            let regions = &graph.layout.regions;
            let sliding = |r: &&leviath_runtime::spec::graph::RegionDef| {
                matches!(r.kind, RegionKind::SlidingWindow { .. })
            };

            // An explicit conversation sliding window.
            assert!(
                regions
                    .iter()
                    .filter(sliding)
                    .any(|r| r.name.as_str() == "conversation"),
                "{template} template needs an explicit conversation sliding_window"
            );

            // No routing targets a non-conversation sliding window.
            let windows: std::collections::HashSet<&str> = regions
                .iter()
                .filter(sliding)
                .map(|r| r.name.as_str())
                .collect();
            for stage in &graph.stages {
                let Some(routing) = &stage.tool_routing else {
                    continue;
                };
                let targets = std::iter::once(&routing.default_region)
                    .chain(routing.tool_regions.values())
                    .map(|r| r.as_str());
                for t in targets {
                    assert!(
                        t == "conversation" || !windows.contains(t),
                        "{template} stage '{}' routes to non-conversation sliding_window '{t}'",
                        stage.name
                    );
                }
            }

            // Every compacting region has a compact_history pair.
            let histories: Vec<&str> = regions
                .iter()
                .filter_map(|r| match &r.kind {
                    RegionKind::CompactHistory { source } => source.as_ref().map(|s| s.as_str()),
                    _ => None,
                })
                .collect();
            for r in regions {
                if matches!(r.kind, RegionKind::Compacting { .. }) {
                    assert!(
                        histories.contains(&r.name.as_str()),
                        "{template} compacting region '{}' has no compact_history pair",
                        r.name
                    );
                }
            }
        }
    }

    /// The stage names of a template's graph.
    fn stages(template: &str) -> Vec<String> {
        read("x", template)
            .graph
            .stages
            .iter()
            .map(|s| s.name.to_string())
            .collect()
    }

    #[test]
    fn unknown_template_falls_back_to_default() {
        assert_eq!(stages("nonexistent-template"), ["main"]);
    }

    #[test]
    fn coder_template_has_analyze_and_implement_stages() {
        assert_eq!(stages("coder"), ["analyze", "implement"]);
    }

    #[test]
    fn researcher_template_has_gather_and_synthesize_stages() {
        assert_eq!(stages("researcher"), ["gather", "synthesize"]);
    }

    #[test]
    fn template_embeds_agent_name() {
        let manifest = create_manifest("special-name-123", "coder");
        assert!(manifest.contains("special-name-123"));
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

        assert!(blueprint_path.join("agent.toml").exists());
        assert!(blueprint_path.join(".gitignore").exists());
        assert!(blueprint_path.join(".env.example").exists());

        let manifest = fs::read_to_string(blueprint_path.join("agent.toml")).unwrap();
        assert!(manifest.contains("analyze"));
    }

    /// What `lev create` writes is what `lev add` installs and `lev run`
    /// finds: the file names itself after its directory.
    #[tokio::test]
    async fn execute_names_the_blueprint_after_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let blueprint_path = dir.path().join("default-template-agent");
        let args = CreateArgs {
            name: blueprint_path.to_str().unwrap().to_string(),
            template: "default".to_string(),
        };

        with_tracing(|| execute(args)).await.unwrap();

        let loaded = leviath_blueprint::validate(&blueprint_path).unwrap();
        assert_eq!(loaded.reference.name.as_str(), "default-template-agent");
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

        // `agent.toml` is unconditionally the *first* write `execute_with`
        // attempts, so failing on every call (rather than branching on the
        // path) is sufficient here and avoids an else-arm that could never
        // actually run: the `?` on this first failure returns before any
        // other path is ever passed to this closure.
        let result = execute_with(args, &|_path, _contents| {
            Err(std::io::Error::other("injected agent.toml write failure"))
        });

        let err = result.unwrap_err();
        assert!(
            err.to_string()
                .contains("injected agent.toml write failure")
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
        // The blueprint write before it genuinely happened.
        assert!(
            dir.path()
                .join("gitignore-write-fails")
                .join("agent.toml")
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
        assert!(created.join("agent.toml").exists());
        assert!(created.join(".gitignore").exists());
    }
}
