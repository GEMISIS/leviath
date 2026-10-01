//! The daemon-side [`FanOutSpawner`]: starts a fan-out worker from the
//! request the runtime hands it, in the shared world.
//!
//! The runtime's fan-out systems only *start and track* workers. A worker is
//! started the way every run is, through the [`DaemonStarter`]: resolved,
//! recorded and bound as a [`Caller::Worker`] of its parent, then placed. It
//! is started from inside the world's tick, so the machine is not warmed for
//! it first: the parent's start already warmed what its fan-outs use, and any
//! MCP server a worker declares that is not connected yet is connected in the
//! background for the workers after it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bevy_ecs::entity::Entity;
use bevy_ecs::world::World;
use leviath_runtime::fanout::FanOutSpawner;
use leviath_runtime::spec::env::Caller;
use leviath_runtime::spec::names::BlueprintRef;
use leviath_runtime::spec::request::SpawnRequest;

use crate::daemon::starter::DaemonStarter;

/// What the fan-out systems start workers with.
#[derive(Clone)]
pub(crate) struct DaemonFanOutSpawner {
    /// What starts every run.
    pub starter: Arc<DaemonStarter>,
    /// `~/.leviath/agents`, for resolving a worker query. `None` when there
    /// is no home directory.
    pub agents_dir: Option<PathBuf>,
}

impl FanOutSpawner for DaemonFanOutSpawner {
    fn spawn_worker(
        &self,
        world: &mut World,
        _parent: Entity,
        request: SpawnRequest,
        caller: Caller,
    ) -> Result<Entity, String> {
        let config = self.starter.config.current();
        let env = self.starter.env_for(&request, config);
        // A server the worker's graph declares that is not connected yet is
        // connected for the next worker of the same kind.
        if let Some(graph) = self.starter.graph_of(&request) {
            let servers = crate::daemon::starter::mcp_configs(&graph);
            let uncached: Vec<_> = servers
                .into_iter()
                .filter(|s| {
                    self.starter
                        .mcp_pool
                        .cached_defs_for(std::slice::from_ref(s))
                        .is_empty()
                })
                .collect();
            if !uncached.is_empty() {
                tokio::runtime::Handle::current()
                    .spawn(self.starter.mcp_pool.clone().ensure_all(uncached));
            }
        }
        let prepared =
            crate::daemon::block_on::block_on(self.starter.start_with(env, request, caller))
                .map_err(|issues| issues.to_string())?;
        Ok(leviath_runtime::insert::insert(
            world,
            prepared.spec,
            prepared.bindings,
            &prepared.state,
        ))
    }

    fn find_worker(&self, query: &str) -> Result<BlueprintRef, String> {
        find_installed(self.agents_dir.as_deref(), query)
    }
}

/// The installed blueprint [`discover_worker`] finds for `query`, by name.
pub(crate) fn find_installed(
    agents_dir: Option<&Path>,
    query: &str,
) -> Result<BlueprintRef, String> {
    let path = discover_worker(agents_dir, query)?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    BlueprintRef::parse(&name).map_err(|e| e.to_string())
}

