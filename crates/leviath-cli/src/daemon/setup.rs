//! Daemon assembly: build a fully-wired [`WorldHost`] (world + tool service +
//! interaction hub + the run starter) ready to be driven by
//! [`WorldHost::serve`]. The async setup (provider registry, MCP connections)
//! happens in the binary and is passed in; this wiring is synchronous and
//! testable - starting a run through the installed starter exercises the whole
//! path.

use std::sync::Arc;

use leviath_providers::Tool;
use leviath_runtime::ProviderRegistry;
use leviath_runtime::host::WorldHost;
use leviath_runtime::interaction_hub::InteractionHub;
use leviath_runtime::world::PipelineWorld;
use tokio::runtime::Handle;
use tokio::sync::Mutex;

use leviath_runtime::control_socket::StartupBoard;
use leviath_runtime::fanout::FanOutSpawnerRes;

use crate::config::Config;
use crate::daemon::fanout_spawner::DaemonFanOutSpawner;
use crate::daemon::starter::DaemonStarter;
use crate::daemon::tool_service::CliToolService;
use crate::tools::ToolRegistry;

/// The daemon's control-channel id, derived from `<leviath-home>/.leviath`
/// (honoring `LEVIATH_HOME`): a Unix-socket path on Unix, a named-pipe name on
/// Windows. `None` if no home directory can be resolved.
pub fn control_address() -> Option<leviath_runtime::control_socket::ControlId> {
    control_dir().map(|dir| leviath_runtime::control_socket::control_id(&dir))
}

/// The directory holding the control channel and its token.
///
/// Separate from [`control_address`] because on Windows a control id is a pipe
/// name rather than a path, so the token's location cannot be derived from it.
pub fn control_dir() -> Option<std::path::PathBuf> {
    leviath_core::paths::data_dir()
}

/// This CLI binary's build id (short git hash, `-dirty` when the tree had
/// uncommitted changes), embedded at compile time by `build.rs`. A long-lived
/// daemon records the build it started from; a mismatch means the installed
/// binary is newer and the daemon is running stale code.
pub const CURRENT_BUILD: &str = env!("LEVIATH_BUILD");

/// Environment variables that a bundled script tool needs and that only the
/// daemon's own process can be asked about.
///
/// A daemon inherits its environment at exec time from whatever started it, and
/// no client shares it. So `lev doctor` inspecting its own environment answers
/// for the shell it was typed in, not for the process that will actually run
/// the tool - which is how a working key and a daemon that could not see it
/// reported as healthy while every search fell back to Wikipedia.
///
/// Kept to credentials a *bundled* tool reads, so the reported set is a fixed
/// list in the source rather than anything scraped from the environment.
pub const TOOL_ENV_PROBE: &[&str] = &["BRAVE_API_KEY"];

/// Which of [`TOOL_ENV_PROBE`] this process can actually read, by name.
///
/// Presence only - the value is never read here and never leaves the process.
/// An empty vec is the real answer "asked, saw none", which the identity type
/// keeps distinct from "did not say".
pub fn visible_tool_env() -> Vec<String> {
    TOOL_ENV_PROBE
        .iter()
        .filter(|name| std::env::var_os(name).is_some_and(|v| !v.is_empty()))
        .map(|name| (*name).to_string())
        .collect()
}

/// Path to the file where a running daemon records its build id
/// (`<leviath-home>/.leviath/daemon.build`).
pub(crate) fn build_marker_path() -> Option<std::path::PathBuf> {
    leviath_core::paths::data_dir().map(|d| d.join("daemon.build"))
}

/// Record this build (its id, version and commit time; see
/// [`super::build::Build::marker`]) so a `lev` of another build can tell how
/// it stands to this daemon. Best-effort: a missing marker reads as an older
/// daemon, which the next spawn command replaces.
pub fn write_build_marker() {
    // Combinators (rather than `if let`) so the "no home dir" / "no parent"
    // fallbacks don't add branches that can't be exercised where a home always
    // resolves - mirroring `control_address`'s `.map` style.
    build_marker_path().into_iter().for_each(|path| {
        let _ = path.parent().map(std::fs::create_dir_all);
        let _ = std::fs::write(&path, super::build::Build::current().marker());
    });
}

/// [`write_build_marker`] before the control channel at `id` is bound, so the
/// first `lev` to reach this daemon reads its build, never the build of the
/// daemon before it. A daemon already answering at `id` keeps its marker:
/// this one is about to lose the single-instance bind to it.
pub fn write_build_marker_unless_running(id: &leviath_runtime::control_socket::ControlId) {
    if !leviath_runtime::control_socket::is_daemon_running(id) {
        write_build_marker();
    }
}

/// The build id a running daemon recorded, if the marker exists and is readable.
pub fn read_build_marker() -> Option<String> {
    build_marker_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .map(|s| s.trim().to_string())
}

/// Build the daemon's [`WorldHost`], doing the async startup work: build the
/// provider registry from config and connect the shared MCP servers (both reused
/// by every agent), upgrade what an earlier release left in the home, then wire
/// the host + spawner via [`build_host`]. Each step says on `board` what it is
/// doing, for the clients that reach the daemon meanwhile.
pub async fn setup_daemon_host(
    config: Config,
    runs_dir: std::path::PathBuf,
    runtime: Handle,
    board: &StartupBoard,
) -> anyhow::Result<WorldHost> {
    setup_daemon_host_with(
        config,
        (runs_dir, board),
        runtime,
        &leviath_providers::provider::build_http_client,
    )
    .await
}

/// How long one provider gets to report its model list at start-up.
///
/// Short on purpose: this is the daemon's start-up path, and the answer is an
/// optimisation over the table compiled into this build, not a requirement. A
/// provider that cannot answer in this long is better skipped than allowed to
/// hold up every command waiting on the daemon.
const PROVIDER_PRIME_TIMEOUT_SECS: u64 = 10;

/// [`setup_daemon_host`], with outbound-client construction injected so the
/// start-up failure path is reachable from a test.
pub(crate) async fn setup_daemon_host_with(
    config: Config,
    (runs_dir, board): (std::path::PathBuf, &StartupBoard),
    runtime: Handle,
    build_client: leviath_providers::provider::HttpClientFactory<'_>,
) -> anyhow::Result<WorldHost> {
    // Apply the machine-wide outbound-network policy before anything can fetch.
    // These live in process-wide atomics because the shared blocking HTTP
    // client's redirect policy and per-host gate have no per-agent context to
    // consult; see `script_host::mirror_process_policy`.
    crate::daemon::script_host::mirror_process_policy(&config);
    // Built before the registry, and shared with it: a script provider's
    // `[model_providers.<name>]` table is read through this, so editing it
    // takes effect on the next load exactly as editing the `.rhai` beside it
    // does.
    //
    // The same mirror is re-applied on every reload, because those three
    // atomics have no other way to follow the file. The per-agent
    // `allow_local_network` check beside them reads the reloaded config, so
    // atomics left at their boot values would refuse the URL a script names
    // while still following a redirect to loopback.
    let reloader = Arc::new(
        crate::daemon::config_reload::ConfigReloader::new(Config::config_path(), config.clone())
            .with_reload_hook(Box::new(crate::daemon::script_host::mirror_process_policy)),
    );
    let providers = crate::commands::run::session::build_provider_registry_live(
        &config,
        reloader.clone(),
        build_client,
    )?;
    // Ask each provider what its models are before anything runs on one.
    // `capabilities()` is synchronous and sits on the inference path, so a
    // provider whose real answer needs a network call has to be told here or
    // never - and "never" means an OpenRouter model this build's table does not
    // name silently gets a 128 000-token window, with every percentage region
    // budget sized against it. Awaited rather than spawned so the first run has
    // the answer instead of racing it; failures are warnings.
    board.begin("asking providers which models they serve", 0);
    let prime_failures = providers
        .prime_capabilities(
            std::time::Duration::from_secs(PROVIDER_PRIME_TIMEOUT_SECS),
            // The machine's default, so a script provider named there can
            // answer what it serves and win an open route. No effect when the
            // default is a native provider, which is already in the list.
            &[config.default_provider.as_str()],
        )
        .await;
    // Write what the prime learned to the shared capability cache, so a
    // short-lived `lev models`, `lev validate` or serve handler answers a
    // model's real limits from the same numbers instead of re-priming to its own
    // conservative default. Best-effort: a home that does not resolve (path is
    // `None`), or a file that cannot be written, just leaves the pre-cache
    // behaviour in place.
    let cache_path = leviath_core::paths::capability_cache_path();
    providers.save_capability_cache(
        cache_path.as_deref(),
        chrono::Utc::now().timestamp(),
        &crate::provider_checks::fingerprints(cache_path.as_deref(), &config),
        &prime_failures,
    );
    // Keeps that registry in step with `config.toml` from here on: a run
    // started after a `lev setup`, a `PUT /api/config` or a hand edit resolves
    // against the providers the file names now, without a daemon restart.
    let provider_reload = crate::daemon::provider_reload::for_daemon(&config, providers.clone());
    // A list the prime could not read is answered from the cache's copy of it
    // until the live one comes back, so a proxy that was down at the wrong
    // moment costs nothing. After the cache was written above, and before the
    // restart recovery in `build_host`, so a resumed run resolves against it.
    crate::daemon::catalog_refresh::settle_at_start(&provider_reload);
    let refresher_reload = provider_reload.clone();
    let refresher_config = reloader.clone();
    // MCP connections are shared across agents; the workdir here only seeds the
    // (discarded) built-ins - each agent gets its own over its own workdir.
    board.begin("connecting MCP servers", 0);
    let registry = ToolRegistry::build(std::env::temp_dir(), &config).await;
    // The shared MCP pool: seed the connected global servers, then reconnect the
    // per-agent MCP servers of any unfinished run so a run resumed on restart
    // can still call its blueprint's MCP tools. Done here, before the
    // synchronous resume inside build_host, as the starter does for a new run.
    let mcp_pool = crate::daemon::mcp_pool::McpPool::for_daemon_with(
        registry.mcp.clone(),
        &config.mcp_servers,
        config.security.credential_store,
        config.security.allow_env_vars.clone(),
        config.limits.mcp_idle_disconnect_secs,
    );
    // File each global server's own defs under its signature. They were
    // connected a moment ago by `ToolRegistry::build`, and the pool has to know
    // what each one contributed for `mcp_reload` to reconcile the set later
    // without keeping a second list of the same tools.
    mcp_pool.seed_all(
        &config.mcp_servers,
        &registry.mcp_tool_defs,
        &registry.mcp_tool_owners,
    );
    // Old run directories become run files first, each stage looked up
    // against this machine's providers and tools, so the servers they declare
    // are warmed with everyone else's.
    let agents_dir = leviath_core::paths::agents_dir();
    // Blueprints a previous release installed become `agent.toml` first, so
    // they are found by name and an old run's workers are pinned to them.
    let mut upgrade = crate::daemon::upgrade::Upgrade::default();
    crate::blueprint_upgrade::upgrade_at_start(
        &runs_dir,
        &crate::runstate::runs_dir(),
        (agents_dir.as_deref(), &config.agent_paths),
        board,
        &mut upgrade,
    );
    let runs = crate::daemon::convert_old::convert_at_start(
        &runs_dir,
        crate::daemon::convert_old::AtStart {
            config: &config,
            registry: provider_reload.registry(),
            agents_dir: agents_dir.as_deref(),
            mcp_defs: &registry.mcp_tool_defs,
            mcp_owners: &registry.mcp_tool_owners,
            shared_mcp: registry.mcp.clone(),
            pool: &mcp_pool,
            // The child is this executable; one that cannot be found
            // converts in the daemon.
            #[cfg(feature = "legacy-runs")]
            child: std::env::current_exe().ok().map(|exe| {
                crate::daemon::convert_old::ChildCmd::lev(exe, &runs_dir, agents_dir.as_deref())
            }),
        },
        board,
    )
    .await;
    upgrade.add_runs(runs);
    upgrade.finish(&crate::home_backup::Backup::of_runs(&runs_dir));
    board.begin("bringing back unfinished runs", 0);
    mcp_pool.warm_recovered(&runs_dir).await;
    // The index `lev ps` and the dashboard list runs from follows the runs
    // as they change.
    crate::run_index::keep_fresh(&runtime, runs_dir.clone());
    let refresher_runtime = runtime.clone();
    let host = build_host(HostParts {
        config,
        providers,
        runs_dir,
        shared_mcp: registry.mcp,
        mcp_tool_defs: registry.mcp_tool_defs,
        mcp_tool_owners: registry.mcp_tool_owners,
        mcp_pool,
        runtime,
        now_secs: || chrono::Utc::now().timestamp(),
        reloader: Some(reloader),
        provider_reload: Some(provider_reload),
    });
    // Every list not read live at start is asked for until it answers, and
    // then every list now and then.
    crate::daemon::catalog_refresh::spawn(
        &refresher_runtime,
        refresher_reload,
        refresher_config,
        crate::daemon::catalog_refresh::Pacing::DAEMON,
    );
    Ok(host)
}

