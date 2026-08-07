//! The daemon-side [`FanOutSpawner`]: resolves a fan-out worker's blueprint
//! (self-at-worker-stage / a named agent / a capability query) and starts it in
//! the shared world, seeded with its work item.
//!
//! The runtime's fan-out systems only *start and track* workers; *finding* the
//! blueprint is CLI policy, so it lives here - mirroring how [`build_agent`]
//! resolves any spawn. For `worker_stage` the worker runs the parent's own
//! blueprint entered at that stage (via [`leviath_runtime::pipeline::force_transition`]);
//! for `worker_agent` / `worker_query` it runs a separate installed blueprint.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bevy_ecs::entity::Entity;
use bevy_ecs::world::World;
use leviath_core::blueprint::FanOutConfig;
use leviath_providers::Tool;
use leviath_runtime::fanout::FanOutSpawner;
use leviath_runtime::host::SubAgentOp;
use leviath_runtime::interaction_hub::InteractionHub;
use leviath_runtime::persistence::RunMetadata;
use leviath_runtime::pipeline::{AgentBlueprint, force_transition};
use tokio::sync::Mutex;
use tokio::sync::mpsc::UnboundedSender;

use crate::daemon::client::{never_interactive, resolve_spawn_args};
use crate::daemon::spawn::{SpawnDeps, build_agent};
use crate::daemon::tool_service::CliToolService;

/// Everything [`build_agent`] needs, captured so a fan-out worker can be spawned
/// from inside a world-system (which has no access to the daemon's context).
#[derive(Clone)]
pub struct DaemonFanOutSpawner {
    /// Spawn-time config, read fresh per worker so a `config.toml` edit reaches
    /// fan-out workers too (shared with the daemon's main spawner).
    pub config: Arc<crate::daemon::config_reload::ConfigReloader>,
    /// The daemon-wide MCP executor, for servers configured globally.
    pub shared_mcp: Arc<Mutex<leviath_mcp::ToolExecutor>>,
    /// Tool definitions from `shared_mcp`, resolved once rather than per worker.
    pub mcp_tool_defs: Vec<Tool>,
    /// Shared MCP pool for per-agent `[[mcp_servers]]` - a fan-out worker
    /// advertises its blueprint's already-connected servers and lazily warms any
    /// uncached ones for subsequent workers of the same type.
    pub mcp_pool: Arc<crate::daemon::mcp_pool::McpPool>,
    /// Where a worker's prompts go. Shared with the daemon, so a fan-out
    /// worker's question reaches the same place as any other run's.
    pub hub: InteractionHub,
    /// Where a worker's own sub-agent spawns are sent.
    pub subagent_tx: UnboundedSender<SubAgentOp>,
    /// The tool dispatcher every worker's calls go through.
    pub tool_service: Arc<CliToolService>,
    /// `~/.leviath/agents`, for resolving `worker_query`. `None` when there is no
    /// home directory.
    pub agents_dir: Option<PathBuf>,
    /// The clock, injected so a test can stamp a worker deterministically.
    pub now_secs: fn() -> i64,
}

impl DaemonFanOutSpawner {
    /// A fan-out worker's advertised MCP defs: the global servers' defs plus its
    /// blueprint's already-cached servers. Any uncached server is warmed on a
    /// detached task (via the current runtime handle - `spawn_worker` runs inside
    /// the daemon's tick, on the runtime) so a subsequent worker of the same type
    /// advertises it. Returns just the global defs when the manifest is unreadable
    /// or declares no servers.
    fn worker_mcp_defs(&self, blueprint_path: &str) -> Vec<Tool> {
        let mut defs = self.mcp_tool_defs.clone();
        let Ok(toml) = std::fs::read_to_string(blueprint_path) else {
            return defs;
        };
        let servers = crate::daemon::mcp_pool::parse_blueprint_mcp_servers(&toml);
        if servers.is_empty() {
            return defs;
        }
        defs.extend(self.mcp_pool.cached_defs_for(&servers));
        // Warm any not-yet-connected servers for the next worker of this type, on
        // a detached task (we run inside the tick, on the runtime).
        tokio::runtime::Handle::current().spawn(self.mcp_pool.clone().ensure_all(servers));
        defs
    }
}

