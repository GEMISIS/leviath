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
use leviath_runtime::spec::blueprint::Blueprint;
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
    use leviath_runtime::spec::Blueprint;
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
            let blueprint = agent_dir.path().join("agent.leviath");

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
            let blueprint = agent_dir.path().join("agent.leviath");
            let dirs = script_dirs(&blueprint);
            let (set, names, defs) =
                discover_script_tools_in(&dirs, &reserved_tool_names(&HashSet::new(), &[]));
            assert!(set.is_empty() && names.is_empty() && defs.is_empty());
        });
    }

    // ─── resolve_region_scripts ──────────────────────────────────────────
    /// Manifest with a global custom region and a per-stage one, both
    /// pointing into `hooks/` next to the manifest.
    pub(crate) fn custom_region_manifest() -> &'static str {
        "[agent]\nname = \"cr\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n\
         [context.regions.brain]\nkind = \"custom\"\nscript = \"hooks/brain.rhai\"\nmax_tokens = 4000\n\n\
         [stages.main]\nmodel = { provider = \"anthropic\", model = \"m\" }\n\n\
         [stages.main.context.regions.stage_view]\nkind = \"custom\"\nscript = \"hooks/stage.rhai\"\nmax_tokens = 2000\n"
    }

    // ── output validators ──
    fn validator_blueprint(agent_script: Option<&str>, stage_script: Option<&str>) -> Blueprint {
        let mut bp = leviath_runtime::spec::manifest::parse_manifest(
            "[agent]\nname = \"v\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n\
             [stages.main]\nmodel = { provider = \"anthropic\", model = \"m\" }\n",
        )
        .unwrap();
        let spec = |script: &str| leviath_core::output::OutputSpec {
            validator: Some(script.to_string()),
            ..leviath_core::output::OutputSpec::default()
        };
        bp.output = agent_script.map(spec);
        bp.stages[0].output = stage_script.map(spec);
        bp
    }

    /// Compiled at spawn, so a broken validator stops the run before any tokens
    /// are spent. The only other time the script is read is at the end, which is
    /// the worst possible moment to learn the agent cannot hand back its work.
    #[test]
    fn resolve_output_validators_compiles_each_distinct_script_once() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::create_dir(dir.path().join("validators")).unwrap();
        std::fs::write(
            dir.path().join("validators/shape.rhai"),
            "fn validate(content) { () }",
        )
        .unwrap();

        // The same script named by both the agent default and the stage: one
        // compile, one entry.
        let bp = validator_blueprint(Some("validators/shape.rhai"), Some("validators/shape.rhai"));
        let compiled =
            resolve_output_validators(&bp, &manifest.to_string_lossy()).expect("it compiles");

        assert_eq!(compiled.len(), 1);
        assert!(compiled.contains_key("validators/shape.rhai"));
    }

    /// A stage can declare a shape without a validator, which is the common
    /// case: a format label and some instructions, checked by nothing.
    #[test]
    fn resolve_output_validators_is_empty_without_any() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");

        // No output block at all.
        let bp = validator_blueprint(None, None);
        assert!(
            resolve_output_validators(&bp, &manifest.to_string_lossy())
                .unwrap()
                .is_empty()
        );

        // An output block that names no validator.
        let mut shaped = validator_blueprint(None, None);
        shaped.stages[0].output = Some(leviath_core::output::OutputSpec {
            format: Some("a2ui".to_string()),
            ..leviath_core::output::OutputSpec::default()
        });
        assert!(
            resolve_output_validators(&shaped, &manifest.to_string_lossy())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn resolve_output_validators_reports_a_missing_script() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        let bp = validator_blueprint(None, Some("validators/gone.rhai"));

        let err = resolve_output_validators(&bp, &manifest.to_string_lossy())
            .expect_err("a script that is not there");

        assert!(err.contains("cannot read output validator"), "{err}");
        assert!(err.contains("gone.rhai"), "{err}");
    }

    #[test]
    fn resolve_output_validators_reports_one_that_does_not_compile() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(dir.path().join("broken.rhai"), "fn validate(a, b) { () }").unwrap();
        let bp = validator_blueprint(None, Some("broken.rhai"));

        let err =
            resolve_output_validators(&bp, &manifest.to_string_lossy()).expect_err("wrong arity");

        assert!(err.contains("failed to compile"), "{err}");
    }

    // ─── resolve_stage_hook_scripts ──────────────────────────────────────
    fn hooked_manifest(hooks: &str) -> leviath_runtime::spec::Blueprint {
        leviath_runtime::spec::manifest::parse_manifest(&format!(
            "[agent]\nname = \"h\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n\
             [stages.main]\nmodel = {{ provider = \"anthropic\", model = \"m\" }}\n{hooks}"
        ))
        .expect("the fixture manifest parses")
    }

    #[test]
    fn stage_hooks_are_empty_when_no_stage_declares_one() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        let bp = hooked_manifest("");
        let got = resolve_stage_hook_scripts(&bp, &manifest.to_string_lossy()).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn a_declared_hook_is_compiled_and_keyed_by_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(dir.path().join("h.rhai"), "fn on_stage_enter(ctx) { () }").unwrap();
        let bp = hooked_manifest("[stages.main.hooks]\non_stage_enter = \"h.rhai\"\n");

        let got = resolve_stage_hook_scripts(&bp, &manifest.to_string_lossy()).unwrap();
        assert_eq!(got.len(), 1);
        assert!(got["h.rhai"].defines("on_stage_enter"));
    }

    /// One file backing both hooks is read and compiled once, not twice.
    #[test]
    fn one_file_backing_two_hooks_is_compiled_once() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(
            dir.path().join("h.rhai"),
            "fn on_stage_enter(ctx) { () } fn on_stage_exit(ctx) { () }",
        )
        .unwrap();
        let bp = hooked_manifest(
            "[stages.main.hooks]\non_stage_enter = \"h.rhai\"\non_stage_exit = \"h.rhai\"\n",
        );

        let got = resolve_stage_hook_scripts(&bp, &manifest.to_string_lossy()).unwrap();
        assert_eq!(got.len(), 1, "one entry, not one per hook");
        assert!(got["h.rhai"].defines("on_stage_enter"));
        assert!(got["h.rhai"].defines("on_stage_exit"));
    }

    /// Fail-fast at spawn: a missing script must not become a runtime surprise
    /// partway through a run.
    #[test]
    fn a_missing_hook_script_fails_the_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        let bp = hooked_manifest("[stages.main.hooks]\non_stage_enter = \"gone.rhai\"\n");

        let err = resolve_stage_hook_scripts(&bp, &manifest.to_string_lossy())
            .expect_err("a missing script is a spawn error");
        assert!(err.contains("cannot read stage hook script"), "{err}");
    }

    #[test]
    fn a_hook_script_that_does_not_compile_fails_the_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(dir.path().join("h.rhai"), "fn on_stage_enter(ctx) {").unwrap();
        let bp = hooked_manifest("[stages.main.hooks]\non_stage_enter = \"h.rhai\"\n");

        let err = resolve_stage_hook_scripts(&bp, &manifest.to_string_lossy())
            .expect_err("a broken script is a spawn error");
        assert!(err.contains("failed to compile"), "{err}");
    }

    /// The blueprint named this file for a hook it does not implement. Letting
    /// that spawn would give a hook that never runs, which looks exactly like
    /// one that ran and allowed everything.
    #[test]
    fn a_file_that_lacks_the_hook_it_was_named_for_fails_the_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(dir.path().join("h.rhai"), "fn on_stage_exit(ctx) { () }").unwrap();
        let bp = hooked_manifest("[stages.main.hooks]\non_stage_enter = \"h.rhai\"\n");

        let err = resolve_stage_hook_scripts(&bp, &manifest.to_string_lossy())
            .expect_err("a file missing its named hook is a spawn error");
        assert!(err.contains("defines no"), "{err}");
    }

    #[test]
    fn resolve_region_scripts_empty_without_custom_regions() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        let bp = leviath_runtime::spec::manifest::parse_manifest(
            "[agent]\nname = \"plain\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n\
             [stages.main]\nmodel = { provider = \"anthropic\", model = \"m\" }\n",
        )
        .unwrap();
        let scripts = resolve_region_scripts(&bp, &manifest.to_string_lossy()).unwrap();
        assert!(scripts.is_empty());
    }

    #[test]
    fn resolve_region_scripts_collects_global_and_per_stage_layouts() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::create_dir(dir.path().join("hooks")).unwrap();
        std::fs::write(
            dir.path().join("hooks/brain.rhai"),
            "fn render(ctx) { \"b\" }",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("hooks/stage.rhai"),
            "fn render(ctx) { \"s\" }",
        )
        .unwrap();
        let bp = leviath_runtime::spec::manifest::parse_manifest(custom_region_manifest()).unwrap();
        let scripts = resolve_region_scripts(&bp, &manifest.to_string_lossy()).unwrap();
        assert_eq!(scripts.len(), 2);
        assert!(scripts.contains_key("hooks/brain.rhai"));
        assert!(scripts.contains_key("hooks/stage.rhai"));
    }

    #[test]
    fn resolve_region_scripts_reads_a_shared_path_once() {
        // Two regions declaring the same script share one compiled Arc.
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::create_dir(dir.path().join("hooks")).unwrap();
        std::fs::write(
            dir.path().join("hooks/shared.rhai"),
            "fn render(ctx) { \"x\" }",
        )
        .unwrap();
        let bp = leviath_runtime::spec::manifest::parse_manifest(
            "[agent]\nname = \"cr\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n\
             [context.regions.a]\nkind = \"custom\"\nscript = \"hooks/shared.rhai\"\nmax_tokens = 2000\n\n\
             [context.regions.b]\nkind = \"custom\"\nscript = \"hooks/shared.rhai\"\nmax_tokens = 2000\n\n\
             [stages.main]\nmodel = { provider = \"anthropic\", model = \"m\" }\n",
        )
        .unwrap();
        let scripts = resolve_region_scripts(&bp, &manifest.to_string_lossy()).unwrap();
        assert_eq!(scripts.len(), 1);
    }

    #[test]
    fn resolve_region_scripts_missing_file_is_a_hard_error() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        let bp = leviath_runtime::spec::manifest::parse_manifest(custom_region_manifest()).unwrap();
        let err = resolve_region_scripts(&bp, &manifest.to_string_lossy()).unwrap_err();
        assert!(err.contains("region 'brain'"), "{err}");
        assert!(err.contains("hooks/brain.rhai"), "{err}");
    }

    #[test]
    fn resolve_region_scripts_uncompilable_script_is_a_hard_error() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::create_dir(dir.path().join("hooks")).unwrap();
        std::fs::write(dir.path().join("hooks/brain.rhai"), "fn render(ctx) {").unwrap();
        std::fs::write(
            dir.path().join("hooks/stage.rhai"),
            "fn render(ctx) { \"s\" }",
        )
        .unwrap();
        let bp = leviath_runtime::spec::manifest::parse_manifest(custom_region_manifest()).unwrap();
        let err = resolve_region_scripts(&bp, &manifest.to_string_lossy()).unwrap_err();
        assert!(err.contains("failed to compile"), "{err}");
        assert!(err.contains("region 'brain'"), "{err}");
    }

    /// Where a blueprint at `manifest` finds its script tools: its own
    /// `tools/`, then the global one.
    fn script_dirs(manifest: &std::path::Path) -> Vec<std::path::PathBuf> {
        manifest
            .parent()
            .map(|d| d.join("tools"))
            .into_iter()
            .chain(leviath_core::tools_dir())
            .collect()
    }

    fn model_cfg(models: Vec<(&str, &str)>) -> leviath_runtime::spec::blueprint::ModelConfig {
        leviath_runtime::spec::blueprint::ModelConfig {
            models: models
                .into_iter()
                .map(|(p, m)| leviath_runtime::spec::blueprint::ModelEntry {
                    provider: p.to_string(),
                    model: m.to_string(),
                })
                .collect(),
            allow_user_default: true,
            parameters: HashMap::new(),
            request_timeout_secs: None,
        }
    }

    fn blueprint_declaring(read_paths: &[&str]) -> Blueprint {
        let stage =
            leviath_runtime::spec::Stage::new("s".to_string(), model_cfg(vec![("anthropic", "m")]));
        let layout = leviath_runtime::spec::layout::ContextLayout::new(vec![], 1000);
        let mut bp = Blueprint::new("cto".to_string(), "d".to_string(), vec![stage], layout);
        if !read_paths.is_empty() {
            bp.read_paths = Some(leviath_runtime::spec::ReadPathsConfig {
                allow: read_paths.iter().map(|s| s.to_string()).collect(),
            });
        }
        bp
    }

    #[test]
    fn read_path_policy_is_inactive_without_declarations() {
        let bp = blueprint_declaring(&[]);
        let (policy, warning) = compile_read_path_policy(
            &bp.name,
            bp.read_paths.as_ref(),
            &Config::default(),
            Path::new("/w"),
        )
        .unwrap();
        assert!(!policy.is_active());
        assert!(warning.is_none());

        // An explicitly empty `[read_paths]` block is the same as none.
        let mut bp = blueprint_declaring(&[]);
        bp.read_paths = Some(leviath_runtime::spec::ReadPathsConfig { allow: vec![] });
        let (policy, warning) = compile_read_path_policy(
            &bp.name,
            bp.read_paths.as_ref(),
            &Config::default(),
            Path::new("/w"),
        )
        .unwrap();
        assert!(!policy.is_active());
        assert!(warning.is_none());
    }

    /// Declared but ungranted: the agent still spawns, and the warning names
    /// the agent and shows both config stanzas that would grant the paths.
    #[test]
    fn read_path_policy_warns_when_nothing_grants() {
        let bp = blueprint_declaring(&["/data/runs", "glob:/data/docs/**"]);
        let (policy, warning) = compile_read_path_policy(
            &bp.name,
            bp.read_paths.as_ref(),
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
        let bp = blueprint_declaring(&["/data/runs"]);
        let mut config = Config::default();
        config.agent_read_paths.insert(
            "cto".to_string(),
            crate::config::ReadPathGrants {
                allow: vec!["/data/runs".to_string()],
            },
        );
        let (policy, warning) =
            compile_read_path_policy(&bp.name, bp.read_paths.as_ref(), &config, Path::new("/w"))
                .unwrap();
        assert!(policy.is_active());
        assert!(!policy.grants.is_empty());
        assert!(warning.is_none());
    }

    #[test]
    fn read_path_policy_is_quiet_under_the_override() {
        let bp = blueprint_declaring(&["/data/runs"]);
        let mut config = Config::default();
        config.security.allow_blueprint_read_paths = true;
        let (policy, warning) =
            compile_read_path_policy(&bp.name, bp.read_paths.as_ref(), &config, Path::new("/w"))
                .unwrap();
        assert!(policy.allow_blueprint);
        assert!(warning.is_none());
    }

    /// A malformed entry is a hard spawn error naming its source - the
    /// blueprint's section or the user's own grant list.
    #[test]
    fn read_path_policy_rejects_bad_entries_loudly() {
        let bp = blueprint_declaring(&["glob:["]);
        let err = compile_read_path_policy(
            &bp.name,
            bp.read_paths.as_ref(),
            &Config::default(),
            Path::new("/w"),
        )
        .unwrap_err();
        assert!(err.contains("agent 'cto' [read_paths]"), "{err}");

        let bp = blueprint_declaring(&["/data/runs"]);
        let mut config = Config::default();
        config.security.read_paths = vec!["regex:(".to_string()];
        let err =
            compile_read_path_policy(&bp.name, bp.read_paths.as_ref(), &config, Path::new("/w"))
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

    // ─── resolve_seeds ────────────────────────────────────────────────────────
    fn bp(regions_toml: &str) -> Blueprint {
        // A region named `task` picks up the caller's task implicitly, which is
        // how a real blueprint accepts one - and without it a supplied task is
        // refused. Skipped when the caller declares its own, or the key would
        // be duplicated.
        let implicit_task = match regions_toml.contains("task") {
            true => "",
            false => "task = { kind = \"pinned\", max_tokens = 1000 }",
        };
        let toml = format!(
            r#"
    [agent]
    name = "seedy"

    [stages.main]
    mode = "autonomous"

    [stages.main.model]
    provider = "anthropic"
    model = "claude-sonnet-5"

    [context.regions]
    {regions_toml}
    {implicit_task}
    conversation = {{ kind = "sliding_window", max_items = 20, max_tokens = 10000 }}
    "#
        );
        leviath_runtime::spec::manifest::parse_manifest(&toml).unwrap()
    }

    /// A script is code the blueprint ships, so it has no `[read_paths]` escape
    /// at all: outside the blueprint's own directory is simply refused.
    #[test]
    fn a_hook_script_outside_the_blueprint_directory_is_refused() {
        let root = tempfile::tempdir().expect("tempdir");
        let bp_dir = root.path().join("agents").join("evil");
        std::fs::create_dir_all(&bp_dir).expect("dirs");
        std::fs::write(root.path().join("outside.txt"), "NOT RHAI").expect("write");

        let mut stage = leviath_runtime::spec::Stage::new(
            "main".to_string(),
            leviath_runtime::spec::blueprint::ModelConfig::new("p".to_string(), "m".to_string()),
        );
        stage.hooks.on_stage_enter = Some("../../outside.txt".to_string());
        let blueprint = leviath_runtime::spec::Blueprint::new(
            "evil".to_string(),
            "d".to_string(),
            vec![stage],
            leviath_runtime::spec::layout::ContextLayout::new(vec![], 1000),
        );

        let bp_path = bp_dir.join("agent.leviath");
        let err = resolve_stage_hook_scripts(&blueprint, bp_path.to_str().expect("utf8"))
            .expect_err("an escaping script path is refused");
        assert!(err.contains("outside the blueprint's directory"), "{err}");
        // Refused before the read, so the file is never opened: a compile
        // failure here would mean it had already been slurped.
        assert!(!err.contains("failed to compile"), "{err}");
    }

    #[test]
    fn a_custom_region_script_outside_the_blueprint_directory_is_refused() {
        let root = tempfile::tempdir().expect("tempdir");
        let bp_dir = root.path().join("agents").join("evil");
        std::fs::create_dir_all(&bp_dir).expect("dirs");
        std::fs::write(root.path().join("outside.txt"), "NOT RHAI").expect("write");

        let blueprint =
            bp(r#"notes = { kind = "custom", script = "../../outside.txt", max_tokens = 2000 }"#);
        let bp_path = bp_dir.join("agent.leviath");
        let err = resolve_region_scripts(&blueprint, bp_path.to_str().expect("utf8"))
            .expect_err("an escaping script path is refused");
        assert!(err.contains("outside the blueprint's directory"), "{err}");
    }

    #[test]
    fn an_output_validator_outside_the_blueprint_directory_is_refused() {
        let root = tempfile::tempdir().expect("tempdir");
        let bp_dir = root.path().join("agents").join("evil");
        std::fs::create_dir_all(&bp_dir).expect("dirs");
        std::fs::write(root.path().join("outside.txt"), "NOT RHAI").expect("write");

        let mut blueprint = leviath_runtime::spec::Blueprint::new(
            "evil".to_string(),
            "d".to_string(),
            vec![],
            leviath_runtime::spec::layout::ContextLayout::new(vec![], 1000),
        );
        blueprint.output = Some(leviath_core::output::OutputSpec {
            validator: Some("../../outside.txt".to_string()),
            ..Default::default()
        });
        let bp_path = bp_dir.join("agent.leviath");
        let err = resolve_output_validators(&blueprint, bp_path.to_str().expect("utf8"))
            .expect_err("an escaping validator path is refused");
        assert!(err.contains("outside the blueprint's directory"), "{err}");
    }

    /// The control: a script beside the blueprint compiles as before.
    #[test]
    fn a_hook_script_beside_the_blueprint_still_loads() {
        let root = tempfile::tempdir().expect("tempdir");
        let bp_dir = root.path().join("agents").join("good");
        std::fs::create_dir_all(&bp_dir).expect("dirs");
        std::fs::write(bp_dir.join("h.rhai"), "fn on_stage_enter(ctx) { () }").expect("write");

        let mut stage = leviath_runtime::spec::Stage::new(
            "main".to_string(),
            leviath_runtime::spec::blueprint::ModelConfig::new("p".to_string(), "m".to_string()),
        );
        stage.hooks.on_stage_enter = Some("h.rhai".to_string());
        let blueprint = leviath_runtime::spec::Blueprint::new(
            "good".to_string(),
            "d".to_string(),
            vec![stage],
            leviath_runtime::spec::layout::ContextLayout::new(vec![], 1000),
        );

        let bp_path = bp_dir.join("agent.leviath");
        let scripts = resolve_stage_hook_scripts(&blueprint, bp_path.to_str().expect("utf8"))
            .expect("a script beside the blueprint loads");
        assert!(scripts.contains_key("h.rhai"));
    }

    /// Every bundled agent that tells the user to pass `--task` can hold one.
    ///
    /// The refusal above is only safe if no shipped agent trips it while being
    /// driven the documented way. `reviewer` takes `--diff`, not `--task`, and
    /// that is fine; what would not be fine is an agent whose own description
    /// says `--task` while its blueprint has nowhere to put it.
    #[test]
    fn every_bundled_agent_that_documents_a_task_accepts_one() {
        for agent in crate::bundled::BUNDLED_AGENTS {
            let name = agent.name;
            // Static `expect` messages rather than an interpolated `panic!`:
            // both facts already have their own named test (`bundled.rs` for
            // the manifest's presence, `manifest_integration.rs` for its
            // parse), so naming the agent here buys nothing and the closure
            // would leave a region no test can reach.
            let (_, content) = agent
                .files
                .iter()
                .find(|(rel, _)| *rel == "agent.leviath")
                .expect("every bundled agent ships an agent.leviath");
            let bp = leviath_runtime::spec::manifest::parse_manifest(content)
                .expect("every bundled agent's manifest parses");
            // The question is `accepts_task`, not "did `resolve_seeds` error".
            // Driving the whole resolver here reported `coder` as refusing a
            // task on Windows only, because one of its *path* seeds failed
            // against the fixture workdir - an unrelated error the proxy could
            // not tell apart from the one under test.
            assert!(
                !content.contains("--task") || bp.accepts_task(),
                "{name} tells the user to pass --task but declares no region to hold one"
            );
        }
    }
}
