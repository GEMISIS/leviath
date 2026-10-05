//! Starting runs in tests: a [`DaemonStarter`] over plain values, and a
//! [`start_run`] that starts one into a world the way the daemon does,
//! from the task-and-flags shape the tests describe runs in.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bevy_ecs::entity::Entity;
use bevy_ecs::world::World;
use leviath_providers::Tool;
use leviath_runtime::host::SubAgentOp;
use leviath_runtime::interaction_hub::InteractionHub;
use leviath_runtime::spec::env::Caller;
use tokio::sync::Mutex;
use tokio::sync::mpsc::UnboundedSender;

use super::DaemonStarter;
use crate::config::Config;
use crate::daemon::resolve_env::DaemonEnv;
use crate::daemon::tool_service::CliToolService;

/// A starter over `config` and `registry`, keeping its runs under `runs_dir`
/// and its run files' blobs in memory. Nothing it reads or writes is the
/// developer's own: the policy files it reads do not exist.
pub(crate) fn starter(
    config: Config,
    registry: leviath_runtime::ProviderRegistry,
    runs_dir: &Path,
) -> DaemonStarter {
    let hub = InteractionHub::new();
    let shared_mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
    let pool = crate::daemon::mcp_pool::McpPool::for_daemon(shared_mcp.clone(), &[]);
    let nowhere = runs_dir.join("no-such-config");
    DaemonStarter {
        providers: crate::daemon::provider_reload::for_daemon(&config, registry),
        mcp_global: crate::daemon::mcp_reload::McpReload::new(
            &config,
            pool.clone(),
            Vec::new(),
            Default::default(),
        ),
        config: Arc::new(crate::daemon::config_reload::ConfigReloader::fixed(config)),
        policy: Arc::new(crate::daemon::policy_reload::PolicyReload::new(
            nowhere.join("policy.toml"),
            nowhere.join("rules"),
        )),
        telemetry: crate::daemon::telemetry_reload::TelemetryReload::for_daemon(),
        limits: crate::daemon::live_limits::for_daemon(hub.clone(), Default::default()),
        mcp_pool: pool,
        shared_mcp,
        tool_service: Arc::new(CliToolService::new()),
        hub,
        subagent_tx: tokio::sync::mpsc::unbounded_channel().0,
        runs_dir: runs_dir.to_path_buf(),
        agents_dir: leviath_core::paths::agents_dir(),
        blob_store: Arc::new(leviath_core::mime::MemoryBlobStore::new()),
    }
}

/// A run as the tests describe it: a manifest, a task and the launch flags.
#[derive(Clone, Default)]
pub(crate) struct TestLaunch {
    /// Ignored: the daemon mints every run's id. Kept so a test can say
    /// which run it means in its own words.
    pub run_id: String,
    pub blueprint_path: String,
    pub task: String,
    pub regions: HashMap<String, String>,
    pub parts: Vec<leviath_core::mime::InboundPart>,
    pub model: Option<String>,
    pub workdir: String,
    pub metadata: HashMap<String, String>,
    pub callback_url: Option<String>,
    pub callback_secret: Option<String>,
    pub unattended: leviath_core::Unattended,
    pub no_seed_commands: bool,
    pub allow: Vec<String>,
    pub max_depth: Option<usize>,
    pub output: Option<leviath_core::output::OutputSpec>,
    pub capture_model_input: bool,
    /// Starts the run as a child of this run, under a policy that allows it.
    pub parent_run_id: Option<String>,
}

/// What a run is started against, as the tests hand it over.
#[derive(Clone)]
pub(crate) struct TestDeps<'a> {
    /// The tool service a started run's tools are registered with.
    pub tool_service: &'a Arc<CliToolService>,
    /// The configuration.
    pub config: &'a Config,
    /// The MCP connections every run shares.
    pub shared_mcp: Arc<Mutex<leviath_mcp::ToolExecutor>>,
    /// The tools those servers advertise.
    pub mcp_tool_defs: &'a [Tool],
    /// Which server advertises each of them.
    pub mcp_tool_owners: &'a leviath_runtime::pipeline::ToolOwners,
    /// Where a run's prompts are parked.
    pub hub: &'a InteractionHub,
    /// Where a run's sub-agent tools send.
    pub subagent_tx: UnboundedSender<SubAgentOp>,
}