impl FanOutSpawner for DaemonFanOutSpawner {
    fn spawn_worker(
        &self,
        world: &mut World,
        parent: Entity,
        config: &FanOutConfig,
        item_id: &str,
        item_context: &serde_json::Value,
    ) -> Result<Entity, String> {
        // The parent supplies the worker's workdir, run id (parentage), and (for
        // `worker_stage`) its blueprint path.
        // `unattended` rides along: a worker of an unattended parent is a worker
        // nobody is watching either, and one that stops on an approval prompt
        // parks the parent behind it for good.
        // The requested output shape rides along too: a caller who asked the
        // parent for a2ui wants its workers' contributions in the same shape,
        // and the worker's answer is what the merge stage reads.
        let (parent_path, workdir, parent_run_id, unattended, output_request) = world
            .get::<RunMetadata>(parent)
            .map(|md| {
                (
                    md.agent_path.clone(),
                    md.workdir.clone(),
                    md.run_id.clone(),
                    md.unattended,
                    md.output_request.clone(),
                )
            })
            .ok_or_else(|| "fan-out parent has no run metadata".to_string())?;

        let (resolve_path, entry_stage) =
            resolve_worker_source(config, &parent_path, self.agents_dir.as_deref())?;

        let task = format_worker_task(item_id, item_context);
        let mut args = resolve_spawn_args(
            &resolve_path,
            Some(&task),
            &never_interactive,
            None,
            &workdir,
            unattended,
            Vec::new(),
            None,
            // Fan-out workers get their split of the parent task via `task`.
            std::collections::HashMap::new(),
            // Workers share the parent's workdir and are splits of a task the
            // parent already scoped, so re-running a repo-scan command seed once
            // per worker would be pure waste (and up to `max_workers` copies of
            // the same output).
            true,
            output_request,
        )
        .map_err(|e| format!("resolve worker blueprint: {e}"))?;
        // Nest the worker under its fan-out parent in the run tree.
        args.parent_run_id = Some(parent_run_id);

        // Per-agent MCP (issue #97): advertise the worker blueprint's servers that
        // are already connected in the shared pool (a `worker_stage` worker shares
        // the parent's - already warmed by the parent's preprocessor; the first
        // `worker_agent`/`worker_query` worker warms them here for its siblings).
        let mcp_defs = self.worker_mcp_defs(&args.blueprint_path);
        // The worker holds its blueprint's per-agent servers open like any
        // other run; the reap hook releases the lease when the worker ends.
        self.mcp_pool
            .lease_blueprint(&args.blueprint_path, &args.run_id);

        let config = self.config.current();
        let child = build_agent(
            world,
            SpawnDeps {
                tool_service: self.tool_service.as_ref(),
                config: &config,
                shared_mcp: self.shared_mcp.clone(),
                mcp_tool_defs: &mcp_defs,
                hub: &self.hub,
                now_secs: (self.now_secs)(),
                subagent_tx: self.subagent_tx.clone(),
            },
            &args,
        )?;

        // `worker_stage` runs the same blueprint entered at that stage rather than
        // its default entry stage.
        if let Some(stage) = entry_stage {
            match world
                .get::<AgentBlueprint>(child)
                .and_then(|bp| bp.0.stages.iter().position(|s| s.name == stage))
            {
                Some(idx) => force_transition(world, child, idx),
                None => return Err(format!("worker stage '{stage}' not found in blueprint")),
            }
        }
        Ok(child)
    }
}

/// Resolve a fan-out config's worker source to `(path_to_resolve, entry_stage)`.
/// `path_to_resolve` is fed to [`resolve_spawn_args`] (which resolves a file,
/// directory, or installed-agent name); `entry_stage` is `Some` only for
/// `worker_stage` (self-as-worker).
pub(crate) fn resolve_worker_source(
    config: &FanOutConfig,
    parent_path: &str,
    agents_dir: Option<&Path>,
) -> Result<(String, Option<String>), String> {
    if let Some(stage) = &config.worker_stage {
        return Ok((parent_path.to_string(), Some(stage.clone())));
    }
    if let Some(agent) = &config.worker_agent {
        return Ok((agent.clone(), None));
    }
    if let Some(query) = &config.worker_query {
        let path = discover_worker(agents_dir, query)?;
        return Ok((path.to_string_lossy().to_string(), None));
    }
    Err("fan-out config has no worker source".to_string())
}

/// The task text seeded into a worker: its id plus the compact JSON context.
fn format_worker_task(item_id: &str, item_context: &serde_json::Value) -> String {
    format!("Work item id: {item_id}\nContext: {item_context}")
}

