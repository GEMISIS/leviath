//! Daemon assembly: build a fully-wired [`WorldHost`] (world + tool service +
//! interaction hub + the blueprint spawner) ready to be driven by
//! [`WorldHost::serve`]. The async setup (provider registry, MCP connections)
//! happens in the binary and is passed in; this wiring is synchronous and
//! testable — spawning an agent through the installed spawner exercises the whole
//! path.

use std::sync::Arc;

use leviath_providers::Tool;
use leviath_runtime::ProviderRegistry;
use leviath_runtime::host::WorldHost;
use leviath_runtime::inference_pool::InferencePoolConfig;
use leviath_runtime::interaction_hub::InteractionHub;
use leviath_runtime::world::PipelineWorld;
use tokio::runtime::Handle;
use tokio::sync::Mutex;

use crate::commands::run::session::build_provider_registry_from_config;
use crate::config::{Config, leviath_home_dir};
use crate::daemon::spawn::build_agent;
use crate::daemon::tool_service::CliToolService;
use crate::tools::ToolRegistry;

/// The daemon's control-channel id, derived from `<leviath-home>/.leviath`
/// (honoring `LEVIATH_HOME`): a Unix-socket path on Unix, a named-pipe name on
/// Windows. `None` if no home directory can be resolved.
pub fn control_address() -> Option<leviath_runtime::control_socket::ControlId> {
    leviath_home_dir()
        .map(|home| leviath_runtime::control_socket::control_id(&home.join(".leviath")))
}

/// Build the daemon's [`WorldHost`], doing the async startup work: build the
/// provider registry from config and connect the shared MCP servers (both reused
/// by every agent), then wire the host + spawner via [`build_host`].
pub async fn setup_daemon_host(
    config: Config,
    runs_dir: std::path::PathBuf,
    runtime: Handle,
) -> WorldHost {
    let providers = build_provider_registry_from_config(&config);
    // MCP connections are shared across agents; the workdir here only seeds the
    // (discarded) built-ins — each agent gets its own over its own workdir.
    let registry = ToolRegistry::build(std::env::temp_dir(), &config).await;
    build_host(
        config,
        providers,
        runs_dir,
        registry.mcp,
        registry.mcp_tool_defs,
        runtime,
        || chrono::Utc::now().timestamp(),
    )
}

