//! The bundled coder blueprint, spawned both ways: through the daemon's
//! spawner, and through resolve, bind and insert over a [`DaemonEnv`]. The two
//! entities must carry the same components, apart from the three the
//! spawner still places for the modules that read a parsed blueprint.

use std::any::TypeId;
use std::collections::{BTreeSet, HashMap};

use bevy_ecs::entity::Entity;
use bevy_ecs::world::World;
use leviath_runtime::host::SpawnArgs;
use leviath_runtime::inference_pool::InferencePoolConfig;
use leviath_runtime::resolve::{ResolveMode, resolve};
use leviath_runtime::spec::env::Caller;
use leviath_runtime::spec::inputs::RawInput;
use leviath_runtime::spec::request::{SpawnRequest, SpawnSource};
use leviath_runtime::world::PipelineWorld;

use super::*;
use crate::test_support::FakeProvider;

fn registry() -> leviath_runtime::ProviderRegistry {
    let mut r = leviath_runtime::ProviderRegistry::new();
    for p in ["anthropic", "openai", "ollama"] {
        r.register(
            p.to_string(),
            Arc::new(FakeProvider::new().context_window(200_000)),
        );
    }
    r
}

fn pipeline(tools: Arc<CliToolService>) -> PipelineWorld {
    PipelineWorld::new(
        registry(),
        tools,
        InferencePoolConfig::new(),
        1,
        None,
        tokio::runtime::Handle::current(),
    )
}

/// Every component type on `entity`.
fn components(world: &World, entity: Entity) -> BTreeSet<TypeId> {
    world
        .inspect_entity(entity)
        .unwrap()
        .filter_map(|info| info.type_id())
        .collect()
}

macro_rules! named {
    ($($t:ty),* $(,)?) => {
        vec![$((TypeId::of::<$t>(), std::any::type_name::<$t>())),*]
    };
}

/// A name for every component either path places, for a readable failure.
fn names() -> Vec<(TypeId, &'static str)> {
    use leviath_runtime::components as c;
    use leviath_runtime::pipeline as p;
    named![
        leviath_runtime::insert::RunSpecC,
        c::AgentState,
        c::MessageInbox,
        c::ContextWindow,
        c::InteractionAutoApprove,
        c::GateAutoApprove,
        c::OutputValidators,
        c::StageHookScripts,
        p::StageCursor,
        p::StageInference,
        p::StageLedger,
        p::PersistWatermark,
        p::ReadyToInfer,
        p::CompactionSettings,
        p::CaptureModelInput,
        p::DynamicTools,
        p::RescanBeforeDispatch,
        p::ToolSensitivities,
        p::AgentBlueprint,
        leviath_runtime::persistence::RunMetadata,
        leviath_runtime::persistence::TokenTotals,
        leviath_runtime::persistence::RunClock,
        leviath_runtime::persistence::RunOutcomeFlags,
        leviath_runtime::TaintGate,
        leviath_runtime::title::PendingTitle,
        leviath_runtime::title::TitleCandidates,
        leviath_runtime::blob_store::RunMimeRegistry,
        leviath_runtime::bind::RegionScripts,
    ]
}

