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

use leviath_runtime::fanout::FanOutSpawnerRes;

use crate::commands::run::session::build_provider_registry_from_config;
use crate::config::{Config, leviath_home_dir};
use crate::daemon::fanout_spawner::DaemonFanOutSpawner;
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

/// This CLI binary's build id (short git hash, `-dirty` when the tree had
/// uncommitted changes), embedded at compile time by `build.rs`. A long-lived
/// daemon records the build it started from; a mismatch means the installed
/// binary is newer and the daemon is running stale code.
pub const CURRENT_BUILD: &str = env!("LEVIATH_BUILD");

/// Path to the file where a running daemon records its build id
/// (`<leviath-home>/.leviath/daemon.build`).
pub fn build_marker_path() -> Option<std::path::PathBuf> {
    leviath_home_dir().map(|home| home.join(".leviath").join("daemon.build"))
}

/// Record [`CURRENT_BUILD`] so the CLI can detect a stale daemon later.
/// Best-effort — a missing marker just triggers a restart on the next command.
pub fn write_build_marker() {
    // Combinators (rather than `if let`) so the "no home dir" / "no parent"
    // fallbacks don't add branches that can't be exercised where a home always
    // resolves — mirroring `control_address`'s `.map` style.
    build_marker_path().into_iter().for_each(|path| {
        let _ = path.parent().map(std::fs::create_dir_all);
        let _ = std::fs::write(&path, CURRENT_BUILD);
    });
}

/// The build id a running daemon recorded, if the marker exists and is readable.
pub fn read_build_marker() -> Option<String> {
    build_marker_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .map(|s| s.trim().to_string())
}

/// Whether a running daemon should be restarted because it is on a different
/// build than this CLI (or recorded no build at all — e.g. it predates this
/// check).
pub fn daemon_build_is_stale(recorded: Option<&str>) -> bool {
    recorded != Some(CURRENT_BUILD)
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
    // The shared MCP pool: seed the connected global servers, then reconnect the
    // per-agent MCP servers of any non-terminal persisted run so a run reloaded on
    // restart can still execute its blueprint MCP tools (recovery warming — the
    // async counterpart of the live-spawn preprocessor, done here before the
    // sync reload inside build_host).
    let mcp_pool =
        crate::daemon::mcp_pool::McpPool::for_daemon(registry.mcp.clone(), &config.mcp_servers);
    mcp_pool.warm_recovered(&runs_dir).await;
    build_host(
        config,
        providers,
        runs_dir,
        registry.mcp,
        registry.mcp_tool_defs,
        mcp_pool,
        runtime,
        || chrono::Utc::now().timestamp(),
    )
}

/// The reap hook installed on the host: drops a reaped agent's tool state and
/// tears down its sandbox via [`CliToolService::reap`]. Factored out (rather than
/// an inline closure) so its body is exercised by a unit test — the daemon itself
/// only ever fires the reaper from the private `serve()` loop.
fn make_reaper(tool_service: Arc<CliToolService>) -> leviath_runtime::host::Reaper {
    Box::new(move |_world, entity| tool_service.reap(entity))
}