/// Build the daemon's [`WorldHost`]: one world hosting every agent, its tool
/// service + interaction hub, and a `Spawn`-op spawner that loads blueprints and
/// registers per-agent tool state. `shared_mcp` / `mcp_tool_defs` are the MCP
/// connections built once at startup and reused by every agent.
pub fn build_host(
    config: Config,
    providers: ProviderRegistry,
    runs_dir: std::path::PathBuf,
    shared_mcp: Arc<Mutex<leviath_mcp::ToolExecutor>>,
    mcp_tool_defs: Vec<Tool>,
    runtime: Handle,
    now_secs: fn() -> i64,
) -> WorldHost {
    let hub = InteractionHub::new();
    let tool_service = Arc::new(CliToolService::new());
    let world = PipelineWorld::new(
        providers,
        tool_service.clone(),
        InferencePoolConfig::new(),
        runs_dir.clone(),
        runtime,
    );
    let mut host = WorldHost::with_interactions(world, hub.clone());

    // Restart recovery: reload persisted non-terminal agents so interrupted runs
    // (including mid-inference ones) resume. Done before the spawner moves the
    // shared resources.
    let reloaded = crate::daemon::recovery::reload_persisted_agents(
        host.world_mut(),
        tool_service.as_ref(),
        &config,
        shared_mcp.clone(),
        &mcp_tool_defs,
        &hub,
        &runs_dir,
        now_secs(),
    );
    for (run_id, entity) in reloaded {
        host.register(run_id, entity);
    }

    // The spawner captures everything an agent needs; `now_secs` is called at
    // spawn time for the run's start timestamp.
    host.set_spawner(Box::new(move |world, args| {
        build_agent(
            world,
            tool_service.as_ref(),
            &config,
            shared_mcp.clone(),
            &mcp_tool_defs,
            &hub,
            args,
            now_secs(),
        )
    }));
    host
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_runtime::components::AgentStatus;
    use leviath_runtime::host::{ControlOp, SpawnArgs};
    use tokio::sync::oneshot;

    struct FakeProvider;
    #[async_trait::async_trait]
    impl leviath_providers::Provider for FakeProvider {
        async fn infer(
            &self,
            _r: leviath_providers::InferenceRequest,
        ) -> leviath_providers::Result<leviath_providers::InferenceResponse> {
            Err(leviath_providers::ProviderError::Other("test".to_string()))
        }
        fn count_tokens(&self, _t: &str, _m: &str) -> usize {
            1
        }
        fn max_context_tokens(&self, _m: &str) -> usize {
            1000
        }
        fn name(&self) -> &str {
            "fake"
        }
        fn capabilities(&self, _m: &str) -> leviath_providers::ModelCapabilities {
            leviath_providers::ModelCapabilities::default()
        }
    }

    #[test]
    fn control_address_is_derived_from_leviath_home() {
        let a = temp_env::with_var("LEVIATH_HOME", Some("/tmp/leviath-home-a"), control_address)
            .unwrap();
        let b = temp_env::with_var("LEVIATH_HOME", Some("/tmp/leviath-home-b"), control_address)
            .unwrap();
        // Different homes resolve to different control ids on every platform.
        assert_ne!(a, b);
        // On Unix the id is the socket path under the home's `.leviath` dir.
        #[cfg(unix)]
        {
            assert!(a.ends_with(".leviath/control.sock"));
            assert!(a.starts_with("/tmp/leviath-home-a"));
        }
    }

    #[tokio::test]
    async fn setup_daemon_host_builds_a_working_host() {
        // Config::default has no MCP servers → the shared MCP connect is a no-op.
        // An empty runs dir → restart recovery finds nothing to reload.
        let runs = tempfile::tempdir().unwrap();
        let mut host = setup_daemon_host(
            Config::default(),
            runs.path().to_path_buf(),
            Handle::current(),
        )
        .await;

        // Spawning through the wired host exercises the real setup end to end
        // (including the now_secs timestamp closure).
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(
            &manifest,
            std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../agents/coder/agent.leviath"),
            )
            .unwrap(),
        )
        .unwrap();
        let (reply, rx) = oneshot::channel();
        host.handle(ControlOp::Spawn {
            args: SpawnArgs {
                run_id: "run-s".to_string(),
                blueprint_path: manifest.to_string_lossy().to_string(),
                task: "t".to_string(),
                model: None,
                workdir: std::env::temp_dir().to_string_lossy().to_string(),
                metadata: Default::default(),
                callback_url: None,
                yolo: false,
                allow: Vec::new(),
                max_depth: None,
            },
            reply,
        });
        assert_eq!(rx.await.unwrap(), Ok("run-s".to_string()));
    }

    #[tokio::test]
    async fn fake_provider_methods_are_exercised() {
        use leviath_providers::Provider;
        let p = FakeProvider;
        assert_eq!(p.name(), "fake");
        assert_eq!(p.count_tokens("t", "m"), 1);
        assert_eq!(p.max_context_tokens("m"), 1000);
        let _ = p.capabilities("m");
        assert!(
            p.infer(leviath_providers::InferenceRequest {
                system: vec![],
                messages: vec![],
                model: "m".to_string(),
                max_tokens: 1,
                temperature: 0.0,
                tools: vec![],
                extra: serde_json::Value::Null,
            })
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn build_host_spawns_agents_through_the_installed_spawner() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(
            &manifest,
            std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../agents/coder/agent.leviath"),
            )
            .unwrap(),
        )
        .unwrap();

        let mut registry = ProviderRegistry::new();
        registry.register("anthropic".to_string(), Arc::new(FakeProvider));
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));

        let runs = tempfile::tempdir().unwrap();
        let mut host = build_host(
            Config::default(),
            registry,
            runs.path().to_path_buf(),
            mcp,
            vec![],
            Handle::current(),
            || 100,
        );

        // Drive a Spawn control op through the host.
        let (reply, rx) = oneshot::channel();
        host.handle(ControlOp::Spawn {
            args: SpawnArgs {
                run_id: "run-1".to_string(),
                blueprint_path: manifest.to_string_lossy().to_string(),
                task: "do it".to_string(),
                model: None,
                workdir: std::env::temp_dir().to_string_lossy().to_string(),
                metadata: Default::default(),
                callback_url: None,
                yolo: false,
                allow: Vec::new(),
                max_depth: None,
            },
            reply,
        });
        assert_eq!(rx.await.unwrap(), Ok("run-1".to_string()));

        // The run is registered and Active.
        let (reply, rx) = oneshot::channel();
        host.handle(ControlOp::Status {
            run_id: "run-1".to_string(),
            reply,
        });
        assert_eq!(rx.await.unwrap(), Some(AgentStatus::Active));
    }

    #[tokio::test]
    async fn build_host_reloads_and_registers_persisted_runs() {
        // A running run persisted under the runs dir must be reloaded + registered
        // by `build_host` (exercising the recovery register loop).
        let agent = tempfile::tempdir().unwrap();
        let manifest = agent.path().join("agent.leviath");
        std::fs::write(
            &manifest,
            std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../agents/coder/agent.leviath"),
            )
            .unwrap(),
        )
        .unwrap();

        let runs = tempfile::tempdir().unwrap();
        let run_dir = runs.path().join("resumed");
        std::fs::create_dir_all(&run_dir).unwrap();
        let meta = leviath_core::run_meta::RunMeta {
            run_id: "resumed".to_string(),
            agent_name: "coder".to_string(),
            agent_path: manifest.to_string_lossy().to_string(),
            task: "resume".to_string(),
            model: None,
            pid: 0,
            status: leviath_core::run_meta::RunStatus::Running,
            current_stage: "implement".to_string(),
            stage_index: 0,
            num_stages: 1,
            iteration: 2,
            prompt_tokens: 0,
            completion_tokens: 0,
            cached_tokens: 0,
            cache_write_tokens: 0,
            tool_calls: 0,
            workdir: std::env::temp_dir().to_string_lossy().to_string(),
            started_at: 1,
            updated_at: 1,
            error: None,
            title: None,
            metadata: Default::default(),
            callback_url: None,
            parent_run_id: None,
        };
        std::fs::write(
            run_dir.join("meta.json"),
            serde_json::to_string(&meta).unwrap(),
        )
        .unwrap();

        let mut registry = ProviderRegistry::new();
        registry.register("anthropic".to_string(), Arc::new(FakeProvider));
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        let mut host = build_host(
            Config::default(),
            registry,
            runs.path().to_path_buf(),
            mcp,
            vec![],
            Handle::current(),
            || 100,
        );

        // The reloaded run is registered → Status resolves it.
        let (reply, rx) = oneshot::channel();
        host.handle(ControlOp::Status {
            run_id: "resumed".to_string(),
            reply,
        });
        assert_eq!(rx.await.unwrap(), Some(AgentStatus::Active));
    }
}
