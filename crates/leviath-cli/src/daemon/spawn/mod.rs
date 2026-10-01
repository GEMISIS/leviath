//! What the daemon's spawn path still answers for others: the model defaults
//! a run's stages are chosen against, the shell environment and script tools
//! a run is given, the scripts a blueprint names (for `lev validate` and
//! `lev test`), read-path grants, and the tool lane's per-run state. Runs
//! themselves are started by [`DaemonStarter`](crate::daemon::starter::DaemonStarter).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};

use leviath_providers::Tool;
use leviath_runtime::interaction_hub::InteractionHub;
use leviath_runtime::pipeline::ModelDefaults;
use tokio::sync::Mutex;

use crate::config::Config;
use crate::daemon::subagent::SubAgentHandle;
use crate::daemon::tool_service::AgentToolState;

/// Default max sub-agent tree depth when a blueprint doesn't set one.
pub(crate) const DEFAULT_SUBAGENT_DEPTH: usize = 3;

// One section per question this answers. Each is re-exported because the
// daemon reaches them directly.
mod policy;
pub(crate) use policy::*;
mod scripts;
pub(crate) use scripts::*;
mod seeds;
pub(crate) use seeds::*;
mod tool_state;
pub(crate) use tool_state::*;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config::Config;
    use leviath_runtime::spec::graph::RunGraph;
    use std::collections::HashMap;
    use std::path::Path;

    #[test]
    fn fallback_order_parses_provider_slash_model_and_drops_junk() {
        // The `tracing::warn!` on the reject path evaluates its field
        // expressions only under a real subscriber.
        crate::test_support::with_tracing(|| {
            let parsed = parse_fallback_order(&[
                // A model id containing a slash must survive intact, which is
                // the common OpenRouter shape.
                "openrouter/deepseek/deepseek-v4-flash".to_string(),
                "anthropic/claude-sonnet-5".to_string(),
                // Rejected: a bare provider gives us no model to send.
                "anthropic".to_string(),
                "/no-provider".to_string(),
                "no-model/".to_string(),
                String::new(),
            ]);
            assert_eq!(
                parsed
                    .iter()
                    .map(|e| (e.provider.as_str(), e.model.as_str()))
                    .collect::<Vec<_>>(),
                vec![
                    ("openrouter", "deepseek/deepseek-v4-flash"),
                    ("anthropic", "claude-sonnet-5"),
                ]
            );
        });
    }

    #[test]
    fn model_defaults_carries_the_fallback_chain_from_config() {
        let mut config = Config {
            default_provider: "openrouter".to_string(),
            override_model: Some("deepseek".to_string()),
            fallback_model: Some("flash".to_string()),
            ..Default::default()
        };
        config.providers.fallback_order = vec!["anthropic/claude-sonnet-5".to_string()];
        let defaults = model_defaults(&config);
        assert_eq!(defaults.provider, "openrouter");
        assert_eq!(defaults.override_model.as_deref(), Some("deepseek"));
        assert_eq!(defaults.fallback_model.as_deref(), Some("flash"));
        assert_eq!(defaults.fallback_order.len(), 1);
        assert_eq!(defaults.fallback_order[0].provider, "anthropic");
    }

    #[test]
    fn discover_script_tools_registers_and_drops_collisions() {
        crate::test_support::with_tracing(|| {});
        // Point LEVIATH_HOME at an empty temp dir so the global tools/ scan is
        // hermetic (no real ~/.leviath/tools leaking in).
        let home = tempfile::tempdir().unwrap();
        temp_env::with_var("LEVIATH_HOME", Some(home.path().to_str().unwrap()), || {
            let agent_dir = tempfile::tempdir().unwrap();
            let tools = agent_dir.path().join("tools");
            std::fs::create_dir(&tools).unwrap();
            std::fs::write(tools.join("echo.rhai"), "// @tool echo\nparams.x").unwrap();
            // A tool named after a built-in must be dropped (never shadow it).
            std::fs::write(tools.join("read_file.rhai"), "// @tool read_file\n1").unwrap();
            // A tool colliding with an MCP tool is also dropped (exercises the
            // mcp_tool_defs reservation).
            std::fs::write(tools.join("mcp_tool.rhai"), "// @tool mcp_tool\n1").unwrap();
            // A malformed script is skipped + warned about (the skipped loop).
            std::fs::write(tools.join("bad.rhai"), "no tool directive\nlet").unwrap();
            // A tool requiring a capability this platform can't provide is dropped
            // (unknown cap name → never satisfiable). Desktop has every real cap,
            // so a bogus name is the portable way to exercise the drop branch.
            std::fs::write(
                tools.join("needs_gpu.rhai"),
                "// @tool needs_gpu\n// @requires gpu\n1",
            )
            .unwrap();
            // A tool requiring a capability the desktop platform *does* provide is kept.
            std::fs::write(
                tools.join("net_tool.rhai"),
                "// @tool net_tool\n// @requires network\n1",
            )
            .unwrap();
            let blueprint = agent_dir.path().join(leviath_blueprint::FILE_NAME);

            let builtins: HashSet<String> = ["read_file".to_string()].into_iter().collect();
            let mcp = vec![leviath_providers::Tool {
                name: "mcp_tool".to_string(),
                description: String::new(),
                parameters: serde_json::json!({}),
            }];
            let dirs = script_dirs(&blueprint);
            let (set, names, defs) =
                discover_script_tools_in(&dirs, &reserved_tool_names(&builtins, &mcp));
            // Compiled the valid ones; only the non-colliding, platform-satisfiable
            // ones are routable.
            assert!(set.contains("echo") && set.contains("read_file"));
            assert!(names.contains("echo"));
            assert!(!names.contains("read_file"));
            assert!(!names.contains("mcp_tool"));
            assert!(!names.contains("needs_gpu"), "unsatisfiable cap dropped");
            assert!(names.contains("net_tool"), "satisfiable cap kept");
            let mut def_names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
            def_names.sort_unstable();
            assert_eq!(def_names, vec!["echo", "net_tool"]);
        });
    }

    #[test]
    fn script_cap_maps_known_and_unknown_names() {
        use leviath_tools::ToolCapability::*;
        assert_eq!(script_cap("network"), Some(Network));
        assert_eq!(script_cap("http"), Some(Network));
        assert_eq!(script_cap("shell"), Some(ProcessSpawn));
        assert_eq!(script_cap("process_spawn"), Some(ProcessSpawn));
        assert_eq!(script_cap("filesystem"), Some(FileSystem));
        assert_eq!(script_cap("fs"), Some(FileSystem));
        assert_eq!(script_cap("gpu"), None);
    }

    #[test]
    fn platform_satisfies_caps_gates_on_support() {
        use leviath_tools::{PlatformCapabilities, ToolCapability};
        // Empty requirement is always satisfied.
        let mobile = PlatformCapabilities::mobile();
        assert!(platform_satisfies_caps(&mobile, &[]));
        // Mobile has filesystem/network but not process spawning.
        assert!(platform_satisfies_caps(&mobile, &["network".to_string()]));
        assert!(!platform_satisfies_caps(&mobile, &["shell".to_string()]));
        // An unknown cap name is never satisfiable, even on a full desktop.
        let desktop = PlatformCapabilities::from_capabilities([
            ToolCapability::Network,
            ToolCapability::FileSystem,
            ToolCapability::ProcessSpawn,
        ]);
        assert!(!platform_satisfies_caps(&desktop, &["mystery".to_string()]));
    }

    #[test]
    fn discover_script_tools_empty_when_no_tools_dir() {
        let home = tempfile::tempdir().unwrap();
        temp_env::with_var("LEVIATH_HOME", Some(home.path().to_str().unwrap()), || {
            let agent_dir = tempfile::tempdir().unwrap();
            let blueprint = agent_dir.path().join(leviath_blueprint::FILE_NAME);
            let dirs = script_dirs(&blueprint);
            let (set, names, defs) =
                discover_script_tools_in(&dirs, &reserved_tool_names(&HashSet::new(), &[]));
            assert!(set.is_empty() && names.is_empty() && defs.is_empty());
        });
    }

    // ─── check_graph_code ────────────────────────────────────────────────
    /// A one-stage blueprint's graph: `graph_extra` lands in `[graph]` before
    /// any table, `stage_extra` in its stage, and `regions_extra` in the
    /// layout's region list.
    fn graph_with(graph_extra: &str, stage_extra: &str, regions_extra: &str) -> RunGraph {
        let text = format!(
            r#"
[blueprint]
name = "fixture"
version = "0.1.0"

[graph]
{graph_extra}

[[graph.stages]]
name = "main"
model = {{ models = [{{ provider = "anthropic", model = "m" }}] }}
{stage_extra}

[graph.layout]
total_budget_tokens = 4000
regions = [
    {regions_extra}
    {{ name = "conversation", kind = {{ kind = "sliding_window", max_items = 20 }}, budget = 2000 }},
]
"#
        );
        leviath_blueprint::BlueprintFile::parse(&text)
            .expect("the fixture parses")
            .run_graph()
    }

    /// A graph whose only code is the hooks `hooks` names on its stage.
    fn hooked(hooks: &str) -> RunGraph {
        graph_with("", &format!("hooks = {{ {hooks} }}"), "")
    }

    /// The graph's output and its stage's, each with an optional validator.
    fn validated(graph_script: Option<&str>, stage_script: Option<&str>) -> RunGraph {
        let output = |script: &str| format!("output = {{ validator = {{ file = \"{script}\" }} }}");
        graph_with(
            &graph_script.map(output).unwrap_or_default(),
            &stage_script.map(output).unwrap_or_default(),
            "",
        )
    }

    /// A blueprint whose graph layout and whose stage's own layout each hold
    /// a custom region, with their scripts in `hooks/` beside the blueprint.
    pub(crate) fn custom_region_manifest() -> &'static str {
        r#"