/// The reap hook installed on the host: drops a reaped agent's tool state and
/// tears down its sandbox via [`CliToolService::reap`]. Factored out (rather than
/// an inline closure) so its body is exercised by a unit test - the daemon itself
/// only ever fires the reaper from the private `serve()` loop.
fn make_reaper(
    tool_service: Arc<CliToolService>,
    mcp_pool: Arc<crate::daemon::mcp_pool::McpPool>,
) -> leviath_runtime::host::Reaper {
    Box::new(move |world, entity| {
        // Release the run's MCP leases before the entity goes away; servers
        // nobody else holds get an idle-disconnect timer.
        if let Some(lease) = world
            .world()
            .get::<crate::daemon::mcp_pool::McpLease>(entity)
        {
            mcp_pool.release(lease);
        }
        // A finished run deletes what it put in providers' file storage.
        leviath_runtime::provider_files::forget_finished(world.world(), entity);
        tool_service.reap(entity)
    })
}

/// The resume hook installed on the host: re-reads the config layers an agent
/// resolved at spawn, against `config.toml` as it stands now.
///
/// Which is the whole point of the hook. A run parked on a tool it is not
/// permitted to call, on a path it may not read, or against a write ceiling it
/// has hit could not be freed by editing the file those come from: the answer
/// was resolved once at spawn, so the only way out was to cancel the run and
/// start it again, losing everything it had done. With an unanswered approval
/// prompt waiting for ever by default, that stall never ends on its own.
///
/// A run that is not paused and not being paged in never reaches here, so a
/// stage under way keeps the snapshot it started on.
///
/// Factored out (rather than an inline closure) so its body is exercised by a
/// unit test - the daemon only ever fires it from the private `serve()` loop.
fn make_resumer(
    tool_service: Arc<CliToolService>,
    reloader: Arc<crate::daemon::config_reload::ConfigReloader>,
) -> leviath_runtime::host::Resumer {
    Box::new(move |_world, entity| {
        let Some(state) = tool_service.state_for(entity) else {
            // Not every resumed entity has tool state: a fan-out parent that
            // has been paged back in registers its workers as it goes.
            return;
        };
        state.reread_config(&reloader.current());
    })
}

/// The hook the host runs on every safety re-drive: the config as it stands
/// on disk now, applied to the world where it differs from what is in it.
/// One stat of `config.toml` and one read of `mime_types.toml` when nothing
/// changed. Factored out so the closure body is unit-testable.
fn make_housekeeper(
    reloader: Arc<crate::daemon::config_reload::ConfigReloader>,
    live_limits: Arc<crate::daemon::live_limits::LiveLimits>,
) -> leviath_runtime::host::Housekeeper {
    Box::new(move |world| {
        live_limits.apply(&reloader.current(), world);
    })
}

/// Everything the daemon hands its world host at construction.
///
/// A struct rather than eight positional parameters because these are not
/// arguments in the usual sense: each is a resource the host owns for the rest
/// of the process's life. Naming them here describes the daemon; listing them
/// at the call site describes nothing.
pub struct HostParts {
    /// The resolved configuration this daemon booted with.
    pub config: Config,
    /// Providers built from that config, keyed by name.
    pub providers: ProviderRegistry,
    /// Where run state is persisted.
    pub runs_dir: std::path::PathBuf,
    /// MCP connections shared across every agent.
    pub shared_mcp: Arc<Mutex<leviath_mcp::ToolExecutor>>,
    /// The tools those servers advertise.
    pub mcp_tool_defs: Vec<Tool>,
    /// Which MCP server advertises each of those.
    pub mcp_tool_owners: leviath_runtime::pipeline::ToolOwners,
    /// The pool that keeps per-agent MCP servers warm.
    pub mcp_pool: Arc<crate::daemon::mcp_pool::McpPool>,
    /// The tokio runtime the async lanes run on.
    pub runtime: Handle,
    /// The clock, injected so a test does not depend on the wall clock.
    pub now_secs: fn() -> i64,
    /// The daemon's config source. Built before the provider registry so the
    /// script-provider layer can follow it too; `None` builds a fixed one from
    /// `config`, for a caller with nothing to watch.
    pub reloader: Option<Arc<crate::daemon::config_reload::ConfigReloader>>,
    /// Keeps the provider registry in step with `config.toml`. `None` builds
    /// one over `config` and `providers`, which is the same thing for a caller
    /// that booted them together.
    pub provider_reload: Option<Arc<crate::daemon::provider_reload::ProviderReload>>,
}