/// Build the daemon's [`WorldHost`]: one world hosting every agent, its tool
/// service + interaction hub, and a `Spawn`-op spawner that loads blueprints and
/// registers per-agent tool state. `shared_mcp` / `mcp_tool_defs` are the MCP
/// connections built once at startup and reused by every agent.
#[allow(clippy::too_many_arguments)]
pub fn build_host(
    config: Config,
    providers: ProviderRegistry,
    runs_dir: std::path::PathBuf,
    shared_mcp: Arc<Mutex<leviath_mcp::ToolExecutor>>,
    mcp_tool_defs: Vec<Tool>,
    mcp_pool: Arc<crate::daemon::mcp_pool::McpPool>,
    runtime: Handle,
    now_secs: fn() -> i64,
) -> WorldHost {
    let hub = InteractionHub::new();
    let tool_service = Arc::new(CliToolService::new());
    // The configured global fallback bounds concurrent inference for any model
    // without its own per-model pool entry (defaults to a small cap so a fresh
    // install can't fan out unbounded requests against provider rate limits).
    let pool_config =
        InferencePoolConfig::new().with_default(config.limits.max_concurrent_inferences);
    let mut world = PipelineWorld::new(
        providers,
        tool_service.clone(),
        pool_config,
        config.limits.max_concurrent_tools,
        runs_dir.clone(),
        runtime,
    );
    // Opt-in accurate pre-inference budget guard (off by default).
    world.set_exact_token_counting(config.limits.exact_token_counting);
    // Share the hub with the tick loop so a blocked agent's open prompt is
    // reflected into its status (Active ↔ Waiting) for the dashboard to surface.
    world.insert_interaction_hub(hub.clone());
    let mut host = WorldHost::with_interactions(world, hub.clone());
    // Handed to each agent's tool state so its sub-agent tools reach the world
    // through the host.
    let subagent_tx = host.subagent_sender();

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
        &subagent_tx,
    );
    for (run_id, entity) in reloaded {
        host.register(run_id, entity);
    }

    // Install the fan-out spawner as a world resource so the runtime's fan-out
    // systems can start workers (it captures the same context as the spawner
    // below, cloned before those move into the closure).
    let fanout_spawner = DaemonFanOutSpawner {
        config: config.clone(),
        shared_mcp: shared_mcp.clone(),
        mcp_tool_defs: mcp_tool_defs.clone(),
        mcp_pool: mcp_pool.clone(),
        hub: hub.clone(),
        subagent_tx: subagent_tx.clone(),
        tool_service: tool_service.clone(),
        agents_dir: leviath_home_dir().map(|h| h.join(".leviath").join("agents")),
        now_secs,
    };
    host.world_mut()
        .world_mut()
        .insert_resource(FanOutSpawnerRes(Arc::new(fanout_spawner)));

    // The tool allowlist policy (`policy.toml`), for the taint gate. A malformed
    // file falls back to an empty policy (deny-by-clearance only) rather than
    // failing daemon startup.
    let policy = crate::commands::policy::load_policy().unwrap_or_default();
    host.world_mut()
        .world_mut()
        .insert_resource(leviath_runtime::pipeline::PolicyGate(policy));

    // Scripted gate rules (`<config>/leviath/rules/*.rhai`), consulted by the gate
    // after the static allowlist (a no-op checker when there are none).
    let script_checker =
        crate::daemon::gate_rules::build_gate_script_checker(&crate::commands::policy::rules_dir());
    host.world_mut()
        .world_mut()
        .insert_resource(leviath_runtime::pipeline::GateScriptRules(script_checker));

    // Reload-on-demand: an op targeting an unloaded run pages it back in from
    // disk. Capture the shared context (cloned before the spawner moves the
    // originals below).
    let reload_tools = tool_service.clone();
    let reload_config = config.clone();
    let reload_mcp = shared_mcp.clone();
    let reload_defs = mcp_tool_defs.clone();
    let reload_hub = hub.clone();
    let reload_tx = subagent_tx.clone();
    let reload_runs = runs_dir.clone();
    host.set_reloader(Box::new(move |world, run_id| {
        crate::daemon::recovery::reload_run(
            world,
            reload_tools.as_ref(),
            &reload_config,
            reload_mcp.clone(),
            &reload_defs,
            &reload_hub,
            run_id,
            &reload_runs,
            now_secs(),
            &reload_tx,
        )
    }));

    // Reap hook: when a terminal agent is reaped, tear down its sandbox and drop
    // its per-agent tool state (the latter also fixing a prior leak where tool
    // state was never released). Factored into `make_reaper` so the closure body
    // is unit-testable — the daemon only ever drives it from `serve()`.
    host.set_reaper(make_reaper(tool_service.clone()));

    // The shared MCP pool (created + recovery-warmed by the caller). Per-agent
    // `[[mcp_servers]]` connect lazily through it.

    // Preprocessor: before the sync spawner runs, connect the blueprint's declared
    // MCP servers into the shared pool (lazy, deduped) so they're warm to advertise
    // — and pre-warm the servers declared by any `worker_agent`/`worker_query`
    // fan-out worker this blueprint will spawn, so the *first* such worker already
    // advertises them (they'd otherwise land one turn late — issue #97).
    let pp_pool = mcp_pool.clone();
    let pp_agents_dir = leviath_home_dir().map(|h| h.join(".leviath").join("agents"));
    host.set_spawn_preprocessor(Box::new(move |args| {
        let pool = pp_pool.clone();
        let blueprint_path = args.blueprint_path.clone();
        let agents_dir = pp_agents_dir.clone();
        Box::pin(async move {
            warm_blueprint_mcp(&pool, &blueprint_path).await;
            warm_fanout_worker_mcp(&pool, &blueprint_path, agents_dir.as_deref()).await;
        })
    }));

    // The spawner captures everything an agent needs; `now_secs` is called at
    // spawn time for the run's start timestamp. Per-agent MCP defs = the global
    // servers' defs plus this blueprint's declared servers' defs (warmed above).
    let spawn_pool = mcp_pool.clone();
    host.set_spawner(Box::new(move |world, args| {
        // Stake out the run directory before anything that can fail: blueprint
        // parsing, sandbox creation, provider resolution and seed validation all
        // come later, and until now a failure at any of them left no trace on
        // disk at all — no run dir, no meta.json, nothing to diagnose (#107).
        // The reload path deliberately doesn't do this: it must not overwrite a
        // recovering run's own metadata.
        write_placeholder_meta(args);
        let defs = per_agent_mcp_defs(&spawn_pool, &mcp_tool_defs, &args.blueprint_path);
        build_agent(
            world.world_mut(),
            tool_service.as_ref(),
            &config,
            shared_mcp.clone(),
            &defs,
            &hub,
            args,
            now_secs(),
            subagent_tx.clone(),
        )
    }));
    host
}