[blueprint]
name = "cr"
version = "0.1.0"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "m" }] }

[graph.stages.layout]
total_budget_tokens = 2000
regions = [{ name = "stage_view", kind = { kind = "custom", code = { file = "hooks/stage.rhai" } }, budget = 2000 }]

[graph.layout]
total_budget_tokens = 4000
regions = [{ name = "brain", kind = { kind = "custom", code = { file = "hooks/brain.rhai" } }, budget = 4000 }]
"#
    }

    fn custom_regions() -> RunGraph {
        leviath_blueprint::BlueprintFile::parse(custom_region_manifest())
            .expect("the fixture parses")
            .run_graph()
    }

    /// A blueprint directory with `files` written into it.
    fn dir_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (path, text) in files {
            let full = dir.path().join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, text).unwrap();
        }
        dir
    }

    /// A graph that names no code has nothing to check.
    #[test]
    fn a_graph_with_no_code_checks_clean() {
        let dir = dir_with(&[]);
        check_graph_code(&graph_with("", "", ""), dir.path()).unwrap();
    }

    /// Checked before a run starts, so a broken validator stops the run before
    /// any tokens are spent. The same script named twice is read once.
    #[test]
    fn an_output_validator_is_compiled() {
        let dir = dir_with(&[("validators/shape.rhai", "fn validate(content) { () }")]);
        let graph = validated(Some("validators/shape.rhai"), Some("validators/shape.rhai"));
        check_graph_code(&graph, dir.path()).expect("it compiles");

        // A shape with no validator is checked by nothing.
        let shaped = graph_with("", "output = { format = \"a2ui\" }", "");
        check_graph_code(&shaped, dir.path()).unwrap();

        // Inline code is compiled where it stands.
        let inline = graph_with(
            "output = { validator = { inline = \"fn validate(content) { () }\" } }",
            "",
            "",
        );
        check_graph_code(&inline, dir.path()).unwrap();
    }

    #[test]
    fn a_missing_or_broken_validator_is_refused() {
        let dir = dir_with(&[("broken.rhai", "fn validate(a, b) { () }")]);
        let err = check_graph_code(&validated(None, Some("validators/gone.rhai")), dir.path())
            .expect_err("a script that is not there");
        assert!(err.contains("cannot read output validator"), "{err}");
        assert!(err.contains("gone.rhai"), "{err}");

        let err = check_graph_code(&validated(None, Some("broken.rhai")), dir.path())
            .expect_err("wrong arity");
        assert!(err.contains("output validator failed to compile"), "{err}");
    }

    #[test]
    fn a_declared_hook_is_compiled() {
        let dir = dir_with(&[("h.rhai", "fn on_stage_enter(ctx) { () }")]);
        check_graph_code(
            &hooked("on_stage_enter = { file = \"h.rhai\" }"),
            dir.path(),
        )
        .unwrap();
    }

    /// One file backing two hooks is compiled once and must define both.
    #[test]
    fn one_file_backing_two_hooks_must_define_both() {
        let both = dir_with(&[(
            "h.rhai",
            "fn on_stage_enter(ctx) { () } fn on_stage_exit(ctx) { () }",
        )]);
        let graph =
            hooked("on_stage_enter = { file = \"h.rhai\" }, on_stage_exit = { file = \"h.rhai\" }");
        check_graph_code(&graph, both.path()).unwrap();

        // The blueprint named this file for a hook it does not implement. A
        // hook that never runs looks exactly like one that ran and allowed
        // everything, so it is refused.
        let one = dir_with(&[("h.rhai", "fn on_stage_exit(ctx) { () }")]);
        let err = check_graph_code(&graph, one.path()).expect_err("on_stage_enter is missing");
        assert!(err.contains("defines no"), "{err}");
    }

    #[test]
    fn a_missing_or_broken_hook_script_is_refused() {
        let dir = dir_with(&[("broken.rhai", "fn on_stage_enter(ctx) {")]);
        let err = check_graph_code(
            &hooked("on_stage_enter = { file = \"gone.rhai\" }"),
            dir.path(),
        )
        .expect_err("a missing script is a spawn error");
        assert!(err.contains("cannot read stage hook script"), "{err}");

        let err = check_graph_code(
            &hooked("on_stage_enter = { file = \"broken.rhai\" }"),
            dir.path(),
        )
        .expect_err("a broken script is a spawn error");
        assert!(
            err.contains("stage hook script 'broken.rhai' failed to compile"),
            "{err}"
        );
    }

    #[test]
    fn custom_region_scripts_in_every_layout_are_compiled() {
        let dir = dir_with(&[
            ("hooks/brain.rhai", "fn render(ctx) { \"b\" }"),
            ("hooks/stage.rhai", "fn render(ctx) { \"s\" }"),
        ]);
        check_graph_code(&custom_regions(), dir.path()).unwrap();

        // Two regions naming one script read it once.
        let shared = graph_with(
            "",
            "",
            "{ name = \"a\", kind = { kind = \"custom\", code = { file = \"hooks/brain.rhai\" } }, budget = 1000 },\
             { name = \"b\", kind = { kind = \"custom\", code = { file = \"hooks/brain.rhai\" } }, budget = 1000 },",
        );
        check_graph_code(&shared, dir.path()).unwrap();
    }

    #[test]
    fn a_missing_or_broken_region_script_names_its_region() {
        let missing = dir_with(&[]);
        let err = check_graph_code(&custom_regions(), missing.path()).unwrap_err();
        assert!(err.contains("region 'brain'"), "{err}");
        assert!(err.contains("hooks/brain.rhai"), "{err}");

        let broken = dir_with(&[
            ("hooks/brain.rhai", "fn render(ctx) {"),
            ("hooks/stage.rhai", "fn render(ctx) { \"s\" }"),
        ]);
        let err = check_graph_code(&custom_regions(), broken.path()).unwrap_err();
        assert!(err.contains("failed to compile"), "{err}");
        assert!(err.contains("region 'brain'"), "{err}");
    }

    /// Code is something the blueprint ships, so it has no `read_paths` escape
    /// at all: outside the blueprint's own directory is simply refused, before
    /// the file is read.
    #[test]
    fn code_outside_the_blueprint_directory_is_refused() {
        let root = dir_with(&[("outside.txt", "NOT RHAI")]);
        let bp_dir = root.path().join("agents").join("evil");
        std::fs::create_dir_all(&bp_dir).unwrap();
        let escaping = [
            hooked("on_stage_enter = { file = \"../../outside.txt\" }"),
            validated(Some("../../outside.txt"), None),
            graph_with(
                "",
                "",
                "{ name = \"notes\", kind = { kind = \"custom\", code = { file = \"../../outside.txt\" } }, budget = 2000 },",
            ),
        ];
        for graph in escaping {
            let err = check_graph_code(&graph, &bp_dir).expect_err("an escaping path is refused");
            assert!(err.contains("outside the blueprint's directory"), "{err}");
            // Refused before the read: a compile failure here would mean the
            // file had already been opened.
            assert!(!err.contains("failed to compile"), "{err}");
        }
    }

    /// Where a blueprint at `file` finds its script tools: its own `tools/`,
    /// then the global one.
    fn script_dirs(file: &std::path::Path) -> Vec<std::path::PathBuf> {
        file.parent()
            .map(|d| d.join("tools"))
            .into_iter()
            .chain(leviath_core::tools_dir())
            .collect()
    }

    #[test]
    fn read_path_policy_is_inactive_without_declarations() {
        let (policy, warning) =
            compile_read_path_policy("cto", &[], &Config::default(), Path::new("/w")).unwrap();
        assert!(!policy.is_active());
        assert!(warning.is_none());
    }

    fn declared(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|s| s.to_string()).collect()
    }

    /// Declared but ungranted: the agent still spawns, and the warning names
    /// the agent and shows both config stanzas that would grant the paths.
    #[test]
    fn read_path_policy_warns_when_nothing_grants() {
        let (policy, warning) = compile_read_path_policy(
            "cto",
            &declared(&["/data/runs", "glob:/data/docs/**"]),
            &Config::default(),
            Path::new("/w"),
        )
        .unwrap();
        assert!(policy.is_active());
        assert!(!policy.allow_blueprint);
        assert!(policy.grants.is_empty());
        let warning = warning.expect("ungranted declarations must warn");
        assert!(warning.contains("allow_blueprint_read_paths"), "{warning}");
        assert!(warning.contains("[agent_read_paths.cto]"), "{warning}");
        assert!(warning.contains("\"/data/runs\""), "{warning}");
        assert!(warning.contains("\"glob:/data/docs/**\""), "{warning}");
    }

    #[test]
    fn read_path_policy_is_quiet_when_granted() {
        let mut config = Config::default();
        config.agent_read_paths.insert(
            "cto".to_string(),
            crate::config::ReadPathGrants {
                allow: vec!["/data/runs".to_string()],
            },
        );
        let (policy, warning) =
            compile_read_path_policy("cto", &declared(&["/data/runs"]), &config, Path::new("/w"))
                .unwrap();
        assert!(policy.is_active());
        assert!(!policy.grants.is_empty());
        assert!(warning.is_none());
    }

    #[test]
    fn read_path_policy_is_quiet_under_the_override() {
        let mut config = Config::default();
        config.security.allow_blueprint_read_paths = true;
        let (policy, warning) =
            compile_read_path_policy("cto", &declared(&["/data/runs"]), &config, Path::new("/w"))
                .unwrap();
        assert!(policy.allow_blueprint);
        assert!(warning.is_none());
    }

    /// A malformed entry is a hard spawn error naming its source - the
    /// blueprint's list or the user's own grant list.
    #[test]
    fn read_path_policy_rejects_bad_entries_loudly() {
        let err = compile_read_path_policy(
            "cto",
            &declared(&["glob:["]),
            &Config::default(),
            Path::new("/w"),
        )
        .unwrap_err();
        assert!(err.contains("agent 'cto' [read_paths]"), "{err}");

        let mut config = Config::default();
        config.security.read_paths = vec!["regex:(".to_string()];
        let err =
            compile_read_path_policy("cto", &declared(&["/data/runs"]), &config, Path::new("/w"))
                .unwrap_err();
        assert!(err.contains("config.toml"), "{err}");
    }

    /// Granted read paths raise the read tools to `Private`; nothing else
    /// moves, and an ungranted or missing tool entry is left alone.
    #[test]
    fn read_sensitivities_bump_only_the_read_tools_when_granted() {
        use leviath_core::TaintLevel;
        let base = || {
            HashMap::from([
                ("read_file".to_string(), TaintLevel::Internal),
                ("list_dir".to_string(), TaintLevel::Public),
                ("write_file".to_string(), TaintLevel::Internal),
            ])
        };

        let mut map = base();
        bump_read_sensitivities(&mut map, true);
        assert_eq!(map.get("read_file"), Some(&TaintLevel::Private));
        assert_eq!(map.get("list_dir"), Some(&TaintLevel::Private));
        assert_eq!(map.get("write_file"), Some(&TaintLevel::Internal));
        // `read_files` was absent from the map: no entry invented for it.
        assert!(!map.contains_key("read_files"));

        let mut map = base();
        bump_read_sensitivities(&mut map, false);
        assert_eq!(map, base(), "no grant, no change");
    }

    /// Every bundled agent that tells the user to pass `--task` declares a
    /// `task` input to hold one: a spawn handing a task to a graph that
    /// declares no such input is refused.
    #[test]
    fn every_bundled_agent_that_documents_a_task_accepts_one() {
        for agent in crate::bundled::BUNDLED_AGENTS {
            let name = agent.name;
            // Static `expect` messages rather than an interpolated `panic!`:
            // both facts already have their own named tests in `bundled.rs`,
            // so naming the agent here buys nothing and the closure would
            // leave a region no test can reach.
            let (_, content) = agent
                .files
                .iter()
                .find(|(rel, _)| *rel == leviath_blueprint::FILE_NAME)
                .expect("every bundled agent ships an agent.toml");
            let file = leviath_blueprint::BlueprintFile::parse(content)
                .expect("every bundled agent's blueprint parses");
            let accepts_task = file
                .graph
                .inputs
                .iter()
                .any(|input| input.name.as_str() == "task");
            assert!(
                !content.contains("--task") || accepts_task,
                "{name} tells the user to pass --task but declares no task input"
            );
        }
    }
}