/// Find an installed agent whose directory name or manifest description contains
/// `query` (case-insensitive). Returns the agent's directory.
pub(crate) fn discover_worker(agents_dir: Option<&Path>, query: &str) -> Result<PathBuf, String> {
    let dir = agents_dir.ok_or_else(|| "no agents directory to search for a worker".to_string())?;
    let needle = query.to_lowercase();
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("read agents dir '{}': {e}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let manifest = path.join(leviath_core::files::MANIFEST_FILENAME);
        if !manifest.is_file() {
            continue;
        }
        let name_matches = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.to_lowercase().contains(&needle));
        let desc_matches = std::fs::read_to_string(&manifest)
            .ok()
            .and_then(|c| leviath_runtime::spec::manifest::parse_manifest(&c).ok())
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
    use crate::daemon::starter::testing::{TestLaunch, request_for, starter};
    use crate::daemon::tool_service::CliToolService;
    use crate::test_support::{FakeProvider, fixtures};
    use leviath_runtime::components::AgentStatus;
    use leviath_runtime::inference_pool::InferencePoolConfig;
    use leviath_runtime::insert::RunSpecC;
    use leviath_runtime::persistence::RunMetadata;
    use leviath_runtime::pipeline::StageCursor;
    use leviath_runtime::spec::inputs::RawInput;
    use leviath_runtime::spec::launch::LaunchRequest;
    use leviath_runtime::spec::request::SpawnSource;
    use leviath_runtime::world::PipelineWorld;
    use std::collections::HashMap;
    use tokio::runtime::Handle;

    /// Fails every inference; the window is wide enough for the budgets
    /// the manifests here declare.
    fn fake_provider() -> FakeProvider {
        FakeProvider::new().context_window(100_000)
    }

    /// A two-stage blueprint whose second stage opts in as a fan-out worker.
    fn two_stage_manifest() -> String {
        "[agent]\nname = \"host\"\nversion = \"0.1.0\"\ndescription = \"d\"\nentry_stage = \"first\"\n\n\
         [stages.first]\nmodel = { provider = \"anthropic\", model = \"m\" }\nsystem_prompt = \"first\"\n\n\
         [stages.second]\nmodel = { provider = \"anthropic\", model = \"m\" }\nallow_as_worker = true\nsystem_prompt = \"second\"\n\n\
         [context.regions]\ntask = { kind = \"pinned\", max_tokens = 2000 }\n"
            .to_string()
    }

    /// A spawner whose runs are kept under `runs_dir`, registering tools
    /// with `tool_service` and calling the fake `anthropic` provider.
    fn spawner_with(tool_service: Arc<CliToolService>, runs_dir: &Path) -> DaemonFanOutSpawner {
        let mut started = starter(Config::default(), registry(), runs_dir);
        started.tool_service = tool_service;
        started.agents_dir = None;
        DaemonFanOutSpawner {
            starter: Arc::new(started),
            agents_dir: None,
        }
    }

    /// The providers every run here calls.
    fn registry() -> leviath_runtime::ProviderRegistry {
        let mut registry = leviath_runtime::ProviderRegistry::new();
        registry.register("anthropic".to_string(), Arc::new(fake_provider()));
        registry
    }

    /// What a fan-out hands its spawner for a worker of `parent`: a worker
    /// entering `stage` of the parent's own blueprint, or one running
    /// `agent`, with the parent's model, workdir and unattended setting.
    fn worker_of(
        world: &PipelineWorld,
        parent: Entity,
        stage: Option<&str>,
        agent: Option<&str>,
    ) -> (SpawnRequest, Caller) {
        let spec = world
            .world()
            .get::<RunSpecC>(parent)
            .expect("the parent has a run spec")
            .0
            .clone();
        let source = agent.map_or_else(
            || spec.same_graph_source(),
            |agent| {
                SpawnSource::BlueprintFile(
                    leviath_runtime::spec::names::BlueprintPath::new(agent).unwrap(),
                )
            },
        );
        let request = SpawnRequest {
            model: spec.requested_model.clone(),
            output: spec.requested_output.clone(),
            workdir: Some(spec.placement.workdir.clone()),
            launch: LaunchRequest {
                unattended: spec.launch.unattended.clone(),
                seed_commands: false,
                ..LaunchRequest::default()
            },
            ..SpawnRequest::new(source)
        }
        .input("task", RawInput::Text("Work item id: item-1".into()));
        let caller = Caller::Worker {
            parent: spec.run_id.clone(),
            policy: spec.launch.clone(),
            depth: 0,
            stage: stage.map(|s| leviath_runtime::spec::names::StageName::new(s).unwrap()),
        };
        (request, caller)
    }

    /// A world holding a live parent started from `launch`, the spawner
    /// that started it, and the parent. Runs are kept beside the manifest.
    fn world_with(launch: TestLaunch) -> (PipelineWorld, DaemonFanOutSpawner, Entity) {
        let cli = Arc::new(CliToolService::new());
        let mut world = PipelineWorld::new(
            registry(),
            cli.clone(),
            InferencePoolConfig::new(),
            1,
            None,
            Handle::current(),
        );
        let runs_dir = Path::new(&launch.blueprint_path)
            .parent()
            .expect("the manifest is in a directory")
            .join("runs");
        let spawner = spawner_with(cli, &runs_dir);
        let request = request_for(&launch).expect("the parent's request reads");
        let env = spawner
            .starter
            .env_for(&request, Arc::new(Config::default()));
        let prepared = crate::daemon::block_on::block_on(spawner.starter.start_with(
            env,
            request,
            Caller::TopLevel,
        ))
        .expect("the parent starts");
        let parent = leviath_runtime::insert::insert(
            world.world_mut(),
            prepared.spec,
            prepared.bindings,
            &prepared.state,
        );
        (world, spawner, parent)
    }

    /// The parent's launch: `manifest_path` given a task.
    fn launch(manifest_path: &str) -> TestLaunch {
        TestLaunch {
            blueprint_path: manifest_path.to_string(),
            task: "parent task".to_string(),
            ..TestLaunch::default()
        }
    }

    /// [`world_with`] for a plain parent of `manifest_path`.
    fn world_with_parent(manifest_path: &str) -> (PipelineWorld, DaemonFanOutSpawner, Entity) {
        world_with(launch(manifest_path))
    }

    /// The two-stage manifest written into `dir`, as a path.
    fn two_stage_in(dir: &Path) -> String {
        crate::daemon::starter::testing::manifest_in(dir, &two_stage_manifest())
            .to_string_lossy()
            .into_owned()
    }

    /// Start a worker of `parent` and hand back the entity it was placed as.
    fn spawn(
        world: &mut PipelineWorld,
        spawner: &DaemonFanOutSpawner,
        parent: Entity,
        stage: Option<&str>,
        agent: Option<&str>,
    ) -> Result<Entity, String> {
        let (request, caller) = worker_of(world, parent, stage, agent);
        spawner.spawn_worker(world.world_mut(), parent, request, caller)
    }

    #[test]
    fn find_worker_discovers_an_agent_by_query() {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path().join("test-fixer");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(
            agent.join("agent.leviath"),
            "[agent]\nname = \"test-fixer\"\nversion = \"0.1.0\"\ndescription = \"fixes tests\"\n\n[stages.main]\nmodel = { provider = \"anthropic\", model = \"claude-sonnet-4-6\" }\n",
        )
        .unwrap();
        let mut spawner = spawner_with(Arc::new(CliToolService::new()), dir.path());
        spawner.agents_dir = Some(dir.path().to_path_buf());
        let found = spawner.find_worker("fixer").unwrap();
        assert!(found.name.as_str().contains("test-fixer"));

        // A query with no match propagates discover_worker's error.
        let empty = tempfile::tempdir().unwrap();
        spawner.agents_dir = Some(empty.path().to_path_buf());
        assert!(spawner.find_worker("zzz").is_err());
        // A path that does not read as a blueprint reference is refused.
        let odd = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(odd.path().join("x@fixer")).unwrap();
        std::fs::write(
            odd.path().join("x@fixer").join("agent.leviath"),
            "[agent]\nname = \"x\"\n",
        )
        .unwrap();
        spawner.agents_dir = Some(odd.path().to_path_buf());
        assert!(spawner.find_worker("fixer").is_err());
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

    #[tokio::test]
    async fn spawn_worker_worker_stage_enters_the_worker_stage() {
        let dir = tempfile::tempdir().unwrap();
        let (mut world, spawner, parent) = world_with_parent(&two_stage_in(dir.path()));

        let child =
            spawn(&mut world, &spawner, parent, Some("second"), None).expect("worker spawns");
        // The worker entered the `second` stage (index 1).
        assert_eq!(world.world().get::<StageCursor>(child).unwrap().index, 1);
        assert_eq!(
            world.agent_status(world.own_agent(child)),
            Some(AgentStatus::Active)
        );
        // It was recorded like every run, in its own run file.
        let id = world
            .world()
            .get::<RunSpecC>(child)
            .unwrap()
            .0
            .run_id
            .clone();
        assert!(
            spawner
                .starter
                .runs_dir
                .join(id.as_str())
                .join(leviath_core::files::RUN_FILE)
                .is_file()
        );
    }

    /// The parent was started with a `--diff` its blueprint requires; its
    /// workers get their share of that diff inside the work item, so the
    /// requirement is not put to them again. This is the bundled reviewer's
    /// shape, whose workers were refused at spawn every run.
    #[tokio::test]
    async fn spawn_worker_is_not_held_to_the_parents_required_caller_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let requiring = two_stage_manifest().replace(
            "[context.regions]\n",
            "[context.regions]\ndiff = { kind = \"pinned\", max_tokens = 2000, seed = \"diff\", required = true }\n",
        );
        assert!(
            requiring.contains("seed = \"diff\""),
            "fixture gained the region"
        );
        let manifest = crate::daemon::starter::testing::manifest_in(dir.path(), &requiring);
        let (mut world, spawner, parent) = world_with(TestLaunch {
            regions: HashMap::from([("diff".to_string(), "- a\n+ b".to_string())]),
            ..launch(&manifest.to_string_lossy())
        });

        let child = spawn(&mut world, &spawner, parent, Some("second"), None)
            .expect("a worker is spawned without the parent's --diff");
        assert_eq!(world.world().get::<StageCursor>(child).unwrap().index, 1);
    }

    /// A worker of an unattended parent is unattended. An attended worker under
    /// a `--yolo` parent stops on an approval prompt nobody is watching for,
    /// and parks the parent behind it.
    #[tokio::test]
    async fn spawn_worker_inherits_the_parents_unattended_setting() {
        for unattended in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let (mut world, spawner, parent) = world_with(TestLaunch {
                yolo: unattended,
                ..launch(&two_stage_in(dir.path()))
            });

            let child =
                spawn(&mut world, &spawner, parent, Some("second"), None).expect("worker spawns");

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

    /// A run-level `--model` covers the whole run, workers included: stopping
    /// at the run boundary would send most of a thirty-worker fan-out's spend
    /// to whatever the worker blueprint lists, against an instruction typed for
    /// this run.
    ///
    /// Asserts the worker's *resolved stage model*, not just the field: what
    /// matters is which model the worker actually calls.
    #[tokio::test]
    async fn spawn_worker_inherits_the_parents_model_override() {
        let dir = tempfile::tempdir().unwrap();
        // The blueprint says `anthropic/m` at every stage; the run says `m2`.
        let (mut world, spawner, parent) = world_with(TestLaunch {
            model: Some("anthropic/m2".to_string()),
            ..launch(&two_stage_in(dir.path()))
        });

        let child =
            spawn(&mut world, &spawner, parent, Some("second"), None).expect("worker spawns");

        let inference = world
            .world()
            .get::<leviath_runtime::pipeline::StageInference>(child)
            .expect("the worker resolved a stage model");
        assert_eq!(
            inference.model, "m2",
            "the worker runs the overridden model"
        );
        assert_eq!(
            world
                .world()
                .get::<RunMetadata>(child)
                .expect("worker has run metadata")
                .model_override
                .as_deref(),
            Some("anthropic/m2"),
            "and carries it on, so its own children inherit it too"
        );
    }

    /// No override on the run leaves every worker resolving against its own
    /// blueprint.
    #[tokio::test]
    async fn spawn_worker_without_an_override_uses_the_blueprints_model() {
        let dir = tempfile::tempdir().unwrap();
        let (mut world, spawner, parent) = world_with_parent(&two_stage_in(dir.path()));

        let child =
            spawn(&mut world, &spawner, parent, Some("second"), None).expect("worker spawns");

        let inference = world
            .world()
            .get::<leviath_runtime::pipeline::StageInference>(child)
            .expect("the worker resolved a stage model");
        assert_eq!(inference.model, "m");
        assert!(
            world
                .world()
                .get::<RunMetadata>(child)
                .unwrap()
                .model_override
                .is_none()
        );
    }

    #[tokio::test]
    async fn spawn_worker_worker_agent_uses_a_separate_blueprint() {
        let dir = tempfile::tempdir().unwrap();
        let (mut world, spawner, parent) = world_with_parent(&two_stage_in(dir.path()));

        // worker_agent given as a directory containing agent.leviath.
        let worker_dir = dir.path().join("worker");
        std::fs::create_dir_all(&worker_dir).unwrap();
        two_stage_in(&worker_dir);
        let child = spawn(
            &mut world,
            &spawner,
            parent,
            None,
            Some(&worker_dir.to_string_lossy()),
        )
        .expect("worker spawns");
        // A separate blueprint enters at its own entry stage (index 0).
        assert_eq!(world.world().get::<StageCursor>(child).unwrap().index, 0);
    }

    #[tokio::test]
    async fn spawn_worker_errors_when_worker_stage_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let (mut world, spawner, parent) = world_with_parent(&two_stage_in(dir.path()));
        let err = spawn(&mut world, &spawner, parent, Some("ghost"), None).unwrap_err();
        assert!(err.contains("ghost"), "{err}");
    }

    #[tokio::test]
    async fn spawn_worker_propagates_an_invalid_worker_blueprint() {
        let dir = tempfile::tempdir().unwrap();
        let (mut world, spawner, parent) = world_with_parent(&two_stage_in(dir.path()));

        // A worker blueprint that parses but fails validation: a transition
        // to a stage that does not exist.
        let bad_dir = dir.path().join("bad");
        std::fs::create_dir_all(&bad_dir).unwrap();
        crate::daemon::starter::testing::manifest_in(
            &bad_dir,
            "[agent]\nname = \"bad\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n\
             [stages.only]\nmodel = { provider = \"anthropic\", model = \"m\" }\n\n\
             [stages.only.transitions.nowhere]\n\n\
             [context.regions]\ntask = { kind = \"pinned\", max_tokens = 500 }\n",
        );
        let err = spawn(
            &mut world,
            &spawner,
            parent,
            None,
            Some(&bad_dir.to_string_lossy()),
        )
        .unwrap_err();
        assert!(err.contains("nowhere"), "{err}");
    }

    #[tokio::test]
    async fn spawn_worker_propagates_a_missing_worker_blueprint() {
        let dir = tempfile::tempdir().unwrap();
        let (mut world, spawner, parent) = world_with_parent(&two_stage_in(dir.path()));
        assert!(
            spawn(
                &mut world,
                &spawner,
                parent,
                None,
                Some("/no/such/agent/xyz")
            )
            .is_err()
        );
    }

    /// A worker whose blueprint declares an MCP server gets the server's
    /// tools when the pool already has them, and starts without them while
    /// the pool connects to one it has not seen, for the workers after it.
    #[tokio::test]
    async fn a_workers_mcp_servers_are_connected_for_the_workers_after_it() {
        let dir = tempfile::tempdir().unwrap();
        let (mut world, spawner, parent) = world_with_parent(&two_stage_in(dir.path()));
        let worker_dir = dir.path().join("worker");
        std::fs::create_dir_all(&worker_dir).unwrap();
        crate::daemon::starter::testing::manifest_in(
            &worker_dir,
            &two_stage_manifest().replace(
                "[context.regions]\n",
                "[[mcp_servers]]\nname = \"srv\"\ncommand = \"/no/such/mcp-server\"\n\n[context.regions]\n",
            ),
        );
        let agent = worker_dir.to_string_lossy().into_owned();

        // Not connected yet: the worker starts, and a connection is begun.
        spawn(&mut world, &spawner, parent, None, Some(&agent)).expect("worker spawns");

        // Connected: the worker is started with the server's tools.
        let (request, _) = worker_of(&world, parent, None, Some(&agent));
        let graph = spawner
            .starter
            .graph_of(&request)
            .expect("the worker's graph");
        let servers = crate::daemon::starter::mcp_configs(&graph);
        spawner.starter.mcp_pool.seed(
            &servers[0],
            vec![leviath_providers::Tool {
                name: "srv__look".to_string(),
                description: String::new(),
                parameters: serde_json::json!({}),
            }],
        );
        spawn(&mut world, &spawner, parent, None, Some(&agent)).expect("worker spawns");
    }

    #[tokio::test]
    async fn fake_provider_metadata_is_exercised() {
        use leviath_providers::Provider;
        let p = fake_provider();
        assert_eq!(p.name(), "fake");
        assert_eq!(p.count_tokens("t", "m").await, 1);
        assert_eq!(p.max_context_tokens("m"), 100_000);
        let _ = p.capabilities("m");
    }

    #[tokio::test]
    async fn fake_provider_infer_errors() {
        use leviath_providers::Provider;
        let p = fake_provider();
        assert!(p.infer(&fixtures::inference_request()).await.is_err());
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

    /// A worker of a profiled parent runs under the same profile. Handing it
    /// only the bit would spawn it under bare `--yolo`, which is wider than
    /// what the person launched.
    #[tokio::test]
    async fn spawn_worker_inherits_the_parents_yolo_profile() {
        crate::config::with_isolated_config_path_async("fanout_yolo_profile", |home| async move {
            std::fs::write(home.join("yolo.toml"), "[careful]\ndefault = \"ask\"\n").unwrap();
            let dir = tempfile::tempdir().unwrap();
            let (mut world, spawner, parent) = world_with(TestLaunch {
                yolo: true,
                yolo_profile: Some("careful".to_string()),
                ..launch(&two_stage_in(dir.path()))
            });

            let child =
                spawn(&mut world, &spawner, parent, Some("second"), None).expect("worker spawns");
            let md = world
                .world()
                .get::<RunMetadata>(child)
                .expect("worker has run metadata");
            assert!(md.unattended);
            assert_eq!(md.yolo_profile.as_deref(), Some("careful"));
        })
        .await;
    }
}