/// Build the daemon's [`WorldHost`]: one world hosting every agent, its tool
/// service + interaction hub, and the starter every run goes through. The MCP
/// connections in [`HostParts`] are shared: every agent dispatches through the
/// one pool.
pub fn build_host(parts: HostParts) -> WorldHost {
    let hub = InteractionHub::new();
    let tool_service = Arc::new(CliToolService::new());
    // The configured global fallback bounds concurrent inference for any model
    // without its own per-model pool entry (defaults to a small cap so a fresh
    // install can't fan out unbounded requests against provider rate limits),
    // alongside the per-model overrides and the per-provider caps.
    let pool_config = parts.config.limits.inference_pools();
    // Kept before the world takes ownership: the starter warms each run's
    // models through the same registry the world will infer on, so both see
    // one set of learned windows rather than two.
    let pp_providers = parts.providers.clone();
    let mut world = PipelineWorld::new(
        parts.providers,
        tool_service.clone(),
        pool_config,
        parts.config.limits.max_concurrent_tools,
        Some(parts.runs_dir.clone()),
        parts.runtime,
    );
    world
        .world_mut()
        .init_resource::<leviath_runtime::pipeline::ProviderCircuits>();
    // Share the hub with the tick loop so a blocked agent's open prompt is
    // reflected into its status (Active ↔ Waiting) for the dashboard to surface.
    world.insert_interaction_hub(hub.clone());
    let mut host = WorldHost::with_interactions(world, hub.clone());
    // Everything the world is *tuned* with - the pools and the tool lane, the
    // stall and wedge watchdogs, the circuit breaker, the retry schedule, the
    // fan-out ceiling, the relief threshold, the spend figures, the listing
    // window, the prompt timeout and `[title]` - is installed from one place,
    // and reinstalled from the same place whenever `config.toml` changes, so an
    // edit to any of them lands without a daemon restart.
    let live_limits = crate::daemon::live_limits::for_daemon(hub.clone(), host.settings());
    live_limits.apply(&parts.config, host.world_mut());
    // Handed to each agent's tool state so its sub-agent tools reach the world
    // through the host.
    let subagent_tx = host.subagent_sender();

    // Config hot-reload: after boot, spawn-time parts.config (permissions,
    // `[read_paths]`, sandbox, limits, taint) is served from here, reloaded
    // when `config.toml` changes on disk. The reloader takes a clone, so
    // `parts.config` stays usable below as the boot snapshot the rest of this
    // wiring reads. The telemetry sink follows the reloaded config too, through
    // `telemetry_reload` below.
    //
    // Normally handed in: `setup_daemon_host_with` builds it before the
    // provider registry so the script-provider layer can follow it as well. A
    // caller that did not gets a fixed one over its own config.
    let reloader = parts.reloader.clone().unwrap_or_else(|| {
        std::sync::Arc::new(crate::daemon::config_reload::ConfigReloader::new(
            Config::config_path(),
            parts.config.clone(),
        ))
    });

    // Same fallback as the config reloader above: a caller that booted its
    // registry and its config together is already consistent, so one built
    // over both is the right starting point.
    let provider_reload = parts.provider_reload.clone().unwrap_or_else(|| {
        crate::daemon::provider_reload::for_daemon(&parts.config, pp_providers.clone())
    });

    // The global `[[mcp_servers]]` as a live set rather than the boot copy.
    // Reconciled against the reloaded config before each spawn, so `lev mcp
    // add`, `POST /api/mcp/servers` and a hand edit all reach the next run.
    let mcp_global = crate::daemon::mcp_reload::McpReload::new(
        &parts.config,
        parts.mcp_pool.clone(),
        parts.mcp_tool_defs.clone(),
        parts.mcp_tool_owners.clone(),
    );

    // The taint gate's two files: the tool allowlist (`policy.toml`) and the
    // scripted rules (`<config>/leviath/rules/*.rhai`). Reading them is this
    // reload's first refresh, so the boot install and every later one go
    // through the same seam and an edit reaches the next run without a daemon
    // restart. A malformed `policy.toml` falls back to an empty policy
    // (deny-by-clearance only) rather than failing startup.
    let policy_reload = crate::daemon::policy_reload::PolicyReload::for_daemon();
    policy_reload.install(host.world_mut());

    // Run-title generation settings; spawn only marks a run for titling when
    // `[title]` is enabled, and the dispatch system reads provider/model here.
    host.world_mut()
        .world_mut()
        .insert_resource(leviath_runtime::title::TitleSettings(
            parts.config.title.clone(),
        ));

    // Structured observability (`[observability]`): replace the world's no-op
    // telemetry sink with the configured exporter, and - for OTLP - forward
    // the daemon's own tracing events through the same pipeline. A pipeline
    // that fails to build logs a warning and leaves the no-op in place -
    // observability must never stop the work it observes.
    //
    // Boot is this reload's first refresh, so turning export on, moving it to
    // another collector, or turning it off reaches the next run without a
    // daemon restart.
    let telemetry_reload = crate::daemon::telemetry_reload::TelemetryReload::for_daemon();
    telemetry_reload.refresh_into(host.world_mut(), &parts.config.observability);

    // What starts every run: a person's, a child an agent asks for, and a
    // fan-out worker. It shares the world's blob store, so a run's files are
    // where its tools look for them.
    let blob_store = host
        .world_mut()
        .world()
        .resource::<leviath_runtime::blob_store::BlobStoreHandle>()
        .0
        .clone();
    let starter = Arc::new(DaemonStarter {
        config: reloader.clone(),
        providers: provider_reload.clone(),
        policy: policy_reload.clone(),
        telemetry: telemetry_reload.clone(),
        limits: live_limits.clone(),
        mcp_global,
        mcp_pool: parts.mcp_pool.clone(),
        shared_mcp: parts.shared_mcp.clone(),
        tool_service: tool_service.clone(),
        hub: hub.clone(),
        subagent_tx,
        runs_dir: parts.runs_dir.clone(),
        agents_dir: leviath_core::paths::agents_dir(),
        blob_store,
    });

    // Install the fan-out spawner as a world resource so the fan-out systems
    // can start workers through the same starter.
    host.world_mut()
        .world_mut()
        .insert_resource(FanOutSpawnerRes(Arc::new(DaemonFanOutSpawner {
            starter: starter.clone(),
            agents_dir: leviath_core::paths::agents_dir(),
        })));

    // Restart recovery: every unfinished run comes back from its run file, so
    // interrupted runs (including mid-inference ones) resume.
    let recovered =
        crate::daemon::recovery::resume_all(host.world_mut(), &starter, &parts.runs_dir);
    for (run_id, entity) in recovered.reloaded {
        host.register(run_id, entity);
    }
    for entry in recovered.held {
        host.hold(entry);
    }

    // Reload-on-demand: an op targeting an unloaded run pages it back in from
    // its run file, against the machine as it stands now.
    host.set_reloader(crate::daemon::recovery::reloader(starter.clone()));

    // Last resort for a cancel the world can't service: force the run's on-disk
    // state to `Cancelled`. The reloader above declines whenever a run can't be
    // rebuilt - deleted blueprint, unreadable metadata, died mid-spawn - and
    // without this a cancel in that state writes nothing at all, leaving
    // the run file claiming the run is live with nothing able to clear it.
    let terminate_runs = parts.runs_dir.clone();
    host.set_force_terminator(Box::new(move |run_id| {
        crate::runstate::force_cancel_in(&terminate_runs.join(run_id), (parts.now_secs)())
            .found_run()
    }));

    // Reap hook: when a terminal agent is reaped, tear down its sandbox and drop
    // its per-agent tool state, which nothing else releases. Factored into
    // `make_reaper` so the closure body is unit-testable - the daemon only ever
    // drives it from `serve()`.
    host.set_reaper(make_reaper(tool_service.clone(), parts.mcp_pool.clone()));

    // Resume hook: a run that starts moving again re-reads the config layers a
    // person edits to unblock it. Without it a run parked on a denied tool has
    // no way out but a cancel, since its permissions were resolved at spawn and
    // an unanswered prompt waits for ever by default.
    host.set_resumer(make_resumer(tool_service.clone(), reloader.clone()));

    // Housekeeping: on the host's own timer, whether or not a spawn comes
    // along, re-read the config layers that reach runs already under way.
    // This is what makes an edit to `mime_types.toml` (or a `[limits]` key)
    // land in a live run within one re-drive interval rather than waiting
    // for the next `lev run` to walk the spawn path.
    host.set_housekeeper(make_housekeeper(reloader.clone(), live_limits.clone()));

    host.set_starter(starter);
    host
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::starter::testing::{
        blueprint_with_mcp, run_on_disk, stub_server_py, task_request,
    };
    use crate::test_support::{FakeProvider, fixtures};
    use leviath_runtime::components::AgentStatus;
    use leviath_runtime::host::ControlOp;
    use leviath_runtime::spec::issues::SpawnIssues;
    use leviath_runtime::spec::names::RunId;
    use leviath_runtime::spec::request::{SpawnRequest, SpawnSource};
    use tokio::sync::oneshot;

    /// Ask `host` to start `request`, and wait for the answer.
    async fn spawn_through(
        host: &mut WorldHost,
        request: SpawnRequest,
    ) -> Result<RunId, SpawnIssues> {
        let (reply, rx) = oneshot::channel();
        host.handle(ControlOp::Spawn {
            request: Box::new(request),
            reply,
        });
        host.finish_starts().await;
        rx.await.unwrap().map(|spawned| spawned.run_id)
    }

    /// A config whose registry actually has `anthropic` in it, so a spawn of a
    /// manifest naming that provider is not refused for having none.
    fn config_with_anthropic_key() -> Config {
        let mut config = Config::default();
        config.providers.anthropic_api_key = Some("test-key".to_string());
        config
    }

    /// The resume hook the daemon installs, driven the way `serve()` drives
    /// it. Both arms in one test: an entity with no tool state (a fan-out
    /// parent paged back in before its workers register) is a no-op, and one
    /// with state has its config layers re-read.
    /// The housekeeper applies the config as it stands on disk, so an edit
    /// to `mime_types.toml` reaches the world on the next pass with no
    /// spawn to carry it.
    #[tokio::test]
    async fn make_housekeeper_applies_the_files_on_disk() {
        crate::config::with_isolated_config_path_async("housekeeper", |dir| async move {
            let world_config = Config::default();
            let mut world = PipelineWorld::new(
                ProviderRegistry::new(),
                Arc::new(CliToolService::new()),
                leviath_runtime::inference_pool::InferencePoolConfig::new(),
                1,
                None,
                Handle::current(),
            );
            let config_path = dir.join("config.toml");
            std::fs::write(&config_path, toml::to_string(&world_config).unwrap()).unwrap();
            let reloader = Arc::new(crate::daemon::config_reload::ConfigReloader::new(
                config_path,
                world_config,
            ));
            let live = crate::daemon::live_limits::for_daemon(
                InteractionHub::new(),
                leviath_runtime::host::HostSettings::default(),
            );
            let mut housekeeper = make_housekeeper(reloader, live);
            let obj: leviath_core::mime::MimeType = "model/obj".parse().unwrap();
            let family = |world: &PipelineWorld| {
                world
                    .world()
                    .get_resource::<leviath_runtime::blob_store::MimeRegistryHandle>()
                    .expect("the first pass installs the registry")
                    .0
                    .info(&obj)
                    .family
                    .clone()
            };
            housekeeper(&mut world);
            assert_eq!(family(&world), "model");
            std::fs::write(
                dir.join("mime_types.toml"),
                "[\"model/obj\"]\nfamily = \"scene\"\n",
            )
            .unwrap();
            housekeeper(&mut world);
            assert_eq!(
                family(&world),
                "scene",
                "the file edit landed with no spawn"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn make_resumer_rereads_the_config_of_a_registered_agent() {
        let tool_service = Arc::new(CliToolService::new());
        let mut world = PipelineWorld::new(
            ProviderRegistry::new(),
            tool_service.clone(),
            leviath_runtime::inference_pool::InferencePoolConfig::new(),
            1,
            None,
            Handle::current(),
        );
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, toml::to_string(&Config::default()).unwrap()).unwrap();
        let reloader = Arc::new(crate::daemon::config_reload::ConfigReloader::new(
            config_path.clone(),
            Config::default(),
        ));
        let mut resumer = make_resumer(tool_service.clone(), reloader);

        let entity = bevy_ecs::entity::Entity::from_raw_u32(1)
            .expect("a small literal index is always a valid entity id");
        // No state registered: nothing to re-read, and nothing panics.
        resumer(&mut world, entity);

        // With state, the layers follow the file. `lev resume` is what makes
        // the daemon read it again.
        let state = crate::daemon::tool_service::test_state_for_resume(dir.path());
        tool_service.register(entity, state.clone());
        assert!(!state.blueprint_may_loosen());
        let mut edited = Config::default();
        edited.security.allow_blueprint_permissions = true;
        std::fs::write(&config_path, toml::to_string(&edited).unwrap()).unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&config_path)
            .unwrap()
            .set_modified(later)
            .unwrap();

        resumer(&mut world, entity);
        assert!(
            state.blueprint_may_loosen(),
            "the resume reads config.toml as it stands now, not as the spawn read it"
        );
    }

    #[tokio::test]
    async fn make_reaper_delegates_to_tool_service_reap() {
        // Exercises the reaper closure body build_host installs. The daemon only
        // fires it from the private `serve()` loop, so drive it directly here.
        let tool_service = Arc::new(CliToolService::new());
        let mut world = PipelineWorld::new(
            ProviderRegistry::new(),
            tool_service.clone(),
            leviath_runtime::inference_pool::InferencePoolConfig::new(),
            1,
            None,
            Handle::current(),
        );
        let pool = crate::daemon::mcp_pool::McpPool::for_daemon(
            Arc::new(tokio::sync::Mutex::new(leviath_mcp::ToolExecutor::new())),
            &[],
        );
        let mut reaper = make_reaper(tool_service.clone(), pool.clone());
        // No registered state for this entity → a clean no-op (the reap-branch
        // logic itself is covered by CliToolService::reap's own unit test).
        let entity = bevy_ecs::entity::Entity::from_raw_u32(1)
            .expect("a small literal index is always a valid entity id");
        reaper(&mut world, entity);
        assert!(tool_service.take(entity).is_none());

        // An entity that holds MCP servers hands them back on reap.
        let server = leviath_mcp::MCPServerConfig::stdio("held", "true", vec![]);
        let lease = pool.lease_servers(std::slice::from_ref(&server), "reaped-run");
        assert_eq!(pool.leased_holders(&server), 1);
        let with_meta = world.spawn_agent((
            lease,
            leviath_runtime::persistence::RunMetadata {
                run_id: "reaped-run".to_string(),
                agent_name: "a".to_string(),
                agent_path: "/p".to_string(),
                task: "t".to_string(),
                model: None,
                workdir: "/w".to_string(),
                num_stages: 1,
                started_at: 0,
                parent_run_id: None,
                metadata: std::collections::HashMap::new(),
                callback_url: None,
                callback_secret: None,
                title: None,
                title_error: None,
                blueprint_digest: None,
                unattended: leviath_core::Unattended::Off,
                read_paths: None,
                output_request: None,
                model_override: None,
            },
        ));
        reaper(&mut world, with_meta.entity());
        assert!(tool_service.take(with_meta.entity()).is_none());
        assert_eq!(
            pool.leased_holders(&server),
            0,
            "the reap released the run's servers"
        );
    }

    fn fake_provider() -> FakeProvider {
        FakeProvider::new()
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
        // The host's reloader watches `Config::config_path()`, which the
        // isolated tests point at their own scratch files; read in one of their
        // windows, this host would adopt a neighbour's config on its next
        // spawn. So this test holds a path of its own for its whole run.
        let _redirect = crate::daemon::script_host::REDIRECT_MIRROR.lock().await;
        crate::config::with_isolated_config_path_async(
            "setup_daemon_host_builds_a_working_host",
            |_| async move {
                // `setup_daemon_host_with` mirrors `[security] allow_local_network`
                // into a process-wide atomic, which makes this test a writer of the
                // switch the script-host redirect tests read. Without the lock, standing
                // up a host here flipped that switch mid-request over there and the
                // refusal it saw was this test's config, not its own.
                // Config::default has no MCP servers → the shared MCP connect is a no-op.
                // An empty runs dir → restart recovery finds nothing to reload.
                // A key for the manifest's provider, because a spawn whose stages have
                // no usable provider is refused outright.
                let runs = tempfile::tempdir().unwrap();
                let mut host = setup_daemon_host(
                    config_with_anthropic_key(),
                    runs.path().to_path_buf(),
                    Handle::current(),
                    &StartupBoard::default(),
                )
                .await
                .expect("the daemon host builds in tests");

                // Spawning through the wired host exercises the real setup end to end.
                let dir = tempfile::tempdir().unwrap();
                let manifest = dir.path().join("agent.toml");
                std::fs::write(&manifest, crate::test_support::inline_coder_manifest()).unwrap();
                let run_id = spawn_through(&mut host, task_request(&manifest, "t"))
                    .await
                    .expect("the run starts");
                assert!(
                    runs.path()
                        .join(run_id.as_str())
                        .join(leviath_core::files::RUN_FILE)
                        .is_file(),
                    "and is recorded in its run file"
                );
            },
        )
        .await;
    }

    /// A spawn of a blueprint that is not there is refused before anything is
    /// written: no run id is taken and no run directory is left behind for
    /// a listing to show as a run that never moves.
    #[tokio::test]
    async fn a_refused_spawn_says_why_and_leaves_nothing_on_disk() {
        let _redirect = crate::daemon::script_host::REDIRECT_MIRROR.lock().await;
        crate::config::with_isolated_config_path_async(
            "a_refused_spawn_says_why_and_leaves_nothing_on_disk",
            |_| async move {
                // A writer of the redirect mirror, like every test that stands up a
                // host: see `setup_daemon_host_builds_a_working_host`.
                let runs = tempfile::tempdir().unwrap();
                let mut host = setup_daemon_host(
                    Config::default(),
                    runs.path().to_path_buf(),
                    Handle::current(),
                    &StartupBoard::default(),
                )
                .await
                .expect("the daemon host builds in tests");
                let gone = runs.path().join("no-such-blueprint");
                let request = SpawnRequest::new(SpawnSource::BlueprintFile(
                    leviath_runtime::spec::names::BlueprintPath::new(gone.to_string_lossy())
                        .unwrap(),
                ));
                let issues = spawn_through(&mut host, request)
                    .await
                    .expect_err("there is no such blueprint");
                assert!(
                    issues.to_string().contains("no-such-blueprint"),
                    "it says what went wrong: {issues}"
                );
                assert!(run_ids_in(runs.path()).is_empty());
            },
        )
        .await;
    }

    /// A run is recorded under the **host's configured** `runs_dir`, never the
    /// home-resolved global one.
    ///
    /// This is an isolation invariant, not a convenience: `runstate::run_dir()`
    /// goes through `dirs::home_dir()`, which ignores a `$HOME` override on macOS,
    /// so a spawn that used it would write into the developer's real
    /// `~/.leviath/runs` from any test that drove a real host. Asserting the
    /// global dir is untouched is what keeps that from happening.
    #[tokio::test]
    async fn a_spawn_records_its_run_under_the_hosts_runs_dir() {
        // A writer of the redirect mirror, like every test that stands up a
        // host: see `setup_daemon_host_builds_a_working_host`.
        let _redirect = crate::daemon::script_host::REDIRECT_MIRROR.lock().await;
        let runs = tempfile::tempdir().unwrap();
        // The assertion below is "spawning wrote nothing into the *global* runs
        // dir", which is only decidable if no other test can write there while
        // this one runs. `with_isolated_runs_dir_async` points `LEVIATH_RUNS_DIR`
        // at a directory only this test can reach, and `temp_env` serialises the
        // change process-wide, so the comparison is deterministic.
        crate::runstate::with_isolated_runs_dir_async(
            "setup-host-isolation",
            |global| async move {
                let global_before = run_ids_in(&global);

                let mut host = setup_daemon_host(
                    config_with_anthropic_key(),
                    runs.path().to_path_buf(),
                    Handle::current(),
                    &StartupBoard::default(),
                )
                .await
                .expect("the daemon host builds in tests");
                let agent = tempfile::tempdir().unwrap();
                let manifest = agent.path().join("agent.toml");
                std::fs::write(&manifest, crate::test_support::inline_coder_manifest()).unwrap();
                let run_id = spawn_through(&mut host, task_request(&manifest, "t"))
                    .await
                    .expect("the run starts");

                assert_eq!(
                    run_ids_in(runs.path()),
                    [run_id.to_string()].into_iter().collect(),
                    "the run lands in the host's configured runs dir"
                );
                assert_eq!(
                    run_ids_in(&global),
                    global_before,
                    "spawning through a host must not write into the home-resolved runs dir"
                );
            },
        )
        .await;
    }

    /// End-to-end for a run the daemon is not holding: its blueprint is gone,
    /// but its run file holds everything a resume needs, so the cancel pages
    /// it in and cancels it there, and the cancel reaches its file. A run id
    /// that names nothing is still an honest miss.
    #[tokio::test]
    async fn cancelling_an_unreloadable_run_terminates_it_on_disk() {
        let _redirect = crate::daemon::script_host::REDIRECT_MIRROR.lock().await;
        crate::config::with_isolated_config_path_async(
            "cancelling_an_unreloadable_run_terminates_it_on_disk",
            |_| async move {
                // A writer of the redirect mirror, like every test that stands up a
                // host: see `setup_daemon_host_builds_a_working_host`.
                let runs = tempfile::tempdir().unwrap();
                let mut host = setup_daemon_host(
                    Config::default(),
                    runs.path().to_path_buf(),
                    Handle::current(),
                    &StartupBoard::default(),
                )
                .await
                .expect("the daemon host builds in tests");

                // Staked out *after* startup, so the recovery sweep (which marks
                // un-reloadable runs as crashed) hasn't already dealt with it - this is
                // the live case: the daemon is up and the run cannot be paged in.
                let run_dir = runs.path().join("gone-1234-ab12");
                let meta = leviath_core::run_meta::RunMeta::new(
                    "gone-1234-ab12".to_string(),
                    "gone".to_string(),
                    // A blueprint path that does not exist - the deleted-manifest case.
                    "/no/such/dir/agent.toml".to_string(),
                    "t".to_string(),
                    None,
                    std::env::temp_dir().to_string_lossy().to_string(),
                    1,
                );
                crate::runstate::create_run_in(&run_dir, &meta).unwrap();
                assert!(
                    !crate::runstate::is_terminal_status(
                        &crate::runstate::read_meta_from(&run_dir).unwrap().status
                    ),
                    "the run starts out looking live"
                );

                let (reply, rx) = oneshot::channel();
                host.handle(ControlOp::Cancel {
                    run_id: "gone-1234-ab12".to_string(),
                    reply,
                });
                host.land_pages().await;
                assert!(rx.await.unwrap(), "the cancel reports that it applied");

                // A run id that names nothing at all is still an honest miss.
                let (reply, rx) = oneshot::channel();
                host.handle(ControlOp::Cancel {
                    run_id: "no-such-run".to_string(),
                    reply,
                });
                host.land_pages().await;
                assert!(!rx.await.unwrap());

                // A closed control channel ends the serve loop, which writes
                // what is queued before it returns.
                let (control, control_rx) = tokio::sync::mpsc::unbounded_channel();
                drop(control);
                host.serve(control_rx).await;
                assert_eq!(
                    crate::runstate::read_meta_from(&run_dir).unwrap().status,
                    leviath_core::run_meta::RunStatus::Cancelled,
                    "and it reached disk, so nothing shows the run as live any more"
                );
            },
        )
        .await;
    }

    /// The run ids present in `dir`. An unreadable or absent directory is an
    /// empty set, which is the same assertion for the isolation check.
    fn run_ids_in(dir: &std::path::Path) -> std::collections::BTreeSet<String> {
        // Runs are directories; the daemon's own session mark beside them
        // is not one.
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn run_ids_in_lists_entries_and_tolerates_a_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("run-one")).unwrap();
        std::fs::create_dir_all(dir.path().join("run-two")).unwrap();
        assert_eq!(
            run_ids_in(dir.path()),
            ["run-one".to_string(), "run-two".to_string()]
                .into_iter()
                .collect()
        );
        // A dir that doesn't exist reads as "nothing there", not a panic.
        assert!(run_ids_in(&dir.path().join("nope")).is_empty());
    }

    fn empty_pool() -> crate::daemon::mcp_pool::McpPool {
        crate::daemon::mcp_pool::McpPool::new(
            Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
            Default::default(),
        )
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
        let _host = build_host(HostParts {
            config,
            providers: ProviderRegistry::new(),
            runs_dir: runs.path().to_path_buf(),
            shared_mcp: Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
            mcp_tool_defs: Vec::new(),
            mcp_tool_owners: Default::default(),
            mcp_pool: crate::daemon::mcp_pool::McpPool::for_daemon(
                Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
                &[],
            ),
            runtime: Handle::current(),
            now_secs: || 0,
            reloader: None,
            provider_reload: None,
        });
    }

    #[tokio::test]
    async fn build_host_installs_the_configured_telemetry_sink() {
        // `[observability] enabled + stdout` replaces the world's no-op sink.
        let config = Config {
            observability: leviath_core::config::ObservabilityConfig {
                enabled: true,
                exporter: leviath_core::config::TelemetryExporterKind::Stdout,
                endpoint: None,
                service_name: None,
                log_file_max_bytes: leviath_core::config::DEFAULT_LOG_FILE_MAX_BYTES,
                capture_model_input: false,
            },
            ..Config::default()
        };
        let runs = tempfile::tempdir().unwrap();
        let mut host = build_host(HostParts {
            config,
            providers: ProviderRegistry::new(),
            runs_dir: runs.path().to_path_buf(),
            shared_mcp: Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
            mcp_tool_defs: Vec::new(),
            mcp_tool_owners: Default::default(),
            mcp_pool: crate::daemon::mcp_pool::McpPool::for_daemon(
                Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
                &[],
            ),
            runtime: Handle::current(),
            now_secs: || 0,
            reloader: None,
            provider_reload: None,
        });
        assert!(
            host.world_mut()
                .world_mut()
                .get_resource::<leviath_runtime::telemetry::Telemetry>()
                .is_some()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn build_host_with_otlp_also_installs_the_log_layer() {
        // The OTLP exporter carries a daemon-log bridge layer; build_host must
        // route it into the logging reload slot (a no-op when no subscriber
        // slot exists, as in this test process - the routing is the point).
        // Port 9 (discard) is never connected until an export flush happens,
        // which this test doesn't trigger.
        let config = Config {
            observability: leviath_core::config::ObservabilityConfig {
                enabled: true,
                exporter: leviath_core::config::TelemetryExporterKind::Otlp,
                endpoint: Some("http://127.0.0.1:9".to_string()),
                service_name: Some("leviath-test".to_string()),
                log_file_max_bytes: leviath_core::config::DEFAULT_LOG_FILE_MAX_BYTES,
                capture_model_input: false,
            },
            ..Config::default()
        };
        let runs = tempfile::tempdir().unwrap();
        let mut host = build_host(HostParts {
            config,
            providers: ProviderRegistry::new(),
            runs_dir: runs.path().to_path_buf(),
            shared_mcp: Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
            mcp_tool_defs: Vec::new(),
            mcp_tool_owners: Default::default(),
            mcp_pool: crate::daemon::mcp_pool::McpPool::for_daemon(
                Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
                &[],
            ),
            runtime: Handle::current(),
            now_secs: || 0,
            reloader: None,
            provider_reload: None,
        });
        assert!(
            host.world_mut()
                .world_mut()
                .get_resource::<leviath_runtime::telemetry::Telemetry>()
                .is_some()
        );
    }

    /// A run started through `serve()` whose blueprint declares an MCP server
    /// has that server connected before it is bound, so its tools are there
    /// for its first stage.
    #[tokio::test]
    async fn a_spawn_connects_the_blueprints_mcp_servers_first() {
        crate::config::with_isolated_config_path_async(
            "a_spawn_connects_the_blueprints_mcp_servers_first",
            |_| async move {
                let (_stub_dir, stub) = stub_server_py();
                let agent_dir = tempfile::tempdir().unwrap();
                let manifest = blueprint_with_mcp(agent_dir.path(), &stub);
                // A `fake` provider so stage resolution succeeds.
                let mut providers = ProviderRegistry::new();
                providers.register("fake".to_string(), Arc::new(fake_provider()));
                let runs = tempfile::tempdir().unwrap();
                let pool = crate::daemon::mcp_pool::McpPool::for_daemon(
                    Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
                    &[],
                );
                let mut host = build_host(HostParts {
                    config: Config::default(),
                    providers,
                    runs_dir: runs.path().to_path_buf(),
                    shared_mcp: Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
                    mcp_tool_defs: Vec::new(),
                    mcp_tool_owners: Default::default(),
                    mcp_pool: pool.clone(),
                    runtime: Handle::current(),
                    now_secs: || 0,
                    reloader: None,
                    provider_reload: None,
                });
                let (op, reply_rx) = spawn_op(&manifest);
                let (ctl_tx, ctl_rx) = tokio::sync::mpsc::unbounded_channel();
                ctl_tx.send(op).unwrap();
                // Close the control channel so serve() returns after handling the op.
                drop(ctl_tx);
                host.serve(ctl_rx).await;
                assert!(reply_rx.await.unwrap().is_ok());
                let file = leviath_blueprint::BlueprintFile::parse(
                    &std::fs::read_to_string(&manifest).unwrap(),
                )
                .unwrap();
                let servers = crate::daemon::starter::mcp_configs(&file.graph);
                let defs = pool.cached_defs_for(&servers);
                assert_eq!(defs.len(), 1);
                assert_eq!(defs[0].name, "search__stub_search");
            },
        )
        .await;
    }

    /// A daemon config routing bare model names through an OpenRouter at
    /// `url`, with a `fallback_model` a silent downgrade would land on.
    fn gateway_config(url: String) -> Config {
        let mut config = Config {
            default_provider: "openrouter".to_string(),
            fallback_model: Some("openrouter/anthropic/claude-sonnet-5".to_string()),
            openrouter_api_key: Some("sk-or-test".to_string()),
            ..Config::default()
        };
        config.providers.openrouter_base_url = Some(url);
        config
    }

    /// A one-stage blueprint naming `claude-opus-5` by bare name, in `dir`.
    fn story_manifest(dir: &std::path::Path) -> std::path::PathBuf {
        let manifest = dir.join("agent.toml");
        std::fs::write(
            &manifest,
            r#"[blueprint]
name = "storyteller"
version = "0.1.0"
description = "d"

[[graph.stages]]
name = "story"
description = "d"
model = { models = [{ model = "claude-opus-5" }] }

[graph.layout]
regions = [{ name = "task", kind = "pinned", budget = 2000 }]
total_budget_tokens = 2000

[[graph.inputs]]
name = "task"
type = { kind = "text", multiline = true }
binds = [{ region = "task" }]
"#,
        )
        .unwrap();
        manifest
    }

    /// A run of `manifest` started by a daemon whose gateway at `url`
    /// answered its model list, which stopped before the run made a call:
    /// what the next daemon finds in `runs`. The capability cache that daemon
    /// wrote is removed, so the next one starts with no copy of the list.
    async fn run_in_flight(
        runs: &std::path::Path,
        manifest: &std::path::Path,
        url: String,
    ) -> String {
        let mut host = setup_daemon_host(
            gateway_config(url),
            runs.to_path_buf(),
            Handle::current(),
            &StartupBoard::default(),
        )
        .await
        .expect("the first daemon starts");
        let run_id = spawn_through(&mut host, task_request(manifest, "t"))
            .await
            .expect("the run starts while the gateway answers");
        drop(host);
        let cache = leviath_core::paths::capability_cache_path().expect("an isolated home");
        let _ = std::fs::remove_file(cache);
        run_id.to_string()
    }

    /// The model list the gateway in these tests answers with.
    const LISTING: &[u8] =
        br#"{"data":[{"id":"anthropic/claude-opus-5","context_length":200000}]}"#;

    /// The model the stage `run_id` is in calls, `None` when the run is not
    /// in the world.
    fn model_of(host: &mut WorldHost, run_id: &str) -> Option<String> {
        let world = host.world_mut().world_mut();
        let mut runs = world.query::<(
            &leviath_runtime::insert::RunSpecC,
            &leviath_runtime::pipeline::StageInference,
        )>();
        runs.iter(world)
            .find(|(spec, _)| spec.0.run_id.as_str() == run_id)
            .map(|(_, inference)| inference.model.clone())
    }

    /// The status `run_id`'s file holds last.
    fn status_on_disk(runs: &std::path::Path, run_id: &str) -> leviath_runtime::state::RunStatus {
        leviath_runtime::runfile::RunFileReader::open(
            &runs.join(run_id).join(leviath_core::files::RUN_FILE),
        )
        .and_then(|r| r.latest_state())
        .expect("the run file reads")
        .status
    }

    /// The status the daemon reports for `run_id`, `None` when it holds none.
    async fn status_of(host: &mut WorldHost, run_id: &str) -> Option<AgentStatus> {
        let (reply, rx) = oneshot::channel();
        host.handle(ControlOp::Status {
            run_id: run_id.to_string(),
            reply,
        });
        rx.await.unwrap()
    }

    /// The same restart with a run in flight. The run chose its model when it
    /// started and its file says which, so a gateway whose list cannot be
    /// read, with no copy of it on disk, does not hold it back: it resumes on
    /// that model at once, and its file is untouched.
    #[tokio::test]
    async fn a_run_in_flight_resumes_through_a_dead_gateway() {
        let _redirect = crate::daemon::script_host::REDIRECT_MIRROR.lock().await;
        crate::config::with_isolated_config_path_async(
            "a_run_in_flight_resumes_through_a_dead_gateway",
            |_| async move {
                let dir = tempfile::tempdir().unwrap();
                let manifest = story_manifest(dir.path());
                let runs = tempfile::tempdir().unwrap();
                // The gateway answers the first daemon, then goes down.
                let (url, _) = leviath_testkit::spawn_mock_sequence(vec![
                    (200, "OK", LISTING.to_vec()),
                    (503, "Service Unavailable", b"proxy down".to_vec()),
                ])
                .await;
                let mid = run_in_flight(runs.path(), &manifest, url.clone()).await;

                let mut host = setup_daemon_host(
                    gateway_config(url),
                    runs.path().to_path_buf(),
                    Handle::current(),
                    &StartupBoard::default(),
                )
                .await
                .expect("the daemon starts");

                assert_eq!(status_of(&mut host, &mid).await, Some(AgentStatus::Active));
                assert_eq!(
                    model_of(&mut host, &mid).as_deref(),
                    Some("anthropic/claude-opus-5"),
                    "on the model it chose, not the fallback"
                );
                assert!(
                    !matches!(
                        status_on_disk(runs.path(), &mid),
                        leviath_runtime::state::RunStatus::Error(_)
                    ),
                    "a resumed run is not marked failed"
                );
            },
        )
        .await;
    }

    /// With a cached copy of the gateway's list, a dead gateway at start costs
    /// nothing: the copy answers, a new run and a run in flight both start on
    /// the model their stage named, and the copy is not written back as
    /// though the gateway had answered.
    #[tokio::test]
    async fn a_cached_model_list_answers_while_the_gateway_is_down() {
        let _redirect = crate::daemon::script_host::REDIRECT_MIRROR.lock().await;
        crate::config::with_isolated_config_path_async(
            "a_cached_model_list_answers_while_the_gateway_is_down",
            |_| async move {
                let dir = tempfile::tempdir().unwrap();
                let manifest = story_manifest(dir.path());
                let runs = tempfile::tempdir().unwrap();
                let (url, _) = leviath_testkit::spawn_mock_sequence(vec![
                    (200, "OK", LISTING.to_vec()),
                    (503, "Service Unavailable", b"proxy down".to_vec()),
                ])
                .await;
                let mid = run_in_flight(runs.path(), &manifest, url.clone()).await;

                let cache_path = leviath_core::paths::capability_cache_path().unwrap();
                let mut cache = leviath_providers::CapabilityCache::new(1);
                cache.set(
                    "openrouter",
                    [(
                        "anthropic/claude-opus-5".to_string(),
                        leviath_providers::LearnedModel::default(),
                    )]
                    .into_iter()
                    .collect(),
                );
                std::fs::create_dir_all(cache_path.parent().unwrap()).unwrap();
                cache.save(&cache_path).unwrap();

                let mut host = setup_daemon_host(
                    gateway_config(url),
                    runs.path().to_path_buf(),
                    Handle::current(),
                    &StartupBoard::default(),
                )
                .await
                .expect("the daemon starts");

                assert!(status_of(&mut host, &mid).await.is_some());
                let (op, rx) = spawn_op(&manifest);
                let (ctl_tx, ctl_rx) = tokio::sync::mpsc::unbounded_channel();
                ctl_tx.send(op).unwrap();
                drop(ctl_tx);
                host.serve(ctl_rx).await;
                let new = rx.await.unwrap().expect("the new run starts");
                for run_id in [new.run_id.as_str(), mid.as_str()] {
                    assert_eq!(
                        model_of(&mut host, run_id).as_deref(),
                        Some("anthropic/claude-opus-5"),
                        "{run_id} runs the model its stage named, not the fallback"
                    );
                }

                let cache = leviath_providers::CapabilityCache::load(&cache_path).unwrap();
                let outcome = cache.check("openrouter").map(|c| c.outcome.clone());
                assert_ne!(
                    outcome,
                    Some(leviath_providers::CheckOutcome::Reachable { models: 1 }),
                    "the gateway did not answer, so the cache must not say it did"
                );
                assert!(outcome.is_some(), "the failed start-up read is recorded");
                assert!(
                    cache.get("openrouter").is_some(),
                    "and still holds the copy"
                );
            },
        )
        .await;
    }

    /// The incident, end to end. A daemon started while its gateway's proxy was
    /// down came up with an empty model list, every bare model name in the
    /// blueprint went unrouted, and every stage ran on `fallback_model` with
    /// nothing but a warning in daemon.log. Here the gateway refuses the boot
    /// listing and the next one, and a stage naming a model it does carry must
    /// be refused rather than started on the fallback. Once the gateway
    /// answers, the next spawn reads the list again and goes through, with no
    /// restart.
    #[tokio::test]
    async fn a_daemon_started_against_a_dead_gateway_refuses_instead_of_falling_back() {
        let _redirect = crate::daemon::script_host::REDIRECT_MIRROR.lock().await;
        crate::config::with_isolated_config_path_async(
            "a_daemon_started_against_a_dead_gateway_refuses_instead_of_falling_back",
            |_| async move {
                let listing =
                    br#"{"data":[{"id":"anthropic/claude-opus-5","context_length":200000}]}"#;
                // The boot listing and the first spawn's fail (the proxy is
                // down); the next answers.
                let (url, _) = leviath_testkit::spawn_mock_sequence(vec![
                    (503, "Service Unavailable", b"proxy down".to_vec()),
                    (503, "Service Unavailable", b"proxy down".to_vec()),
                    (200, "OK", listing.to_vec()),
                ])
                .await;
                let runs = tempfile::tempdir().unwrap();
                let mut host = setup_daemon_host(
                    gateway_config(url),
                    runs.path().to_path_buf(),
                    Handle::current(),
                    &StartupBoard::default(),
                )
                .await
                .expect("a dead gateway does not stop the daemon starting");

                let dir = tempfile::tempdir().unwrap();
                let manifest = story_manifest(dir.path());

                let err = spawn_through(&mut host, task_request(&manifest, "t"))
                    .await
                    .expect_err("must not start on the fallback")
                    .to_string();
                assert!(err.contains("stage 'story' names claude-opus-5"), "{err}");
                assert!(
                    err.contains("model list of openrouter has not been read"),
                    "{err}"
                );

                // The next spawn asks the gateway again: it answers now, so the
                // stage resolves to the model it named rather than to the
                // fallback.
                let back = spawn_through(&mut host, task_request(&manifest, "t"))
                    .await
                    .expect("the gateway answers now");
                assert_eq!(
                    model_of(&mut host, back.as_str()).as_deref(),
                    Some("anthropic/claude-opus-5")
                );
            },
        )
        .await;
    }

    /// A config that names one endpoint provider, written to `path`.
    fn config_naming(path: &std::path::Path, providers: &[(&str, &str)], default: &str) -> Config {
        let mut config = Config {
            default_provider: default.to_string(),
            override_model: Some("m".to_string()),
            ..Config::default()
        };
        for (name, url) in providers {
            config.model_providers.insert(
                (*name).to_string(),
                crate::config::ModelProviderConfig {
                    kind: Some(crate::config::ModelProviderKind::OpenaiCompatible),
                    base_url: Some((*url).to_string()),
                    api_key: Some(format!("key-{name}")),
                    models: Some(vec!["m".to_string()]),
                    ..Default::default()
                },
            );
        }
        std::fs::write(path, toml::to_string(&config).unwrap()).unwrap();
        // Strictly newer, so a rewrite inside one clock tick is still seen.
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5))
            .unwrap();
        config
    }

    /// A one-stage blueprint pinned to `provider`, with the user default
    /// refused: the only way it can run is if that provider is registered.
    fn blueprint_pinned_to(dir: &std::path::Path, provider: &str) -> std::path::PathBuf {
        let manifest = dir.join("agent.toml");
        std::fs::write(
            &manifest,
            format!(
                r#"[blueprint]
name = "pinned"
version = "0.1.0"

[graph]
entry = "work"

[[graph.stages]]
name = "work"
system_prompt = "reply"

[graph.stages.model]
models = [{{ provider = "{provider}", model = "m" }}]
allow_user_default = false

[graph.layout]
regions = [{{ name = "task", kind = "pinned", budget = 200 }}]
total_budget_tokens = 200

[[graph.inputs]]
name = "task"
type = {{ kind = "text", multiline = true }}
binds = [{{ region = "task" }}]
"#
            ),
        )
        .unwrap();
        manifest
    }

    /// A spawn of `manifest` for the control channel, and where its answer
    /// arrives.
    fn spawn_op(
        manifest: &std::path::Path,
    ) -> (
        ControlOp,
        oneshot::Receiver<
            Result<
                leviath_runtime::spec::summary::Spawned,
                leviath_runtime::spec::issues::SpawnIssues,
            >,
        >,
    ) {
        let (reply, reply_rx) = oneshot::channel();
        let op = ControlOp::Spawn {
            request: Box::new(task_request(manifest, "t")),
            reply,
        };
        (op, reply_rx)
    }

    /// What the provider reload exists for: the daemon boots with one provider
    /// configured, the user configures another (`lev setup`, `PUT /api/config`,
    /// an editor - they all write this file), and the very next run has to be
    /// able to use it rather than be refused with "no usable provider".
    #[tokio::test]
    async fn a_provider_configured_after_boot_is_usable_on_the_next_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let config = config_naming(&config_path, &[("alpha", "http://127.0.0.1:9/v1")], "alpha");
        let manifest = blueprint_pinned_to(dir.path(), "beta");
        let runs = tempfile::tempdir().unwrap();
        let providers = leviath_runtime::provider_creds::build_provider_registry_probing(
            &crate::commands::run::session::provider_creds_from_config(&config),
            &leviath_providers::provider::build_http_client,
            &|_| false,
        )
        .unwrap();
        let mut host = build_host(HostParts {
            config: config.clone(),
            providers: providers.clone(),
            runs_dir: runs.path().to_path_buf(),
            shared_mcp: Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
            mcp_tool_defs: Vec::new(),
            mcp_tool_owners: Default::default(),
            mcp_pool: Arc::new(empty_pool()),
            runtime: Handle::current(),
            now_secs: || 0,
            reloader: Some(Arc::new(crate::daemon::config_reload::ConfigReloader::new(
                config_path.clone(),
                config.clone(),
            ))),
            provider_reload: Some(crate::daemon::provider_reload::for_daemon(
                &config, providers,
            )),
        });

        // Both spawns go through one `serve()`: it flushes the persistence
        // lane on the way out, so a host only ever serves once. The driver
        // task rewrites the config between the two, which is the whole point.
        let (ctl_tx, ctl_rx) = tokio::sync::mpsc::unbounded_channel();
        let (first, first_reply) = spawn_op(&manifest);
        ctl_tx.send(first).unwrap();
        let driver_tx = ctl_tx.clone();
        let driver_config = config_path.clone();
        let driver_manifest = manifest.clone();
        let driver = tokio::spawn(async move {
            let before = first_reply.await.unwrap();
            // What `lev setup` (or `PUT /api/config`) does: rewrite the file.
            config_naming(&driver_config, &[("beta", "http://127.0.0.1:9/v1")], "beta");
            let (second, second_reply) = spawn_op(&driver_manifest);
            driver_tx.send(second).unwrap();
            let after = second_reply.await.unwrap();
            drop(driver_tx);
            (before, after)
        });
        drop(ctl_tx);
        host.serve(ctl_rx).await;
        let (before, after) = driver.await.unwrap();

        assert!(
            before.is_err(),
            "beta is not configured yet, so the spawn has nowhere to go: {before:?}"
        );
        assert!(
            after.is_ok(),
            "the provider the user just configured has to work without a daemon restart: {after:?}"
        );
        assert!(
            host.world_mut().providers().has("beta"),
            "the rebuilt registry is the one the world resolves against"
        );
        assert!(
            !host.world_mut().providers().has("alpha"),
            "and the provider they removed is no longer a route"
        );
    }

    #[tokio::test]
    async fn fake_provider_methods_are_exercised() {
        use leviath_providers::Provider;
        let p = fake_provider();
        assert_eq!(p.name(), "fake");
        assert_eq!(p.count_tokens("t", "m").await, 1);
        assert_eq!(p.max_context_tokens("m"), 1000);
        let _ = p.capabilities("m");
        assert!(p.infer(&fixtures::inference_request()).await.is_err());
    }

    #[tokio::test]
    async fn build_host_spawns_agents_through_the_installed_starter() {
        crate::config::with_isolated_config_path_async(
            "build_host_spawns_agents_through_the_installed_starter",
            |_| async move {
                let dir = tempfile::tempdir().unwrap();
                let manifest = dir.path().join("agent.toml");
                std::fs::write(&manifest, crate::test_support::inline_coder_manifest()).unwrap();

                let runs = tempfile::tempdir().unwrap();
                let mut host = fake_host(runs.path());

                let run_id = spawn_through(&mut host, task_request(&manifest, "do it"))
                    .await
                    .expect("the run starts");

                // The run is registered and Active.
                assert_eq!(
                    status_of(&mut host, run_id.as_str()).await,
                    Some(AgentStatus::Active)
                );
            },
        )
        .await;
    }

    /// A host over the fake `anthropic` provider, keeping its runs in `runs`.
    fn fake_host(runs: &std::path::Path) -> WorldHost {
        host_over(runs, fake_registry())
    }

    /// A host over `providers`, keeping its runs in `runs`.
    fn host_over(runs: &std::path::Path, providers: ProviderRegistry) -> WorldHost {
        build_host(HostParts {
            config: Config::default(),
            providers,
            runs_dir: runs.to_path_buf(),
            shared_mcp: Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
            mcp_tool_defs: vec![],
            mcp_tool_owners: Default::default(),
            mcp_pool: crate::daemon::mcp_pool::McpPool::for_daemon(
                Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
                &[],
            ),
            runtime: Handle::current(),
            now_secs: || 100,
            reloader: None,
            provider_reload: None,
        })
    }

    /// The fake `anthropic` provider, as a registry.
    fn fake_registry() -> ProviderRegistry {
        let mut registry = ProviderRegistry::new();
        registry.register("anthropic".to_string(), Arc::new(fake_provider()));
        registry
    }

    #[tokio::test]
    async fn build_host_reloads_and_registers_persisted_runs() {
        crate::config::with_isolated_config_path_async(
            "build_host_reloads_and_registers_persisted_runs",
            |_| async move {
                // A run recorded under the runs dir is resumed and registered by
                // `build_host`.
                let agent = tempfile::tempdir().unwrap();
                let manifest = agent.path().join("agent.toml");
                std::fs::write(&manifest, crate::test_support::inline_coder_manifest()).unwrap();
                let runs = tempfile::tempdir().unwrap();
                let run_id =
                    run_on_disk(Config::default(), fake_registry(), runs.path(), &manifest);

                let mut host = fake_host(runs.path());

                // The resumed run is registered → Status resolves it.
                assert_eq!(
                    status_of(&mut host, &run_id).await,
                    Some(AgentStatus::Active)
                );
            },
        )
        .await;
    }

    /// A run this machine can no longer take back is held at startup: listed,
    /// paused, rather than ended.
    #[tokio::test]
    async fn build_host_holds_a_run_this_machine_cannot_take_back() {
        crate::config::with_isolated_config_path_async(
            "build_host_holds_a_run_this_machine_cannot_take_back",
            |_| async move {
                let agent = tempfile::tempdir().unwrap();
                let manifest = agent.path().join("agent.toml");
                std::fs::write(&manifest, crate::test_support::inline_coder_manifest()).unwrap();
                let runs = tempfile::tempdir().unwrap();
                let run_id =
                    run_on_disk(Config::default(), fake_registry(), runs.path(), &manifest);

                let mut host = host_over(runs.path(), ProviderRegistry::new());

                assert_eq!(
                    status_of(&mut host, &run_id).await,
                    Some(AgentStatus::Paused)
                );
            },
        )
        .await;
    }

    #[tokio::test]
    async fn build_host_installs_a_reloader_that_pages_in_unloaded_runs() {
        // A run that lands on disk *after* startup (so it is not resumed then)
        // must still be reachable: a control op targeting it fires the installed
        // reloader, which pages it into the world on demand.
        let agent = tempfile::tempdir().unwrap();
        let manifest = agent.path().join("agent.toml");
        std::fs::write(&manifest, crate::test_support::inline_coder_manifest()).unwrap();

        let runs = tempfile::tempdir().unwrap();
        let mut host = fake_host(runs.path());

        // Recorded only now - build_host's startup resume already ran, so it
        // is on disk but absent from the world.
        let run_id = run_on_disk(Config::default(), fake_registry(), runs.path(), &manifest);

        // It is not loaded yet: a read-only Status does not page it in.
        assert_eq!(status_of(&mut host, &run_id).await, None);

        // A Cancel routes through the reloader, paging it in off the loop and
        // acting on it once it lands.
        let (reply, rx) = oneshot::channel();
        host.handle(ControlOp::Cancel {
            run_id: run_id.clone(),
            reply,
        });
        host.land_pages().await;
        assert!(rx.await.unwrap());
    }

    /// Presence only, and only for the names the binary asks about - so this
    /// can never become a way to read a daemon's environment.
    #[test]
    fn visible_tool_env_reports_the_probed_names_it_can_read() {
        temp_env::with_var("BRAVE_API_KEY", Some("sk-brave"), || {
            assert_eq!(visible_tool_env(), vec!["BRAVE_API_KEY".to_string()]);
        });
    }

    #[test]
    fn visible_tool_env_is_empty_when_nothing_probed_is_set() {
        temp_env::with_var("BRAVE_API_KEY", None::<&str>, || {
            assert!(visible_tool_env().is_empty());
        });
    }

    /// An exported-but-empty variable reads as configured and is not: the
    /// script that would use it gets an empty key and falls back anyway.
    #[test]
    fn visible_tool_env_treats_an_empty_value_as_absent() {
        temp_env::with_var("BRAVE_API_KEY", Some(""), || {
            assert!(visible_tool_env().is_empty());
        });
    }

    #[test]
    fn build_marker_round_trips_and_is_current() {
        let dir = tempfile::tempdir().unwrap();
        temp_env::with_var("LEVIATH_HOME", Some(dir.path()), || {
            use super::super::build::{Build, Standing};
            // No marker yet: an older daemon, which a spawn replaces.
            assert!(read_build_marker().is_none());
            let current = Build::current();
            assert_eq!(Build::parse("").standing(&current), Standing::Older);

            write_build_marker();
            let path = build_marker_path().unwrap();
            assert!(path.exists());
            let marker = read_build_marker().unwrap();
            assert!(marker.starts_with(CURRENT_BUILD), "{marker}");
            // A daemon that wrote this build is this build.
            assert_eq!(Build::parse(&marker), current);
            assert_eq!(Build::parse(&marker).standing(&current), Standing::Same);
        });
    }

    /// A starting daemon records its build before a client can reach it,
    /// unless another daemon already answers, whose marker stays.
    #[tokio::test]
    async fn the_build_is_recorded_before_the_bind_unless_a_daemon_answers() {
        let dir = tempfile::tempdir().unwrap();
        temp_env::with_var("LEVIATH_HOME", Some(dir.path()), || {
            let id = control_address().unwrap();
            write_build_marker_unless_running(&id);
            let path = build_marker_path().unwrap();
            assert!(path.exists(), "nothing answers, so this build is recorded");
            std::fs::remove_file(&path).unwrap();
            let _listener = leviath_runtime::control_socket::bind_control_listener(&id).unwrap();
            write_build_marker_unless_running(&id);
            assert!(!path.exists(), "the daemon answering keeps its own marker");
        });
    }

    #[tokio::test]
    async fn the_daemon_refuses_to_start_without_a_usable_https_client() {
        // `setup_daemon_host_with` mirrors `[security] allow_local_network`
        // into a process-wide atomic, which makes this test a writer of the
        // switch the script-host redirect tests read. Without the lock, standing
        // up a host here flipped that switch mid-request over there and the
        // refusal it saw was this test's config, not its own.
        let _redirect = crate::daemon::script_host::REDIRECT_MIRROR.lock().await;
        // Better than accepting runs it could never infer for: the error names
        // the cause, where the previous behaviour was a panic at start-up.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = Config::default();
        config.providers.anthropic_api_key = Some("k".to_string());
        let err = setup_daemon_host_with(
            config,
            (dir.path().to_path_buf(), &StartupBoard::default()),
            Handle::current(),
            &|_t| Err(leviath_providers::provider::malformed_url_error()),
        )
        .await
        .err()
        .expect("a failing client factory should stop the daemon starting");
        assert!(err.to_string().contains("root certificate store"));
    }

    /// Write `config` to `path` with a strictly newer mtime, so a rewrite
    /// inside one clock tick is still seen as a change (the reloader polls the
    /// mtime, exactly as `config_reload`'s own tests do).
    fn save_config(path: &std::path::Path, config: &Config) {
        std::fs::write(path, toml::to_string(config).unwrap()).unwrap();
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5))
            .unwrap();
    }

    /// A host whose spawn-time config is read from `path`, the way the daemon's
    /// is, with a stand-in for the manifest's provider so a spawn resolves.
    fn host_watching(path: &std::path::Path, boot: Config, runs: &std::path::Path) -> WorldHost {
        let mut providers = ProviderRegistry::new();
        providers.register("anthropic".to_string(), Arc::new(fake_provider()));
        build_host(HostParts {
            config: boot.clone(),
            providers,
            runs_dir: runs.to_path_buf(),
            shared_mcp: Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
            mcp_tool_defs: Vec::new(),
            mcp_tool_owners: Default::default(),
            mcp_pool: crate::daemon::mcp_pool::McpPool::for_daemon(
                Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
                &[],
            ),
            runtime: Handle::current(),
            now_secs: || 0,
            reloader: Some(Arc::new(crate::daemon::config_reload::ConfigReloader::new(
                path.to_path_buf(),
                boot,
            ))),
            provider_reload: None,
        })
    }

    /// Spawn `manifest` through the host's real starter and assert it took.
    async fn spawn_ok(host: &mut WorldHost, manifest: &std::path::Path) {
        spawn_through(host, task_request(manifest, "name this run"))
            .await
            .expect("the run starts");
    }

    /// A `[limits]` edit is picked up by the next spawn, with no daemon
    /// restart: the pools, the tool lane, the watchdogs, the breaker, the retry
    /// schedule and the fan-out ceiling all move to what the file says now.
    #[tokio::test]
    async fn a_limits_edit_reaches_the_world_on_the_next_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut boot = config_with_anthropic_key();
        boot.limits.max_concurrent_tools = 2;
        boot.limits.max_concurrent_inferences = Some(1);
        boot.limits.stall_timeout_secs = 60;
        boot.limits.max_agents_per_run = 0;
        boot.limits.provider_failures_before_open = 3;
        boot.limits.inference_retry_attempts = 4;
        boot.limits.finished_retention_secs = 300;
        boot.limits.notify_spend_usd = Vec::new();
        save_config(&path, &boot);

        let runs = tempfile::tempdir().unwrap();
        let mut host = host_watching(&path, boot.clone(), runs.path());
        assert_eq!(host.world_mut().tool_concurrency(), 2, "the boot width");

        // The operator edits the file while the daemon runs.
        let mut after = boot.clone();
        after.limits.max_concurrent_tools = 6;
        after.limits.max_concurrent_inferences = Some(4);
        after.limits.stall_timeout_secs = 5;
        after.limits.wedge_timeout_secs = 300;
        after.limits.max_agents_per_run = 20;
        after.limits.provider_failures_before_open = 9;
        after.limits.provider_circuit_cooldown_secs = 42;
        after.limits.inference_retry_attempts = 7;
        after.limits.inference_retry_base_ms = 250;
        after.limits.finished_retention_secs = 30;
        after.limits.dead_cycles_before_relief = 3;
        after.limits.notify_spend_usd = vec![5.0];
        save_config(&path, &after);

        let agent = tempfile::tempdir().unwrap();
        let manifest = agent.path().join("agent.toml");
        std::fs::write(&manifest, crate::test_support::inline_coder_manifest()).unwrap();
        spawn_ok(&mut host, &manifest).await;

        let settings = host.settings();
        let world = host.world_mut();
        assert_eq!(world.tool_concurrency(), 6, "the tool lane widened");
        assert_eq!(
            world.inference_pool_config().limit_for("m"),
            Some(4),
            "the inference pool followed"
        );
        let ecs = world.world();
        assert_eq!(
            ecs.resource::<leviath_runtime::pipeline::StallTimeout>().0,
            5
        );
        assert_eq!(
            ecs.resource::<leviath_runtime::pipeline::WedgeTimeout>().0,
            300
        );
        let circuit = ecs.resource::<leviath_runtime::pipeline::CircuitPolicy>();
        assert_eq!(
            (circuit.failures_before_open, circuit.cooldown_secs),
            (9, 42)
        );
        let retry = ecs.resource::<leviath_runtime::pipeline::InferenceRetryTuning>();
        assert_eq!((retry.max_attempts, retry.base_delay_ms), (7, 250));
        assert_eq!(
            ecs.resource::<leviath_runtime::fanout::FanOutBudget>().0,
            20
        );
        assert_eq!(settings.dead_cycles_before_relief(), 3);
        assert_eq!(settings.finished_retention_secs(), 30);
        assert_eq!(*settings.spend_notify_usd(), vec![5.0]);
    }

    /// Turning `[title]` on has to reach both halves at once. Spawn reads a
    /// fresh config and marks the run `PendingTitle`; if the system that makes
    /// titles were still on a boot-time `TitleSettings` it would see titling
    /// switched off and drop the marker without a word.
    #[tokio::test]
    async fn enabling_titles_reaches_the_system_that_makes_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut boot = config_with_anthropic_key();
        boot.title.enabled = false;
        save_config(&path, &boot);

        let runs = tempfile::tempdir().unwrap();
        let mut host = host_watching(&path, boot.clone(), runs.path());
        assert!(
            !host
                .world_mut()
                .world()
                .resource::<leviath_runtime::title::TitleSettings>()
                .0
                .enabled,
            "titling is off to begin with"
        );

        let mut after = boot.clone();
        after.title.enabled = true;
        save_config(&path, &after);

        let agent = tempfile::tempdir().unwrap();
        let manifest = agent.path().join("agent.toml");
        std::fs::write(&manifest, crate::test_support::inline_coder_manifest()).unwrap();
        spawn_ok(&mut host, &manifest).await;

        let world = host.world_mut().world_mut();
        let mut pending = world.query::<&leviath_runtime::title::PendingTitle>();
        assert_eq!(
            pending.iter(world).count(),
            1,
            "spawn read the new config and marked the run for a title"
        );
        assert!(
            world
                .resource::<leviath_runtime::title::TitleSettings>()
                .0
                .enabled,
            "and the dispatcher agrees, so the marker is acted on rather than dropped"
        );
    }
}