/// Find an installed agent whose directory name or manifest description contains
/// `query` (case-insensitive). Returns the agent's directory.
fn discover_worker(agents_dir: Option<&Path>, query: &str) -> Result<PathBuf, String> {
    let dir = agents_dir.ok_or_else(|| "no agents directory to search for a worker".to_string())?;
    let needle = query.to_lowercase();
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("read agents dir '{}': {e}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let manifest = path.join("agent.leviath");
        if !manifest.is_file() {
            continue;
        }
        let name_matches = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.to_lowercase().contains(&needle));
        let desc_matches = std::fs::read_to_string(&manifest)
            .ok()
            .and_then(|c| leviath_core::manifest::parse_manifest(&c).ok())
            .is_some_and(|bp| bp.description.to_lowercase().contains(&needle));
        if name_matches || desc_matches {
            return Ok(path);
        }
    }
    Err(format!("no installed agent matches worker query '{query}'"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use leviath_core::blueprint::WorkerFailurePolicy;

    fn cfg(stage: Option<&str>, agent: Option<&str>, query: Option<&str>) -> FanOutConfig {
        FanOutConfig {
            worker_agent: agent.map(String::from),
            worker_stage: stage.map(String::from),
            worker_query: query.map(String::from),
            merge_stage: None,
            max_workers: 4,
            on_worker_failure: WorkerFailurePolicy::Continue,
            split_prompt: "split".to_string(),
            results_region: None,
            max_items: None,
        }
    }

    #[test]
    fn format_worker_task_includes_id_and_context() {
        let t = format_worker_task("t1", &serde_json::json!({"file": "a.rs"}));
        assert!(t.contains("Work item id: t1"));
        assert!(t.contains("a.rs"));
    }

    #[test]
    fn resolve_worker_source_picks_the_configured_source() {
        // worker_stage → parent path + entry stage.
        assert_eq!(
            resolve_worker_source(&cfg(Some("w"), None, None), "/p/agent.leviath", None).unwrap(),
            ("/p/agent.leviath".to_string(), Some("w".to_string()))
        );
        // worker_agent → the agent name, no entry stage.
        assert_eq!(
            resolve_worker_source(&cfg(None, Some("fixer"), None), "/p", None).unwrap(),
            ("fixer".to_string(), None)
        );
    }

    #[test]
    fn resolve_worker_source_worker_query_discovers_an_agent() {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path().join("test-fixer");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(
            agent.join("agent.leviath"),
            "[agent]\nname = \"test-fixer\"\nversion = \"0.1.0\"\ndescription = \"fixes tests\"\n\n[stages.main]\nmodel = { provider = \"anthropic\", model = \"claude-sonnet-4-6\" }\n",
        )
        .unwrap();
        let (path, entry) =
            resolve_worker_source(&cfg(None, None, Some("fixer")), "/p", Some(dir.path())).unwrap();
        assert!(path.contains("test-fixer"));
        assert_eq!(entry, None);

        // A worker_query with no match propagates discover_worker's error.
        let empty = tempfile::tempdir().unwrap();
        assert!(
            resolve_worker_source(&cfg(None, None, Some("zzz")), "/p", Some(empty.path())).is_err()
        );
    }

    #[test]
    fn resolve_worker_source_errors_without_a_source() {
        let empty = cfg(None, None, None);
        assert!(resolve_worker_source(&empty, "/p", None).is_err());
    }

    #[test]
    fn discover_worker_matches_name_or_description_and_reports_misses() {
        let dir = tempfile::tempdir().unwrap();
        // An agent whose description (not name) matches.
        let a = dir.path().join("alpha");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::write(
            a.join("agent.leviath"),
            "[agent]\nname = \"alpha\"\nversion = \"0.1.0\"\ndescription = \"a widget wrangler\"\n\n[stages.main]\nmodel = { provider = \"anthropic\", model = \"claude-sonnet-4-6\" }\n",
        )
        .unwrap();
        // A directory without a manifest is skipped.
        std::fs::create_dir_all(dir.path().join("not-an-agent")).unwrap();

        // Matches by description.
        assert!(
            discover_worker(Some(dir.path()), "widget")
                .unwrap()
                .ends_with("alpha")
        );
        // Matches by directory name.
        assert!(
            discover_worker(Some(dir.path()), "ALPHA")
                .unwrap()
                .ends_with("alpha")
        );
        // No match.
        assert!(discover_worker(Some(dir.path()), "nonexistent").is_err());
        // No agents dir.
        assert!(discover_worker(None, "x").is_err());
        // Unreadable dir (path is a file).
        let file = dir.path().join("alpha").join("agent.leviath");
        assert!(discover_worker(Some(&file), "x").is_err());
    }

    // ── spawn_worker (integration over build_agent) ───────────────────────────

    use leviath_runtime::components::AgentStatus;
    use leviath_runtime::host::SpawnArgs;
    use leviath_runtime::inference_pool::InferencePoolConfig;
    use leviath_runtime::pipeline::StageCursor;
    use leviath_runtime::world::PipelineWorld;
    use std::collections::HashMap;
    use tokio::runtime::Handle;

    struct FakeProvider;
    #[async_trait::async_trait]
    impl leviath_providers::Provider for FakeProvider {
        async fn infer(
            &self,
            _r: &leviath_providers::InferenceRequest,
        ) -> leviath_providers::Result<leviath_providers::InferenceResponse> {
            Err(leviath_providers::ProviderError::Other("test".to_string()))
        }
        async fn count_tokens(&self, _t: &str, _m: &str) -> usize {
            1
        }
        fn max_context_tokens(&self, _m: &str) -> usize {
            100_000
        }
        fn name(&self) -> &str {
            "fake"
        }
        fn capabilities(&self, _m: &str) -> leviath_providers::ModelCapabilities {
            leviath_providers::ModelCapabilities::default()
        }
    }

    /// A two-stage blueprint whose second stage opts in as a fan-out worker.
    fn two_stage_manifest() -> String {
        "[agent]\nname = \"host\"\nversion = \"0.1.0\"\ndescription = \"d\"\nentry_stage = \"first\"\n\n\
         [stages.first]\nmodel = { provider = \"anthropic\", model = \"m\" }\nsystem_prompt = \"first\"\n\n\
         [stages.second]\nmodel = { provider = \"anthropic\", model = \"m\" }\nallow_as_worker = true\nsystem_prompt = \"second\"\n"
            .to_string()
    }

    fn spawner_with(tool_service: Arc<CliToolService>) -> DaemonFanOutSpawner {
        let shared_mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        DaemonFanOutSpawner {
            config: Arc::new(crate::daemon::config_reload::ConfigReloader::fixed(
                Config::default(),
            )),
            shared_mcp: shared_mcp.clone(),
            mcp_tool_defs: vec![],
            mcp_pool: crate::daemon::mcp_pool::McpPool::for_daemon(shared_mcp, &[]),
            hub: InteractionHub::new(),
            subagent_tx: tokio::sync::mpsc::unbounded_channel().0,
            tool_service,
            agents_dir: None,
            now_secs: || 100,
        }
    }

    #[tokio::test]
    async fn worker_mcp_defs_advertises_cached_servers_and_falls_back_to_global() {
        let mut spawner = spawner_with(Arc::new(CliToolService::new()));
        spawner.mcp_tool_defs = vec![Tool {
            name: "global_tool".to_string(),
            description: String::new(),
            parameters: serde_json::json!({}),
        }];
        // A worker blueprint declaring an MCP server.
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(
            &manifest,
            "[agent]\nname = \"w\"\n\n[[mcp_servers]]\nname = \"srv\"\ncommand = \"python3\"\nargs = [\"pass\"]\n",
        )
        .unwrap();
        // Seed the pool so the server's tool is already advertised (and the
        // detached warm task hits the cache - no real connection).
        let servers = crate::daemon::mcp_pool::parse_blueprint_mcp_servers(
            &std::fs::read_to_string(&manifest).unwrap(),
        );
        spawner.mcp_pool.seed(
            &servers[0],
            vec![Tool {
                name: "srv_tool".to_string(),
                description: String::new(),
                parameters: serde_json::json!({}),
            }],
        );
        let defs = spawner.worker_mcp_defs(&manifest.to_string_lossy());
        let names: Vec<&str> = defs.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["global_tool", "srv_tool"]);
        // An unreadable manifest → just the global defs (read-error arm).
        let only_global = spawner.worker_mcp_defs("/no/such/agent.leviath");
        assert_eq!(only_global.len(), 1);
        assert_eq!(only_global[0].name, "global_tool");
        // A manifest with no [[mcp_servers]] → just the global defs.
        let empty = dir.path().join("empty.leviath");
        std::fs::write(&empty, "[agent]\nname = \"e\"\n").unwrap();
        assert_eq!(spawner.worker_mcp_defs(&empty.to_string_lossy()).len(), 1);
    }

    /// Build a world with a live parent agent (from `manifest`) and return the
    /// world, the spawner, and the parent entity.
    fn world_with_parent(manifest_path: &str) -> (PipelineWorld, DaemonFanOutSpawner, Entity) {
        world_with_parent_yolo(manifest_path, false)
    }

    fn world_with_parent_yolo(
        manifest_path: &str,
        yolo: bool,
    ) -> (PipelineWorld, DaemonFanOutSpawner, Entity) {
        let cli = Arc::new(CliToolService::new());
        let mut registry = leviath_runtime::ProviderRegistry::new();
        registry.register("anthropic".to_string(), Arc::new(FakeProvider));
        let mut world = PipelineWorld::new(
            registry,
            cli.clone(),
            InferencePoolConfig::new(),
            1,
            None,
            Handle::current(),
        );
        let spawner = spawner_with(cli.clone());
        let args = SpawnArgs {
            run_id: "parent".to_string(),
            blueprint_path: manifest_path.to_string(),
            task: "parent task".to_string(),
            regions: HashMap::new(),
            model: None,
            workdir: std::env::temp_dir().to_string_lossy().to_string(),
            metadata: HashMap::new(),
            callback_url: None,
            callback_secret: None,
            yolo,
            no_seed_commands: false,
            allow: Vec::new(),
            max_depth: None,
            parent_run_id: None,
            output: None,
        };
        let parent = build_agent(
            world.world_mut(),
            SpawnDeps {
                tool_service: cli.as_ref(),
                config: &spawner.config.current(),
                shared_mcp: spawner.shared_mcp.clone(),
                mcp_tool_defs: &spawner.mcp_tool_defs,
                hub: &spawner.hub,
                now_secs: 100,
                subagent_tx: spawner.subagent_tx.clone(),
            },
            &args,
        )
        .expect("parent spawns");
        (world, spawner, parent)
    }

    #[tokio::test]
    async fn spawn_worker_worker_stage_enters_the_worker_stage() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(&manifest, two_stage_manifest()).unwrap();
        let (mut world, spawner, parent) = world_with_parent(&manifest.to_string_lossy());

        let child = spawner
            .spawn_worker(
                world.world_mut(),
                parent,
                &cfg(Some("second"), None, None),
                "item-1",
                &serde_json::json!({"k": "v"}),
            )
            .expect("worker spawns");
        // The worker entered the `second` stage (index 1).
        assert_eq!(world.world().get::<StageCursor>(child).unwrap().index, 1);
        assert_eq!(world.agent_status(child), Some(AgentStatus::Active));
    }

    /// A worker of an unattended parent is unattended. Spawning workers attended
    /// under a `--yolo` parent left them stopping on approval prompts nobody was
    /// watching for, with the parent parked behind them (issue #184).
    #[tokio::test]
    async fn spawn_worker_inherits_the_parents_unattended_setting() {
        for unattended in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let manifest = dir.path().join("agent.leviath");
            std::fs::write(&manifest, two_stage_manifest()).unwrap();
            let (mut world, spawner, parent) =
                world_with_parent_yolo(&manifest.to_string_lossy(), unattended);

            let child = spawner
                .spawn_worker(
                    world.world_mut(),
                    parent,
                    &cfg(Some("second"), None, None),
                    "item-1",
                    &serde_json::json!({"k": "v"}),
                )
                .expect("worker spawns");

            assert_eq!(
                world
                    .world()
                    .get::<RunMetadata>(child)
                    .expect("worker has run metadata")
                    .unattended,
                unattended
            );
        }
    }

    #[tokio::test]
    async fn spawn_worker_worker_agent_uses_a_separate_blueprint() {
        let dir = tempfile::tempdir().unwrap();
        let parent_m = dir.path().join("agent.leviath");
        std::fs::write(&parent_m, two_stage_manifest()).unwrap();
        let (mut world, spawner, parent) = world_with_parent(&parent_m.to_string_lossy());

        // worker_agent given as a directory containing agent.leviath.
        let worker_dir = dir.path().join("worker");
        std::fs::create_dir_all(&worker_dir).unwrap();
        std::fs::write(worker_dir.join("agent.leviath"), two_stage_manifest()).unwrap();
        let child = spawner
            .spawn_worker(
                world.world_mut(),
                parent,
                &cfg(None, Some(&worker_dir.to_string_lossy()), None),
                "item-1",
                &serde_json::json!({}),
            )
            .expect("worker spawns");
        // A separate blueprint enters at its own entry stage (index 0).
        assert_eq!(world.world().get::<StageCursor>(child).unwrap().index, 0);
    }

    #[tokio::test]
    async fn spawn_worker_errors_without_parent_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(&manifest, two_stage_manifest()).unwrap();
        let (mut world, spawner, _parent) = world_with_parent(&manifest.to_string_lossy());
        // A bare entity with no RunMetadata.
        let bare = world.world_mut().spawn_empty().id();
        let err = spawner
            .spawn_worker(
                world.world_mut(),
                bare,
                &cfg(Some("second"), None, None),
                "i",
                &serde_json::json!({}),
            )
            .unwrap_err();
        assert!(err.contains("no run metadata"));
    }

    #[tokio::test]
    async fn spawn_worker_errors_when_worker_stage_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(&manifest, two_stage_manifest()).unwrap();
        let (mut world, spawner, parent) = world_with_parent(&manifest.to_string_lossy());
        let err = spawner
            .spawn_worker(
                world.world_mut(),
                parent,
                &cfg(Some("ghost"), None, None),
                "i",
                &serde_json::json!({}),
            )
            .unwrap_err();
        assert!(err.contains("ghost"));
    }

    #[tokio::test]
    async fn spawn_worker_propagates_a_build_error() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(&manifest, two_stage_manifest()).unwrap();
        let (mut world, spawner, parent) = world_with_parent(&manifest.to_string_lossy());

        // A worker blueprint that parses but fails validation (transition to a
        // stage that doesn't exist) - resolve succeeds, build_agent errors.
        let bad_dir = dir.path().join("bad");
        std::fs::create_dir_all(&bad_dir).unwrap();
        std::fs::write(
            bad_dir.join("agent.leviath"),
            "[agent]\nname = \"bad\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n\
             [stages.only]\nmodel = { provider = \"anthropic\", model = \"m\" }\n\n\
             [stages.only.transitions.nowhere]\n",
        )
        .unwrap();
        let err = spawner
            .spawn_worker(
                world.world_mut(),
                parent,
                &cfg(None, Some(&bad_dir.to_string_lossy()), None),
                "i",
                &serde_json::json!({}),
            )
            .unwrap_err();
        assert!(err.contains("invalid blueprint"));
    }

    #[tokio::test]
    async fn spawn_worker_propagates_a_worker_source_error() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(&manifest, two_stage_manifest()).unwrap();
        let (mut world, spawner, parent) = world_with_parent(&manifest.to_string_lossy());
        // A config with no worker source ⇒ resolve_worker_source errors.
        let err = spawner
            .spawn_worker(
                world.world_mut(),
                parent,
                &cfg(None, None, None),
                "i",
                &serde_json::json!({}),
            )
            .unwrap_err();
        assert!(err.contains("no worker source"));
    }

    #[tokio::test]
    async fn fake_provider_metadata_is_exercised() {
        use leviath_providers::Provider;
        let p = FakeProvider;
        assert_eq!(p.name(), "fake");
        assert_eq!(p.count_tokens("t", "m").await, 1);
        assert_eq!(p.max_context_tokens("m"), 100_000);
        let _ = p.capabilities("m");
    }

    #[tokio::test]
    async fn fake_provider_infer_errors() {
        use leviath_providers::Provider;
        let p = FakeProvider;
        assert!(
            p.infer(&leviath_providers::InferenceRequest {
                system: vec![],
                messages: vec![],
                model: "m".to_string(),
                max_tokens: 1,
                temperature: 0.0,
                tools: vec![],
                extra: serde_json::Value::Null,
                request_timeout_secs: None,
            })
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn spawn_worker_propagates_a_resolve_error() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(&manifest, two_stage_manifest()).unwrap();
        let (mut world, spawner, parent) = world_with_parent(&manifest.to_string_lossy());
        // worker_agent pointing at a nonexistent blueprint.
        let err = spawner
            .spawn_worker(
                world.world_mut(),
                parent,
                &cfg(None, Some("/no/such/agent/xyz"), None),
                "i",
                &serde_json::json!({}),
            )
            .unwrap_err();
        assert!(err.contains("resolve worker blueprint"));
    }

    #[test]
    fn discover_worker_skips_agents_with_unparsable_manifests() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("broken");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("agent.leviath"), "this is not valid toml : : :").unwrap();
        // Name doesn't match and the manifest won't parse → skipped → miss.
        assert!(discover_worker(Some(dir.path()), "zzz").is_err());
        // But the directory name still matches even when the manifest is broken.
        assert!(
            discover_worker(Some(dir.path()), "broken")
                .unwrap()
                .ends_with("broken")
        );
    }
}