/// Create the run directory and write a `Starting` `meta.json` for a run that is
/// about to be built, so a spawn that dies partway through still leaves something
/// on disk to explain itself (issue #107: 3/13 empty runs crashed before any
/// state existed). Everything the agent hasn't resolved yet — model, stage names,
/// stage count — is left blank; the first persistence tick overwrites the file
/// with the real thing. Best-effort: a failure here must not block the spawn.
fn write_placeholder_meta(args: &leviath_runtime::host::SpawnArgs) {
    // The real agent name lives in the blueprint, which hasn't been parsed yet —
    // but the run id is `<agent>-<unix-secs>-<hex4>`, so its prefix is the name
    // (dashes inside the agent name included).
    let agent_name = args
        .run_id
        .rsplitn(3, '-')
        .nth(2)
        .unwrap_or(&args.run_id)
        .to_string();
    let meta = leviath_core::run_meta::RunMeta::new(
        args.run_id.clone(),
        agent_name,
        args.blueprint_path.clone(),
        args.task.clone(),
        None,
        args.workdir.clone(),
        0,
    );
    if let Err(e) = crate::runstate::create_run(&meta) {
        tracing::warn!(run_id = %args.run_id, error = %e, "could not pre-create run directory");
    }
}

/// The spawn-preprocessor body: connect the blueprint's declared `[[mcp_servers]]`
/// into `pool` (lazy, deduped by signature). A missing/unreadable manifest is a
/// no-op. Extracted from the closure so its body is unit-testable.
async fn warm_blueprint_mcp(pool: &crate::daemon::mcp_pool::McpPool, blueprint_path: &str) {
    if let Ok(toml) = std::fs::read_to_string(blueprint_path) {
        for server in crate::daemon::mcp_pool::parse_blueprint_mcp_servers(&toml) {
            pool.ensure(&server).await;
        }
    }
}

/// Pre-warm the MCP servers declared by this blueprint's `worker_agent` /
/// `worker_query` fan-out workers, so the *first* worker spawned advertises them
/// immediately instead of one turn late (issue #97). `worker_stage` workers reuse
/// the parent's own blueprint, already warmed by [`warm_blueprint_mcp`], so they
/// are skipped here. A worker source that can't be read/resolved is skipped.
/// Extracted from the preprocessor closure so its body is unit-testable.
async fn warm_fanout_worker_mcp(
    pool: &crate::daemon::mcp_pool::McpPool,
    blueprint_path: &str,
    agents_dir: Option<&std::path::Path>,
) {
    let Ok(content) = std::fs::read_to_string(blueprint_path) else {
        return;
    };
    let Ok(blueprint) = leviath_core::manifest::parse_manifest(&content) else {
        return;
    };
    for stage in &blueprint.stages {
        let leviath_core::blueprint::StageMode::FanOut { config } = &stage.mode else {
            continue;
        };
        // A `worker_stage` worker runs the parent blueprint (already warmed).
        if config.worker_stage.is_some() {
            continue;
        }
        let Ok((resolve_path, _)) = crate::daemon::fanout_spawner::resolve_worker_source(
            config,
            blueprint_path,
            agents_dir,
        ) else {
            continue;
        };
        let Ok(manifest) = crate::commands::run::manifest::find_manifest(&resolve_path) else {
            continue;
        };
        if let Ok(worker_toml) = std::fs::read_to_string(&manifest) {
            for server in crate::daemon::mcp_pool::parse_blueprint_mcp_servers(&worker_toml) {
                pool.ensure(&server).await;
            }
        }
    }
}