/// The env a run is resolved and bound against, from `deps` and what the
/// world already holds (its providers, mime registry, blob store and taint
/// policy).
pub(crate) fn env_for(world: &World, deps: TestDeps<'_>) -> DaemonEnv {
    let registry = world
        .resource::<leviath_runtime::pipeline::Providers>()
        .0
        .clone();
    let mime = world
        .resource::<leviath_runtime::blob_store::MimeRegistryHandle>()
        .0
        .clone();
    let blob_store = world
        .resource::<leviath_runtime::blob_store::BlobStoreHandle>()
        .0
        .clone();
    let mcp_overrides = world
        .get_resource::<leviath_runtime::pipeline::PolicyGate>()
        .map(|p| p.0.mcp_overrides.clone())
        .unwrap_or_default();
    DaemonEnv {
        config: Arc::new(deps.config.clone()),
        registry,
        agents_dir: leviath_core::paths::agents_dir(),
        workdir_root: None,
        mcp_defs: deps.mcp_tool_defs.to_vec(),
        mcp_owners: deps.mcp_tool_owners.clone(),
        shared_mcp: deps.shared_mcp,
        tool_service: deps.tool_service.clone(),
        hub: deps.hub.clone(),
        subagent_tx: deps.subagent_tx,
        mime,
        blob_store,
        mcp_overrides,
    }
}

/// The request a [`TestLaunch`] makes.
pub(crate) fn request_for(
    args: &TestLaunch,
) -> Result<leviath_runtime::spec::request::SpawnRequest, String> {
    let workdir = match args.workdir.is_empty() {
        true => std::env::temp_dir().to_string_lossy().into_owned(),
        false => args.workdir.clone(),
    };
    crate::daemon::requests::TaskLaunch {
        blueprint: args.blueprint_path.clone(),
        task: args.task.clone(),
        regions: args.regions.clone(),
        parts: args.parts.clone(),
        model: args.model.clone(),
        workdir: Some(workdir),
        unattended: args.unattended.clone(),
        allow: args.allow.clone(),
        max_depth: args.max_depth,
        no_seed_commands: args.no_seed_commands,
        output: args.output.clone(),
        capture_model_input: args.capture_model_input,
        metadata: args.metadata.clone(),
        callback_url: args.callback_url.clone(),
        callback_secret: args.callback_secret.clone(),
    }
    .into_request()
}

/// Start the run `args` describes into `world` as a top-level run, the way
/// the daemon does: resolve, bind, place. `Err` carries every problem found.
pub(crate) fn start_run(
    world: &mut World,
    deps: TestDeps<'_>,
    args: &TestLaunch,
) -> Result<Entity, String> {
    start_as(world, deps, args, caller_for(args))
}

/// Who a [`TestLaunch`] is started for: a child of its `parent_run_id`,
/// under a policy that allows everything, else the top of its tree.
pub(crate) fn caller_for(args: &TestLaunch) -> Caller {
    let Some(parent) = &args.parent_run_id else {
        return Caller::TopLevel;
    };
    let parent = leviath_runtime::spec::names::RunId::new(parent.as_str()).expect("a run id");
    let policy = leviath_runtime::spec::launch::LaunchPolicy {
        unattended: leviath_runtime::spec::launch::Unattended::All,
        allow: Vec::new(),
        max_depth: 8,
        seed_commands: true,
        capture_model_input: false,
    };
    Caller::Child {
        parent,
        policy,
        depth: 0,
    }
}

