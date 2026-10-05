//! The bundled coder blueprint, resolved, bound and inserted over a
//! [`DaemonEnv`]: the run lands with everything the daemon's own systems read.

use std::collections::HashMap;

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

#[tokio::test(flavor = "multi_thread")]
async fn the_coder_lands_with_what_the_daemons_systems_read() {
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
            let tools = Arc::new(CliToolService::new());
            let mut pipeline = PipelineWorld::new(
                registry(),
                tools.clone(),
                InferencePoolConfig::new(),
                1,
                None,
                tokio::runtime::Handle::current(),
            );
            let env = DaemonEnv {
                config: Arc::new(config.clone()),
                registry: registry(),
                agents_dir: Some(agents.clone()),
                workdir_root: None,
                mcp_defs: Vec::new(),
                mcp_owners: Default::default(),
                shared_mcp: Arc::new(tokio::sync::Mutex::new(leviath_mcp::ToolExecutor::new())),
                tool_service: tools.clone(),
                hub: InteractionHub::new(),
                subagent_tx: tokio::sync::mpsc::unbounded_channel().0,
                mime: Arc::new(leviath_core::mime::MimeRegistry::builtin()),
                blob_store: Arc::new(leviath_core::mime::MemoryBlobStore::new()),
                mcp_overrides: HashMap::new(),
                secrets: None,
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
            let entity =
                leviath_runtime::insert::insert(pipeline.world_mut(), spec, bindings, &state);

            let world = pipeline.world();
            assert!(world.get::<leviath_runtime::TaintGate>(entity).is_some());
            assert!(
                world
                    .get::<leviath_runtime::components::GateAutoApprove>(entity)
                    .is_some()
            );
            assert!(
                world
                    .get::<leviath_runtime::components::InteractionAutoApprove>(entity)
                    .is_some()
            );
            assert!(
                world
                    .get::<leviath_runtime::title::TitleCandidates>(entity)
                    .is_some()
            );
            assert!(
                world
                    .get::<leviath_runtime::blob_store::RunMimeRegistry>(entity)
                    .is_some()
            );
            assert!(tools.state_for(entity).is_some());
        },
    )
    .await;
}