fn spelled(ids: &BTreeSet<TypeId>) -> Vec<String> {
    let names: HashMap<TypeId, &str> = names().into_iter().collect();
    let mut out: Vec<String> = ids
        .iter()
        .map(|id| {
            names
                .get(id)
                .map_or_else(|| format!("{id:?}"), |n| n.to_string())
        })
        .collect();
    out.sort();
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn the_coder_lands_with_the_same_components_both_ways() {
    let home = tempfile::tempdir().unwrap();
    let agents = home.path().join("agents");
    let coder = crate::bundled::BUNDLED_AGENTS
        .iter()
        .find(|a| a.name == "coder")
        .expect("coder is bundled");
    crate::bundled::install_bundled(coder, &agents).unwrap();
    let work = tempfile::tempdir().unwrap();
    let workdir = std::fs::canonicalize(work.path()).unwrap();
    let config = Config {
        taint_tracking: true,
        ..Config::default()
    };

    temp_env::async_with_vars(
        [
            ("LEVIATH_HOME", Some(home.path().as_os_str())),
            ("LEVIATH_CONFIG_PATH", None),
        ],
        async {
            // Through the spawner.
            let old_tools = Arc::new(CliToolService::new());
            let mut old = pipeline(old_tools.clone());
            let hub = InteractionHub::new();
            let args = SpawnArgs {
                run_id: "coder-old".into(),
                blueprint_path: agents
                    .join("coder")
                    .join(leviath_core::files::MANIFEST_FILENAME)
                    .to_string_lossy()
                    .into_owned(),
                task: "fix the bug".into(),
                regions: HashMap::new(),
                parts: Vec::new(),
                model: None,
                workdir: workdir.to_string_lossy().into_owned(),
                metadata: HashMap::new(),
                callback_url: None,
                callback_secret: None,
                yolo: true,
                yolo_profile: None,
                no_seed_commands: true,
                allow: Vec::new(),
                max_depth: None,
                parent_run_id: None,
                worker_stage: None,
                output: None,
                capture_model_input: false,
            };
            let old_entity = crate::daemon::spawn::build_agent(
                old.world_mut(),
                crate::daemon::spawn::SpawnDeps {
                    tool_service: old_tools.as_ref(),
                    config: &config,
                    shared_mcp: Arc::new(tokio::sync::Mutex::new(leviath_mcp::ToolExecutor::new())),
                    mcp_tool_defs: &[],
                    mcp_tool_owners: &Default::default(),
                    hub: &hub,
                    now_secs: 1,
                    subagent_tx: tokio::sync::mpsc::unbounded_channel().0,
                },
                &args,
            )
            .unwrap();

            // Through resolve, bind and insert.
            let new_tools = Arc::new(CliToolService::new());
            let mut new = pipeline(new_tools.clone());
            let env = DaemonEnv {
                config: Arc::new(config.clone()),
                registry: registry(),
                agents_dir: Some(agents.clone()),
                workdir_root: None,
                mcp_defs: Vec::new(),
                mcp_owners: Default::default(),
                shared_mcp: Arc::new(tokio::sync::Mutex::new(leviath_mcp::ToolExecutor::new())),
                tool_service: new_tools.clone(),
                hub: InteractionHub::new(),
                subagent_tx: tokio::sync::mpsc::unbounded_channel().0,
                mime: Arc::new(leviath_core::mime::MimeRegistry::builtin()),
                blob_store: Arc::new(leviath_core::mime::MemoryBlobStore::new()),
                mcp_overrides: HashMap::new(),
            };
            let mut request = SpawnRequest::new(SpawnSource::Blueprint(
                BlueprintRef::parse("coder").unwrap(),
            ))
            .input("task", RawInput::Text("fix the bug".into()));
            request.workdir = Some(workdir.clone());
            request.launch.unattended = Unattended::All;
            request.launch.seed_commands = false;
            let resolved = resolve(&request, &Caller::TopLevel, &env, ResolveMode::Spawn)
                .await
                .unwrap();
            let bindings = leviath_runtime::bind::bind(&resolved.spec, &resolved.code, &env)
                .await
                .unwrap();
            let spec = Arc::new(resolved.spec);
            let state = leviath_runtime::insert::initial_state(&spec);
            let new_entity =
                leviath_runtime::insert::insert(new.world_mut(), spec, bindings, &state);

            let old_set = components(old.world_mut(), old_entity);
            let new_set = components(new.world_mut(), new_entity);
            assert_eq!(
                spelled(&new_set.difference(&old_set).copied().collect()),
                Vec::<String>::new(),
                "placed only by resolve, bind and insert"
            );
            // The spawner's own three: the parsed blueprint, and the per-stage
            // inference and setup lists (private to the runtime, so they are
            // counted rather than named), which only the modules still reading
            // a parsed blueprint use.
            let only_old: BTreeSet<TypeId> = old_set.difference(&new_set).copied().collect();
            assert_eq!(only_old.len(), 3, "{:?}", spelled(&only_old));
            assert!(
                only_old.contains(&TypeId::of::<leviath_runtime::pipeline::AgentBlueprint>()),
                "{:?}",
                spelled(&only_old)
            );
            for marker in [
                TypeId::of::<leviath_runtime::TaintGate>(),
                TypeId::of::<leviath_runtime::components::GateAutoApprove>(),
                TypeId::of::<leviath_runtime::components::InteractionAutoApprove>(),
                TypeId::of::<leviath_runtime::title::TitleCandidates>(),
                TypeId::of::<leviath_runtime::blob_store::RunMimeRegistry>(),
            ] {
                assert!(new_set.contains(&marker), "{:?}", spelled(&[marker].into()));
            }
            assert!(old_tools.state_for(old_entity).is_some());
            assert!(new_tools.state_for(new_entity).is_some());
        },
    )
    .await;
}