/// The per-agent MCP tool defs: the global servers' defs plus this blueprint's
/// declared servers' cached defs (the pool must already be warm — the
/// preprocessor ran). A missing/unreadable manifest yields just the global defs.
/// Extracted from the spawner closure so its body is unit-testable.
fn per_agent_mcp_defs(
    pool: &crate::daemon::mcp_pool::McpPool,
    global: &[Tool],
    blueprint_path: &str,
) -> Vec<Tool> {
    let mut defs = global.to_vec();
    if let Ok(toml) = std::fs::read_to_string(blueprint_path) {
        let servers = crate::daemon::mcp_pool::parse_blueprint_mcp_servers(&toml);
        defs.extend(pool.cached_defs_for(&servers));
    }
    defs
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_runtime::components::AgentStatus;
    use leviath_runtime::host::{ControlOp, SpawnArgs};
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn make_reaper_delegates_to_tool_service_reap() {
        // Exercises the reaper closure body build_host installs. The daemon only
        // fires it from the private `serve()` loop, so drive it directly here.
        let tool_service = Arc::new(CliToolService::new());
        let mut world = PipelineWorld::new(
            ProviderRegistry::new(),
            tool_service.clone(),
            InferencePoolConfig::new(),
            1,
            std::env::temp_dir(),
            Handle::current(),
        );
        let mut reaper = make_reaper(tool_service.clone());
        // No registered state for this entity → a clean no-op (the reap-branch
        // logic itself is covered by CliToolService::reap's own unit test).
        let entity = bevy_ecs::entity::Entity::from_raw(1);
        reaper(&mut world, entity);
        assert!(tool_service.take(entity).is_none());
    }

    struct FakeProvider;
    #[async_trait::async_trait]
    impl leviath_providers::Provider for FakeProvider {
        async fn infer(
            &self,
            _r: leviath_providers::InferenceRequest,
        ) -> leviath_providers::Result<leviath_providers::InferenceResponse> {
            Err(leviath_providers::ProviderError::Other("test".to_string()))
        }
        async fn count_tokens(&self, _t: &str, _m: &str) -> usize {
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
        std::fs::write(&manifest, crate::test_support::inline_coder_manifest()).unwrap();
        let (reply, rx) = oneshot::channel();
        host.handle(ControlOp::Spawn {
            args: Box::new(SpawnArgs {
                run_id: "run-s".to_string(),
                blueprint_path: manifest.to_string_lossy().to_string(),
                task: "t".to_string(),
                regions: Default::default(),
                model: None,
                workdir: std::env::temp_dir().to_string_lossy().to_string(),
                metadata: Default::default(),
                callback_url: None,
                callback_secret: None,
                yolo: false,
                no_seed_commands: false,
                allow: Vec::new(),
                max_depth: None,
                parent_run_id: None,
            }),
            reply,
        });
        assert_eq!(rx.await.unwrap(), Ok("run-s".to_string()));
    }

    /// Issue #107: 3/13 empty runs died before any state existed, leaving nothing
    /// on disk to diagnose. The spawner stakes out the run directory first, so a
    /// spawn that fails at *any* later step still leaves a `meta.json`.
    #[tokio::test]
    async fn spawner_pre_creates_the_run_dir_even_when_the_spawn_fails() {
        crate::runstate::with_isolated_runs_dir_async("spawn-placeholder", |_base| async {
            let runs = tempfile::tempdir().unwrap();
            let mut host = setup_daemon_host(
                Config::default(),
                runs.path().to_path_buf(),
                Handle::current(),
            )
            .await;
            let (reply, rx) = oneshot::channel();
            host.handle(ControlOp::Spawn {
                args: Box::new(SpawnArgs {
                    // A blueprint path that doesn't exist: the spawn fails at the
                    // very first step inside build_agent.
                    run_id: "my-agent-1234-ab12".to_string(),
                    blueprint_path: "/no/such/agent.leviath".to_string(),
                    task: "t".to_string(),
                    regions: Default::default(),
                    model: None,
                    workdir: std::env::temp_dir().to_string_lossy().to_string(),
                    metadata: Default::default(),
                    callback_url: None,
                    callback_secret: None,
                    yolo: false,
                    no_seed_commands: false,
                    allow: Vec::new(),
                    max_depth: None,
                    parent_run_id: None,
                }),
                reply,
            });
            assert!(rx.await.unwrap().is_err());

            let meta = crate::runstate::read_meta("my-agent-1234-ab12")
                .expect("a failed spawn still leaves meta.json behind");
            assert_eq!(meta.status, leviath_core::run_meta::RunStatus::Starting);
            assert_eq!(meta.task, "t");
            // The agent name is recovered from the run id's prefix, dashes and all.
            assert_eq!(meta.agent_name, "my-agent");
        })
        .await;
    }

    #[test]
    fn placeholder_meta_falls_back_to_the_whole_run_id_as_the_agent_name() {
        crate::runstate::with_isolated_runs_dir("spawn-placeholder-odd-id", |_base| {
            let args = SpawnArgs {
                // Not the `<agent>-<secs>-<hex>` shape the run-id minter makes.
                run_id: "odd".to_string(),
                task: "t".to_string(),
                ..Default::default()
            };
            write_placeholder_meta(&args);
            assert_eq!(crate::runstate::read_meta("odd").unwrap().agent_name, "odd");
        });
    }

    #[test]
    fn placeholder_meta_failure_is_logged_not_fatal() {
        // An unwritable runs dir (here: a path *under a regular file*) must not
        // stop the spawn — the placeholder is a diagnostic, not a prerequisite.
        crate::test_support::with_tracing(|| {
            let dir = tempfile::tempdir().unwrap();
            let blocker = dir.path().join("not-a-dir");
            std::fs::write(&blocker, "x").unwrap();
            temp_env::with_var(
                "LEVIATH_RUNS_DIR",
                Some(blocker.join("runs").to_string_lossy().to_string()),
                || {
                    let args = SpawnArgs {
                        run_id: "blocked".to_string(),
                        ..Default::default()
                    };
                    write_placeholder_meta(&args);
                    assert!(crate::runstate::read_meta("blocked").is_err());
                },
            );
        });
    }

    // ── per-agent MCP (issue #97) ──

    /// A python stub MCP server written to a temp file; returns (tempdir, path).
    fn stub_server_py() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stub.py");
        std::fs::write(
            &path,
            r#"
import sys, json
def respond(i, r):
    sys.stdout.write(json.dumps({"jsonrpc":"2.0","id":i,"result":r})+"\n"); sys.stdout.flush()
for line in sys.stdin:
    line=line.strip()
    if not line: continue
    req=json.loads(line); m=req.get("method",""); i=req.get("id")
    if m=="initialize": respond(i,{"capabilities":{"tools":{"listChanged":True}},"protocolVersion":"2024-11-05"})
    elif m=="notifications/initialized": pass
    elif m=="tools/list": respond(i,{"tools":[{"name":"stub_search","description":"s","inputSchema":{"type":"object","properties":{}}}]})
    elif m=="tools/call": respond(i,{"content":[{"type":"text","text":"ok"}],"isError":False})
    else: respond(i,{})
"#,
        )
        .unwrap();
        (dir, path)
    }

    /// Write a blueprint declaring one stdio `[[mcp_servers]]` → the stub; returns
    /// its manifest path.
    fn blueprint_with_mcp(dir: &std::path::Path, stub_py: &std::path::Path) -> std::path::PathBuf {
        let manifest = dir.join("agent.leviath");
        std::fs::write(
            &manifest,
            format!(
                r#"
[agent]
name = "mcpagent"
entry_stage = "work"

[[mcp_servers]]
name = "search"
command = "python3"
args = ['{}']

[stages.work]
mode = "autonomous"
model = {{ provider = "fake", model = "m" }}
available_tools = ["stub_search"]
system_prompt = "use stub_search"

[context.regions]
task = {{ kind = "pinned", max_tokens = 200, seed = {{ caller_input = "task" }} }}
"#,
                stub_py.to_string_lossy()
            ),
        )
        .unwrap();
        manifest
    }

    fn empty_pool() -> crate::daemon::mcp_pool::McpPool {
        crate::daemon::mcp_pool::McpPool::new(
            Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
            Default::default(),
        )
    }

    #[tokio::test]
    async fn warm_blueprint_mcp_connects_declared_servers() {
        let (_stub_dir, stub) = stub_server_py();
        let dir = tempfile::tempdir().unwrap();
        let manifest = blueprint_with_mcp(dir.path(), &stub);
        let pool = empty_pool();
        warm_blueprint_mcp(&pool, &manifest.to_string_lossy()).await;
        // The declared server is now warm: its tool is cached + advertised.
        let servers = crate::daemon::mcp_pool::parse_blueprint_mcp_servers(
            &std::fs::read_to_string(&manifest).unwrap(),
        );
        let defs = pool.cached_defs_for(&servers);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "stub_search");
    }

    #[tokio::test]
    async fn warm_blueprint_mcp_missing_manifest_is_noop() {
        let pool = empty_pool();
        // Unreadable path → the read-error arm, no panic.
        warm_blueprint_mcp(&pool, "/no/such/agent.leviath").await;
    }

    /// Write a parent blueprint whose fan-out stage delegates to `worker_source`
    /// (a `worker_agent` path). Returns the parent manifest path.
    fn parent_with_fanout_worker_agent(
        dir: &std::path::Path,
        worker_source: &str,
    ) -> std::path::PathBuf {
        let manifest = dir.join("parent.leviath");
        std::fs::write(
            &manifest,
            format!(
                "[agent]\nname = \"parent\"\n\n\
                 [stages.main]\nmode = \"autonomous\"\n\n\
                 [stages.parallel]\nmode = \"fan_out\"\nworker_agent = '{worker_source}'\nsplit_prompt = \"go\"\n"
            ),
        )
        .unwrap();
        manifest
    }

    #[tokio::test]
    async fn warm_fanout_worker_mcp_prewarms_worker_agent_servers() {
        let (_stub_dir, stub) = stub_server_py();
        // A worker blueprint declaring an MCP server.
        let worker_dir = tempfile::tempdir().unwrap();
        blueprint_with_mcp(worker_dir.path(), &stub);
        // A parent whose fan-out delegates to that worker directory.
        let parent_dir = tempfile::tempdir().unwrap();
        let parent = parent_with_fanout_worker_agent(
            parent_dir.path(),
            &worker_dir.path().to_string_lossy(),
        );
        let pool = empty_pool();
        warm_fanout_worker_mcp(&pool, &parent.to_string_lossy(), None).await;
        // The worker's declared server is now warm (its tool cached), so the first
        // worker will advertise it immediately.
        let servers = crate::daemon::mcp_pool::parse_blueprint_mcp_servers(
            &std::fs::read_to_string(worker_dir.path().join("agent.leviath")).unwrap(),
        );
        let defs = pool.cached_defs_for(&servers);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "stub_search");
    }

    #[tokio::test]
    async fn warm_fanout_worker_mcp_skips_and_tolerates_every_arm() {
        let pool = empty_pool();
        // Unreadable parent → read-error return.
        warm_fanout_worker_mcp(&pool, "/no/such/parent.leviath", None).await;
        // Unparsable parent → parse-error return.
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("bad.leviath");
        std::fs::write(&bad, "not : valid : toml").unwrap();
        warm_fanout_worker_mcp(&pool, &bad.to_string_lossy(), None).await;
        // A blueprint with only a non-fan-out stage → the `continue` (not FanOut).
        let plain = dir.path().join("plain.leviath");
        std::fs::write(
            &plain,
            "[agent]\nname = \"p\"\n\n[stages.main]\nmode = \"autonomous\"\n",
        )
        .unwrap();
        warm_fanout_worker_mcp(&pool, &plain.to_string_lossy(), None).await;
        // A `worker_stage` fan-out → skipped (reuses the parent's own servers).
        let ws = dir.path().join("ws.leviath");
        std::fs::write(
            &ws,
            "[agent]\nname = \"p\"\n\n\
             [stages.parallel]\nmode = \"fan_out\"\nworker_stage = \"w\"\nsplit_prompt = \"go\"\n\n\
             [stages.w]\nmode = \"autonomous\"\nallow_as_worker = true\n",
        )
        .unwrap();
        warm_fanout_worker_mcp(&pool, &ws.to_string_lossy(), None).await;
        // A `worker_query` with no agents dir → resolve_worker_source errors → skip.
        let wq = dir.path().join("wq.leviath");
        std::fs::write(
            &wq,
            "[agent]\nname = \"p\"\n\n\
             [stages.parallel]\nmode = \"fan_out\"\nworker_query = \"x\"\nsplit_prompt = \"go\"\n",
        )
        .unwrap();
        warm_fanout_worker_mcp(&pool, &wq.to_string_lossy(), None).await;
        // A `worker_agent` pointing at a nonexistent path → find_manifest errors → skip.
        let miss = parent_with_fanout_worker_agent(dir.path(), "/no/such/worker/xyz");
        warm_fanout_worker_mcp(&pool, &miss.to_string_lossy(), None).await;
        // A `worker_agent` whose blueprint declares no [[mcp_servers]] → read-ok,
        // empty server loop.
        let worker_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            worker_dir.path().join("agent.leviath"),
            "[agent]\nname = \"w\"\n\n[stages.main]\nmode = \"autonomous\"\n",
        )
        .unwrap();
        let noservers =
            parent_with_fanout_worker_agent(dir.path(), &worker_dir.path().to_string_lossy());
        warm_fanout_worker_mcp(&pool, &noservers.to_string_lossy(), None).await;
        // A `worker_agent` dir whose `agent.leviath` is itself a directory:
        // find_manifest resolves it (it `exists()`), but reading it fails → the
        // inner read-error arm.
        let dir_manifest = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir_manifest.path().join("agent.leviath")).unwrap();
        let unreadable =
            parent_with_fanout_worker_agent(dir.path(), &dir_manifest.path().to_string_lossy());
        warm_fanout_worker_mcp(&pool, &unreadable.to_string_lossy(), None).await;
    }

    #[test]
    fn per_agent_mcp_defs_appends_declared_and_falls_back_to_global() {
        let (_stub_dir, stub) = stub_server_py();
        let dir = tempfile::tempdir().unwrap();
        let manifest = blueprint_with_mcp(dir.path(), &stub);
        let pool = empty_pool();
        // Warm the pool by seeding the declared server's defs (avoids a live
        // connect in this sync test).
        let servers = crate::daemon::mcp_pool::parse_blueprint_mcp_servers(
            &std::fs::read_to_string(&manifest).unwrap(),
        );
        pool.seed(
            &servers[0],
            vec![Tool {
                name: "stub_search".into(),
                description: String::new(),
                parameters: serde_json::json!({}),
            }],
        );
        let global = vec![Tool {
            name: "global_tool".into(),
            description: String::new(),
            parameters: serde_json::json!({}),
        }];
        let defs = per_agent_mcp_defs(&pool, &global, &manifest.to_string_lossy());
        let names: Vec<&str> = defs.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["global_tool", "stub_search"]);
        // Missing manifest → just the global defs (read-error arm).
        let only_global = per_agent_mcp_defs(&pool, &global, "/no/such/x");
        assert_eq!(only_global.len(), 1);
        assert_eq!(only_global[0].name, "global_tool");
    }

    #[tokio::test]
    async fn build_host_seeds_global_mcp_servers() {
        // A config with a (never-connected) global server exercises the seed loop.
        let config = Config {
            mcp_servers: vec![leviath_mcp::MCPServerConfig::stdio(
                "global-srv",
                "python3",
                vec!["-c".to_string(), "pass".to_string()],
            )],
            ..Config::default()
        };
        let runs = tempfile::tempdir().unwrap();
        let _host = build_host(
            config,
            ProviderRegistry::new(),
            runs.path().to_path_buf(),
            Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
            Vec::new(),
            crate::daemon::mcp_pool::McpPool::for_daemon(
                Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
                &[],
            ),
            Handle::current(),
            || 0,
        );
    }

    #[tokio::test]
    async fn serve_runs_spawn_preprocessor_for_per_agent_mcp() {
        // Drive a real spawn through `serve()` so the spawn preprocessor fires
        // (the only path that invokes it): the agent declares an MCP server, which
        // gets connected + advertised, and the spawn replies Ok.
        let (_stub_dir, stub) = stub_server_py();
        let agent_dir = tempfile::tempdir().unwrap();
        let manifest = blueprint_with_mcp(agent_dir.path(), &stub);
        // A `fake` provider so stage resolution succeeds.
        let mut providers = ProviderRegistry::new();
        providers.register("fake".to_string(), Arc::new(FakeProvider));
        let runs = tempfile::tempdir().unwrap();
        let mut host = build_host(
            Config::default(),
            providers,
            runs.path().to_path_buf(),
            Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
            Vec::new(),
            crate::daemon::mcp_pool::McpPool::for_daemon(
                Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
                &[],
            ),
            Handle::current(),
            || 0,
        );
        let (ctl_tx, ctl_rx) = tokio::sync::mpsc::unbounded_channel();
        let (reply, reply_rx) = oneshot::channel();
        ctl_tx
            .send(ControlOp::Spawn {
                args: Box::new(SpawnArgs {
                    run_id: "run-mcp".to_string(),
                    blueprint_path: manifest.to_string_lossy().to_string(),
                    task: "t".to_string(),
                    regions: Default::default(),
                    model: None,
                    workdir: std::env::temp_dir().to_string_lossy().to_string(),
                    metadata: Default::default(),
                    callback_url: None,
                    callback_secret: None,
                    yolo: false,
                    no_seed_commands: false,
                    allow: Vec::new(),
                    max_depth: None,
                    parent_run_id: None,
                }),
                reply,
            })
            .unwrap();
        // Close the control channel so serve() returns after handling the op.
        drop(ctl_tx);
        host.serve(ctl_rx).await;
        assert_eq!(reply_rx.await.unwrap(), Ok("run-mcp".to_string()));
    }

    #[tokio::test]
    async fn fake_provider_methods_are_exercised() {
        use leviath_providers::Provider;
        let p = FakeProvider;
        assert_eq!(p.name(), "fake");
        assert_eq!(p.count_tokens("t", "m").await, 1);
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
                request_timeout_secs: None,
            })
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn build_host_spawns_agents_through_the_installed_spawner() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(&manifest, crate::test_support::inline_coder_manifest()).unwrap();

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
            crate::daemon::mcp_pool::McpPool::for_daemon(
                Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
                &[],
            ),
            Handle::current(),
            || 100,
        );

        // Drive a Spawn control op through the host.
        let (reply, rx) = oneshot::channel();
        host.handle(ControlOp::Spawn {
            args: Box::new(SpawnArgs {
                run_id: "run-1".to_string(),
                blueprint_path: manifest.to_string_lossy().to_string(),
                task: "do it".to_string(),
                regions: Default::default(),
                model: None,
                workdir: std::env::temp_dir().to_string_lossy().to_string(),
                metadata: Default::default(),
                callback_url: None,
                callback_secret: None,
                yolo: false,
                no_seed_commands: false,
                allow: Vec::new(),
                max_depth: None,
                parent_run_id: None,
            }),
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
        std::fs::write(&manifest, crate::test_support::inline_coder_manifest()).unwrap();

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
            callback_secret: None,
            parent_run_id: None,
            children: Vec::new(),
            depth: 0,
            max_child_depth: 0,
            flags: Default::default(),
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
            crate::daemon::mcp_pool::McpPool::for_daemon(
                Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
                &[],
            ),
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

    #[tokio::test]
    async fn build_host_installs_a_reloader_that_pages_in_unloaded_runs() {
        // A run that lands on disk *after* startup (so it is not auto-reloaded)
        // must still be reachable: a control op targeting it fires the installed
        // reloader, which pages it into the world on demand.
        let agent = tempfile::tempdir().unwrap();
        let manifest = agent.path().join("agent.leviath");
        std::fs::write(&manifest, crate::test_support::inline_coder_manifest()).unwrap();

        let runs = tempfile::tempdir().unwrap();
        let mut registry = ProviderRegistry::new();
        registry.register("anthropic".to_string(), Arc::new(FakeProvider));
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        let mut host = build_host(
            Config::default(),
            registry,
            runs.path().to_path_buf(),
            mcp,
            vec![],
            crate::daemon::mcp_pool::McpPool::for_daemon(
                Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
                &[],
            ),
            Handle::current(),
            || 100,
        );

        // Persist a running run only now — build_host's startup reload already ran,
        // so it is on disk but absent from the world.
        let run_dir = runs.path().join("late");
        std::fs::create_dir_all(&run_dir).unwrap();
        let meta = leviath_core::run_meta::RunMeta {
            run_id: "late".to_string(),
            agent_name: "coder".to_string(),
            agent_path: manifest.to_string_lossy().to_string(),
            task: "page me in".to_string(),
            model: None,
            pid: 0,
            status: leviath_core::run_meta::RunStatus::Running,
            current_stage: "implement".to_string(),
            stage_index: 0,
            num_stages: 1,
            iteration: 1,
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
            callback_secret: None,
            parent_run_id: None,
            children: Vec::new(),
            depth: 0,
            max_child_depth: 0,
            flags: Default::default(),
        };
        std::fs::write(
            run_dir.join("meta.json"),
            serde_json::to_string(&meta).unwrap(),
        )
        .unwrap();

        // It is not loaded yet: a read-only Status does not page it in.
        let (reply, rx) = oneshot::channel();
        host.handle(ControlOp::Status {
            run_id: "late".to_string(),
            reply,
        });
        assert_eq!(rx.await.unwrap(), None);

        // A Cancel routes through the reloader, paging it in and acting on it.
        let (reply, rx) = oneshot::channel();
        host.handle(ControlOp::Cancel {
            run_id: "late".to_string(),
            reply,
        });
        assert!(rx.await.unwrap());
    }

    #[test]
    fn daemon_build_is_stale_compares_against_current_build() {
        assert!(daemon_build_is_stale(None), "missing marker is stale");
        assert!(
            daemon_build_is_stale(Some("some-other-build")),
            "a different build is stale"
        );
        assert!(
            !daemon_build_is_stale(Some(CURRENT_BUILD)),
            "the current build is not stale"
        );
    }

    #[test]
    fn build_marker_round_trips_and_is_current() {
        let dir = tempfile::tempdir().unwrap();
        temp_env::with_var("LEVIATH_HOME", Some(dir.path()), || {
            // No marker yet → read is None → treated as stale.
            assert!(read_build_marker().is_none());
            assert!(daemon_build_is_stale(read_build_marker().as_deref()));

            write_build_marker();
            let path = build_marker_path().unwrap();
            assert!(path.exists());
            assert_eq!(read_build_marker().as_deref(), Some(CURRENT_BUILD));
            // A daemon that wrote the current build is not stale.
            assert!(!daemon_build_is_stale(read_build_marker().as_deref()));
        });
    }
}