/// [`start_run`], for `caller`.
pub(crate) fn start_as(
    world: &mut World,
    deps: TestDeps<'_>,
    args: &TestLaunch,
    caller: Caller,
) -> Result<Entity, String> {
    let request = request_for(args)?;
    let env = env_for(world, deps);
    let resolved = crate::daemon::block_on::block_on(leviath_runtime::resolve::resolve(
        &request,
        &caller,
        &env,
        leviath_runtime::resolve::ResolveMode::Spawn,
    ))
    .map_err(|issues| issues.to_string())?;
    let bindings = crate::daemon::block_on::block_on(leviath_runtime::bind::bind(
        &resolved.spec,
        &resolved.code,
        &env,
    ))
    .map_err(|issues| issues.to_string())?;
    let state = leviath_runtime::insert::initial_state(&resolved.spec);
    Ok(leviath_runtime::insert::insert(
        world,
        Arc::new(resolved.spec),
        bindings,
        &state,
    ))
}

/// A blueprint written to `<dir>/agent.toml`, for a test that starts runs
/// of it. Returns the file's path.
pub(crate) fn manifest_in(dir: &Path, manifest: &str) -> PathBuf {
    let path = dir.join(leviath_blueprint::FILE_NAME);
    std::fs::write(&path, manifest).expect("write the manifest");
    path
}

/// A python stub MCP server written to a temp file; returns (tempdir, path).
pub(crate) fn stub_server_py() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("stub.py");
    let stub = crate::test_support::McpStub::new()
        .list_changed(true)
        .tool("stub_search", Some("s"))
        .input_schema(r#"{"type": "object", "properties": {}}"#)
        .replying("ok");
    std::fs::write(&path, stub.source()).expect("write the stub");
    (dir, path)
}

/// A blueprint in `dir` declaring one stdio `[[mcp_servers]]` running
/// `stub_py`, on the `fake` provider. Returns its manifest path.
pub(crate) fn blueprint_with_mcp(dir: &Path, stub_py: &Path) -> PathBuf {
    manifest_in(
        dir,
        &format!(
            r#"[blueprint]
name = "mcpagent"
version = "0.1.0"

[graph]
entry = "work"
mcp_servers = [{{ name = "search", command = "python3", args = ['{}'] }}]

[[graph.stages]]
name = "work"
system_prompt = "use stub_search"
model = {{ models = [{{ provider = "fake", model = "m" }}] }}
tools = ["stub_search"]

[graph.layout]
regions = [{{ name = "task", kind = "pinned", budget = 200 }}]
total_budget_tokens = 200

[[graph.inputs]]
name = "task"
type = {{ kind = "text", multiline = true }}
binds = [{{ region = "task" }}]
"#,
            stub_py.to_string_lossy()
        ),
    )
}

/// A request to run `manifest` on `task` in the temp directory.
pub(crate) fn task_request(
    manifest: &Path,
    task: &str,
) -> leviath_runtime::spec::request::SpawnRequest {
    crate::daemon::requests::TaskLaunch {
        blueprint: manifest.to_string_lossy().into_owned(),
        task: task.to_string(),
        workdir: Some(std::env::temp_dir().to_string_lossy().into_owned()),
        ..Default::default()
    }
    .into_request()
    .expect("the request reads")
}

/// A run of `manifest` recorded under `runs` the way the daemon records one,
/// and loaded nowhere: what a daemon starting up finds. Returns its id.
pub(crate) fn run_on_disk(
    config: Config,
    registry: leviath_runtime::ProviderRegistry,
    runs: &Path,
    manifest: &Path,
) -> String {
    let starter = starter(config, registry, runs);
    let request = task_request(manifest, "carry on");
    let env = starter.env_for(&request, starter.config.current());
    let prepared =
        crate::daemon::block_on::block_on(starter.start_with(env, request, Caller::TopLevel))
            .expect("the run starts");
    prepared.spec.run_id.to_string()
}
