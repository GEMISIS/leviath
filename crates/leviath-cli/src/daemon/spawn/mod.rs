//! The daemon spawner: turns a [`SpawnArgs`] request into a live agent in the
//! shared world - the CLI-side policy the runtime host calls for a `Spawn`
//! control op.
//!
//! It loads the blueprint, resolves each stage's provider/model (against the
//! world's registered providers) and effective tool set, spawns the agent via
//! [`leviath_runtime::pipeline::spawn_agent`], attaches its run metadata /
//! token totals / compaction settings, and registers its per-agent tool state
//! with the [`CliToolService`]. The heavy MCP connections are shared, and the
//! async spawn preprocessor has already warmed the ones this run needs, so this
//! whole path is synchronous - which lets it run straight from the host's
//! control loop.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex};

use bevy_ecs::entity::Entity;
use bevy_ecs::world::World;
use leviath_core::blueprint::Blueprint;
use leviath_providers::Tool;
use leviath_runtime::host::{SpawnArgs, SubAgentOp};
use leviath_runtime::interaction_hub::InteractionHub;
use leviath_runtime::persistence::{RunMetadata, TokenTotals};
use leviath_runtime::pipeline::{
    CompactionSettings, ModelDefaults, PersistWatermark, Providers, resolve_stages,
    spawn_agent_seeded,
};
use tokio::sync::Mutex;
use tokio::sync::mpsc::UnboundedSender;

use crate::config::Config;
use crate::daemon::seed_command::SeedCommandPolicy;
use crate::daemon::subagent::SubAgentHandle;
use crate::daemon::tool_service::{AgentToolState, CliToolService};

/// Default max sub-agent tree depth when a blueprint doesn't set one.
const DEFAULT_SUBAGENT_DEPTH: usize = 3;

// Sections of the former single-file spawn path, one per question it answers.
// The first two are re-exported because the daemon reaches them directly
// (`model_defaults`, the script resolvers); the last two are internal to the
// spawn path and only `build_agent_inner` calls them.
mod policy;
pub(crate) use policy::*;
mod scripts;
pub(crate) use scripts::*;
mod seeds;
pub(crate) use seeds::*;
mod tool_state;
use tool_state::*;

/// Everything a spawn needs that is not the request itself.
///
/// Grouped because these seven travel together through every spawn path -
/// [`build_agent`], [`build_agent_for_reload`], and the fan-out world-system -
/// and threading them positionally meant three signatures that had to agree
/// plus a `too_many_arguments` suppression on each. It also meant a nine-argument
/// call in which `config`, `mcp_tool_defs` and `hub` are adjacent references:
/// transposing two of them type-checks in some orders, and the compiler is the
/// only thing that was ever going to notice.
#[derive(Clone)]
pub(crate) struct SpawnDeps<'a> {
    /// The daemon's tool service, which per-agent state is registered against.
    pub tool_service: &'a CliToolService,
    /// The resolved daemon configuration for this run.
    pub config: &'a Config,
    /// MCP connections shared by every agent, built once at startup.
    pub shared_mcp: Arc<Mutex<leviath_mcp::ToolExecutor>>,
    /// Tool definitions those MCP servers advertise.
    pub mcp_tool_defs: &'a [Tool],
    /// Which server advertises each of them, for a stage that grants a whole
    /// connector. Travels beside the defs because it is only knowable where
    /// they are registered - a `Tool` carries no owner.
    pub mcp_tool_owners: &'a leviath_runtime::pipeline::ToolOwners,
    /// Where this agent's interaction prompts are parked.
    pub hub: &'a InteractionHub,
    /// Spawn time, injected so a test does not depend on the wall clock.
    pub now_secs: i64,
    /// Channel the agent's sub-agent tools send operations on.
    pub subagent_tx: UnboundedSender<SubAgentOp>,
}

/// Load the blueprint at `args.blueprint_path`, spawn the agent into `world`,
/// register its tool state, and return the new entity. Operates on the raw ECS
/// [`World`] so it is callable both from the host's spawner (via
/// `PipelineWorld::world_mut`) and from a fan-out world-system.
///
/// Enforces the required-at-spawn region gate - a fresh spawn whose required
/// caller-input regions weren't provided fails here. Use
/// [`build_agent_for_reload`] on the recovery path, where the window is restored
/// from a snapshot afterward and the gate must not re-fire.
pub(crate) fn build_agent(
    world: &mut World,
    deps: SpawnDeps<'_>,
    args: &SpawnArgs,
) -> Result<Entity, String> {
    build_agent_inner(world, deps, args, true)
}

/// Like [`build_agent`], but skips the required-at-spawn region gate - used by
/// restart recovery, which reloads a run that already passed the gate when first
/// spawned and whose context window is restored from a snapshot after this call.
pub(crate) fn build_agent_for_reload(
    world: &mut World,
    deps: SpawnDeps<'_>,
    args: &SpawnArgs,
) -> Result<Entity, String> {
    build_agent_inner(world, deps, args, false)
}

/// Reject a spawn request that cannot work, before anything is built over it.
///
/// Both checks guard a failure that would otherwise surface far from its cause,
/// which is why they run first and together rather than where each value is
/// eventually used.
fn check_spawn_request(args: &SpawnArgs) -> Result<(), String> {
    // The run id becomes a directory name, and everything this run writes lands
    // under it: `meta.json`, the context snapshot, the answer sidecar. The
    // persistence lane joins it to the runs directory without checking, so a
    // component holding `..` would place a run's files outside it.
    //
    // Not reachable from the API, which mints its own id (`new_run_id` replaces
    // every character that is not alphanumeric or a hyphen), and a control
    // socket client is already the same user. It is one comparison to make the
    // property hold where the request is accepted rather than resting on both of
    // those staying true.
    if !leviath_core::is_safe_path_component(&args.run_id) {
        return Err(format!(
            "run id '{}' is not a usable directory name",
            args.run_id
        ));
    }

    // The working directory must exist before anything is built over it.
    // `ToolContext::new` silently keeps a path it can't canonicalize, so without
    // this a bogus workdir spawns a healthy-looking agent whose every tool call
    // fails with a message naming the shell rather than the directory.
    if !std::fs::metadata(&args.workdir).is_ok_and(|m| m.is_dir()) {
        return Err(format!(
            "workspace '{}' does not exist or is not a directory",
            args.workdir
        ));
    }

    Ok(())
}

/// Read, parse and validate the blueprint, then fold the request's and the
/// user's ceilings into it.
///
/// Returns the manifest text alongside the blueprint because later phases need
/// the raw source too - `[tool_script_permissions]` is read from it directly.
fn load_blueprint(
    args: &SpawnArgs,
    config: &crate::config::Config,
) -> Result<(String, leviath_core::Blueprint), String> {
    let content = std::fs::read_to_string(&args.blueprint_path)
        .map_err(|e| format!("read manifest '{}': {e}", args.blueprint_path))?;
    // A blueprint that will not load is usually the user's own mistake, but for
    // an installed bundled agent it is usually just an old copy: a graph rule
    // added since they installed turns "valid" into "invalid blueprint", which
    // reads as a broken agent rather than a stale file. Say which it is.
    let path = Path::new(&args.blueprint_path);
    let stale = || {
        crate::bundled::stale_install_suffix(
            path,
            crate::bundled::real_agents_dir_opt().as_deref(),
            ". ",
        )
    };
    let mut blueprint = leviath_core::manifest::parse_manifest(&content)
        .map_err(|e| format!("parse manifest: {e}{}", stale()))?;
    blueprint
        .validate()
        .map_err(|e| format!("invalid blueprint: {e}{}", stale()))?;
    // What `lev validate` would have said, in the daemon log. Nothing here
    // refuses a spawn: these are authoring mistakes whose cost is a run that
    // behaves oddly hours later, and the whole point is that they are invisible
    // until then. Logging them means the answer is already in `daemon.log`
    // whenever someone goes looking for why a run stalled.
    log_blueprint_lint(&content, &blueprint, &args.blueprint_path);
    // A request-level `--max-depth` overrides the blueprint's sub-agent depth cap.
    if let Some(md) = args.max_depth {
        blueprint.max_child_depth = Some(md);
    }
    // Apply the deps.config's `default_max_iterations` to any stage that doesn't set
    // its own, so an agent can't loop forever with no completion signal
    // (`enforce_max_iterations` treats `None`/0 as unbounded). A stage's explicit
    // `max_iterations` always wins.
    if let Some(default_max) = config.limits.default_max_iterations {
        for stage in &mut blueprint.stages {
            // `0` means *unbounded* to the pipeline, and `get_or_insert` only
            // fills `None` - so a manifest writing `max_iterations = 0` looked
            // like "unset" while actually opting out of the user's ceiling
            // entirely, and looped without limit against their API keys. A
            // manifest may still declare its own finite number; it may not
            // declare "no limit" over a user who asked for one.
            match stage.max_iterations {
                None | Some(0) => stage.max_iterations = Some(default_max),
                Some(_) => {}
            }
        }
    }

    Ok((content, blueprint))
}

/// Everything phase 7 attaches that is not already on the entity.
///
/// A struct because these are one thing - the durable record of a run - rather
/// than ten independent arguments, and because ten positional arguments of
/// which four are collections is a transposition waiting to happen.
struct RunRecordParts {
    agent_name: String,
    model_label: Option<String>,
    num_stages: usize,
    read_path_counts: Option<leviath_core::run_meta::ReadPathGrantCounts>,
    output_validators:
        HashMap<String, std::sync::Arc<leviath_scripting::output_validator::OutputValidator>>,
    outcome_flags: leviath_runtime::persistence::RunOutcomeFlags,
    /// The provider/model chain the run's title call may walk, best first.
    ///
    /// Empty means "do not title this run at all", so this carries both the
    /// decision and its subject. Decided by the caller rather than here,
    /// because whether to title turns on whether this is a fresh spawn or a run
    /// being paged back in - which is the caller's distinction, not a property
    /// of the record.
    title_chain: Vec<(String, String)>,
    compaction: Option<leviath_core::CompactionConfig>,
    tool_sensitivities: Option<HashMap<String, leviath_core::TaintLevel>>,
    security: leviath_core::taint::SecurityConfig,
    mcp_overrides: std::collections::HashMap<String, leviath_core::policy::McpToolOverride>,
}

/// Record the run on its entity: metadata, counters, and the markers that
/// decide how the pipeline treats it.
fn attach_run_record(
    world: &mut World,
    entity: Entity,
    args: &SpawnArgs,
    deps: &SpawnDeps<'_>,
    parts: RunRecordParts,
) {
    let metadata = RunMetadata {
        run_id: args.run_id.clone(),
        agent_name: parts.agent_name,
        agent_path: args.blueprint_path.clone(),
        task: args.task.clone(),
        model: parts.model_label,
        workdir: args.workdir.clone(),
        num_stages: parts.num_stages,
        started_at: deps.now_secs,
        parent_run_id: args.parent_run_id.clone(),
        metadata: args.metadata.clone(),
        callback_url: args.callback_url.clone(),
        callback_secret: args.callback_secret.clone(),
        title: None,
        title_error: None,
        unattended: args.yolo,
        read_paths: parts.read_path_counts,
        output_request: args.output.clone(),
        model_override: args.model.clone(),
    };
    {
        let mut entity_mut = world.entity_mut(entity);
        if !parts.output_validators.is_empty() {
            entity_mut.insert(leviath_runtime::components::OutputValidators::new(
                parts.output_validators,
            ));
        }
        entity_mut.insert((
            metadata,
            TokenTotals::default(),
            // Stopped and empty; a reloaded run gets its banked working time put
            // back by `recovery::reload_persisted_agents`, and the persistence
            // system starts the clock on its first tick.
            leviath_runtime::persistence::RunClock::default(),
            PersistWatermark::default(),
            // Fresh counters; a reloaded run gets its accumulated flags put back
            // by `recovery::reload_persisted_agents`.
            parts.outcome_flags,
        ));
        // Mark eligible runs for title generation (the `title` module fills
        // `RunMetadata.title`, which the dashboard displays and searches). Root
        // runs only: sub-agents inherit their parent's context in the run list,
        // and titling every fan-out worker would multiply cheap-but-nonzero LLM
        // calls for no UX gain.
        //
        // The candidates ride along with the marker rather than being resolved
        // later from the model label, because the label names one provider and
        // the run has several: a title call that can only try the head of the
        // chain loses the run's name to whatever the head happens to be doing.
        (!parts.title_chain.is_empty())
            .then_some(parts.title_chain)
            .into_iter()
            .for_each(|chain| {
                entity_mut.insert((
                    leviath_runtime::title::PendingTitle,
                    leviath_runtime::title::TitleCandidates(chain),
                ));
            });
        // `--yolo` means run unattended, so a blueprint's stage-boundary
        // checkpoints are approved rather than parked on a deps.hub nobody is
        // watching. (`.then_some(..).into_iter()` keeps the non-yolo path
        // branch-free, matching the taint-gate marker below.)
        args.yolo
            .then_some(leviath_runtime::components::InteractionAutoApprove)
            .into_iter()
            .for_each(|marker| {
                entity_mut.insert(marker);
            });
        // `Option`'s iterator inserts compaction settings when present without a
        // dangling `if let` block-end region.
        parts.compaction.into_iter().for_each(|cc| {
            entity_mut.insert(CompactionSettings(cc));
        });
        // Attach the taint gate + per-tool sensitivities and turn on the window's
        // taint tracking when the blueprint opts in (`Option`'s iterator keeps the
        // enforcement path region-free when taint is off).
        parts
            .tool_sensitivities
            .into_iter()
            .for_each(|sensitivities| {
                let mut gate = leviath_runtime::TaintGate::new(parts.security.clone());
                gate.apply_mcp_overrides(&parts.mcp_overrides);
                entity_mut.insert((
                    gate,
                    leviath_runtime::pipeline::ToolSensitivities(sensitivities),
                ));
                // `--yolo` means run unattended: waive taint-gate prompts (the
                // tool-policy wildcard below doesn't cover them), so a headless run
                // never blocks on a gate no one can answer.
                if args.yolo {
                    entity_mut.insert(leviath_runtime::components::GateAutoApprove);
                }
                // `Option`'s iterator enables tracking without a dead "no window" arm
                // (a freshly spawned agent always carries a ContextWindow).
                entity_mut
                    .get_mut::<leviath_runtime::components::ContextWindow>()
                    .into_iter()
                    .for_each(|mut window| window.enable_taint_tracking());
            });
    }
}

/// What taint tracking this agent runs under.
///
/// The three travel together because they are one decision: whether to gate at
/// all, which reclassifications apply, and the per-tool sensitivities that fall
/// out of both. Returning them separately invited a caller to build a gate from
/// one and forget the others.
struct TaintSetup {
    security: leviath_core::taint::SecurityConfig,
    mcp_overrides: std::collections::HashMap<String, leviath_core::policy::McpToolOverride>,
    tool_sensitivities: Option<HashMap<String, leviath_core::TaintLevel>>,
}

/// Resolve the agent's taint configuration against the global setting, the
/// blueprint's own `[security]` block, and the world's policy overrides.
fn resolve_taint_setup(
    world: &World,
    blueprint: &leviath_core::Blueprint,
    config: &crate::config::Config,
    all_tool_defs: &[leviath_providers::Tool],
    read_paths_granted: bool,
) -> TaintSetup {
    // `resolve_security` (rather than `unwrap_or_default`, which forced taint on
    // for every agent because `SecurityConfig::default()` is taint-on) means a
    // blueprint with no `[security]` block correctly inherits the global setting -
    // off by default. When on, the agent's outbound tool calls are gated
    // against its context taint + the policy allowlist; when off no gate is
    // attached (zero enforcement overhead).
    let security = leviath_core::taint::resolve_security(
        config.taint_tracking,
        blueprint.security.as_ref(),
        None,
    );
    // The `[mcp_overrides]` from policy.toml (loaded into the world at daemon
    // setup), applied to every gate this agent gets so a user's reclassified
    // MCP tool is enforced, not just printed by `lev policy list`.
    let mcp_overrides = world
        .get_resource::<leviath_runtime::pipeline::PolicyGate>()
        .map(|p| p.0.mcp_overrides.clone())
        .unwrap_or_default();
    let tool_sensitivities: Option<HashMap<String, leviath_core::TaintLevel>> =
        security.taint_tracking.then(|| {
            let mut gate = leviath_runtime::TaintGate::new(security.clone());
            gate.apply_mcp_overrides(&mcp_overrides);
            let mut map: HashMap<String, leviath_core::TaintLevel> = all_tool_defs
                .iter()
                .map(|t| {
                    (
                        t.name.clone(),
                        gate.tool_classification(&t.name).sensitivity,
                    )
                })
                .collect();
            bump_read_sensitivities(&mut map, read_paths_granted);
            map
        });

    TaintSetup {
        security,
        mcp_overrides,
        tool_sensitivities,
    }
}

/// Log whatever `lev validate` would have reported about this manifest.
///
/// The lint env is built from the manifest's own directory so the agent's
/// `tools/*.rhai` resolve, and deliberately without the provider check: the
/// stage resolution a few steps later already fails a spawn outright when
/// nothing in a stage's models list is registered, and re-deriving that here
/// would cost a provider-registry build per agent to say the same thing more
/// quietly.
fn log_blueprint_lint(content: &str, blueprint: &Blueprint, manifest_path: &str) {
    let agent_dir = std::path::Path::new(manifest_path)
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    let env = crate::lint::LintEnv::offline(&agent_dir);
    for finding in crate::lint::lint_manifest(content, blueprint, &env) {
        // Notes describe things the blueprint means to do; only the checks that
        // found something questionable are worth a daemon log line.
        if finding.severity == crate::lint::LintSeverity::Note {
            continue;
        }
        // Built before the macro rather than inside it: `tracing::warn!` only
        // evaluates its arguments when the level is enabled, so a call in the
        // argument list is a region that does not run under a subscriber that
        // filters WARN out.
        let line = format!(
            "blueprint '{}': {} [{}]",
            blueprint.name,
            finding.one_line(),
            finding.code
        );
        tracing::warn!("{line}");
    }
}

/// Log the declared shape checks a caller's output-format override retires.
///
/// The CLI and the REST server also tell their own callers, but sub-agent and
/// ACP spawns have no warning channel of their own, so this line in the daemon
/// log is the one place the retirement is guaranteed to be recorded. Named
/// rather than inlined so a test can drive the warning path directly.
fn warn_retired_output_checks(
    run_id: &str,
    blueprint: &Blueprint,
    request: Option<&leviath_core::output::OutputSpec>,
) {
    for line in leviath_core::output::retired_check_warnings(blueprint, request) {
        tracing::warn!(run_id = %run_id, agent_name = %blueprint.name, "{line}");
    }
}

fn build_agent_inner(
    world: &mut World,
    deps: SpawnDeps<'_>,
    args: &SpawnArgs,
    enforce_seeds: bool,
) -> Result<Entity, String> {
    // 0. Everything that can be judged from the request alone.
    check_spawn_request(args)?;

    // 1. Load the blueprint (the client resolves the manifest path). Mutable
    // because step 2d writes each stage's global tool grants into it before the
    // runtime resolves the stages from it.
    let (content, mut blueprint) = load_blueprint(args, deps.config)?;

    // 2a. Entry stage + per-stage sandbox resolution. Each stage's effective
    // sandbox cascades stage → agent → global (`resolve_sandbox`); building the
    // manager creates any containers up front and fails here (returning the
    // error to the spawner) when a required runtime is unavailable and the deps.config
    // says to error. `None` means no stage is sandboxed → no executor attached
    // (zero overhead, exact prior host behavior).
    let entry_stage = blueprint
        .entry_stage
        .clone()
        .or_else(|| blueprint.stages.first().map(|s| s.name.clone()))
        .unwrap_or_default();
    let entry_index = blueprint
        .stages
        .iter()
        .position(|s| s.name == entry_stage)
        .unwrap_or(0);
    let stage_sandbox_by_index: Vec<leviath_core::ToolSandboxConfig> = blueprint
        .stages
        .iter()
        .map(|s| {
            leviath_core::resolve_sandbox(
                deps.config.sandbox.as_ref(),
                blueprint.sandbox.as_ref(),
                s.sandbox.as_ref(),
            )
        })
        .collect();
    let sandbox = crate::daemon::sandbox_manager::SandboxManager::build(
        &args.run_id,
        stage_sandbox_by_index,
        &args.workdir,
        entry_index,
    )?
    .map(Arc::new);

    // 2b. Per-agent built-in tools (over the agent's workdir), routing shell
    // execution through the sandbox when one is configured. The blueprint's
    // `[read_paths]` declarations are resolved against the user's deps.config here -
    // declared AND granted, or the read tools never leave the workdir.
    let (read_path_policy, read_path_warning) =
        build_read_path_policy(&blueprint, deps.config, std::path::Path::new(&args.workdir))?;
    if let Some(warning) = &read_path_warning {
        tracing::warn!(agent_name = %blueprint.name, "{warning}");
    }
    // Whether the agent can actually read outside its workdir - feeds the
    // taint bump below, captured before the policy moves into the context.
    let read_paths_granted = read_path_policy.is_active()
        && (read_path_policy.allow_blueprint || !read_path_policy.grants.is_empty());
    // Seeding runs below, after the policy has moved into the tool context, and
    // a seed path answers to the same policy a `read_file` would.
    let seed_read_paths = read_path_policy.clone();
    // The same question, per entry, recorded on the run so `lev ps` can show
    // that a live run is up but blind to paths its author designed it around.
    let read_path_counts =
        read_path_grant_counts(&blueprint, deps.config, std::path::Path::new(&args.workdir));
    let tool_ctx = leviath_tools::ToolContext::new(std::path::PathBuf::from(&args.workdir))
        .with_read_paths(read_path_policy)
        .with_shell_env(shell_env_policy(deps.config));
    let mut builtins = leviath_tools::BuiltinTools::new(tool_ctx);
    if let Some(mgr) = &sandbox {
        builtins =
            builtins.with_shell_executor(mgr.clone() as Arc<dyn leviath_tools::ShellExecutor>);
    }
    let builtin_names: HashSet<String> = builtins.names().into_iter().collect();
    // `install_tool` refuses every name discovery would drop a script for: the
    // built-ins, the sub-agent tools, and this run's MCP tools. The same set
    // the dynamic re-scan below reserves, so an install never reports a tool
    // that the next scan silently ignores.
    let builtins = Arc::new(
        builtins.with_reserved_names(
            reserved_tool_names(&builtin_names, deps.mcp_tool_defs)
                .into_iter()
                .collect(),
        ),
    );
    let mut all_tool_defs = builtins.tool_defs();
    all_tool_defs.extend(leviath_tools::BuiltinTools::subagent_tool_defs());
    all_tool_defs.extend(deps.mcp_tool_defs.iter().cloned());
    // The non-script defs (built-in + sub-agent + MCP), captured before script
    // defs are appended - a `dynamic_tools` agent re-filters against these plus a
    // fresh script scan on each mid-run refresh.
    let static_tool_defs = all_tool_defs.clone();

    // 2c. Rhai script tools: discover and compile the agent's
    // `tools/` dir plus the global `~/.leviath/tools/` (per-agent wins on a name
    // collision). Their defs are added to `all_tool_defs` *before* stage
    // resolution so a stage's `available_tools` (Layer 1) and taint
    // classification see them. A script tool whose name collides with a built-in,
    // sub-agent, or MCP tool is ignored (the existing tool wins), so it never
    // shadows a core tool.
    // A `dynamic_tools` agent also scans its run workdir's `tools/`, so a tool it
    // writes mid-run (into a workdir it can reach) is discoverable on re-scan.
    let dynamic_tools = blueprint.dynamic_tools;
    let workdir_tools_dir =
        dynamic_tools.then(|| std::path::PathBuf::from(&args.workdir).join("tools"));
    let (script_tools, script_tool_names, script_defs) = discover_script_tools(
        &args.blueprint_path,
        &builtin_names,
        deps.mcp_tool_defs,
        workdir_tools_dir.clone(),
    );
    all_tool_defs.extend(script_defs);

    // 2d. Global tool grants. `available_tools` is exact-match, so a tool an
    // earlier run installed into `~/.leviath/tools/` is invisible to a stage
    // that does not name it; a stage with `available_global_tools` asks for
    // every such tool. The expansion is written into the blueprint itself,
    // before stage resolution, because that is where the runtime reads each
    // stage's grant list from - and the `stage_available` snapshot taken
    // below inherits it, so a `dynamic_tools` refresh re-filters against the
    // same expanded list. Only scripts discovered from the global directory
    // count (see `global_tool_names`): a workdir or agent-dir script that
    // shadows a global name is never granted this way.
    let global_tools_dir = leviath_core::tools_dir();
    let global_names = global_tool_names(
        &script_tools,
        &script_tool_names,
        global_tools_dir.as_deref(),
    );
    for stage in &mut blueprint.stages {
        if stage.available_global_tools {
            stage.available_tools =
                expand_global_grants(&stage.available_tools, true, &global_names);
        }
    }

    // 3. Resolve stages against the world's providers.
    let stages = {
        let registry = &world
            .get_resource::<Providers>()
            .expect("Providers resource present in a PipelineWorld")
            .0;
        resolve_stages(
            &blueprint,
            args.model.as_deref(),
            &model_defaults(deps.config),
            registry,
            leviath_runtime::pipeline::ToolCatalog {
                defs: &all_tool_defs,
                owners: deps.mcp_tool_owners,
            },
            args.yolo,
            args.output.as_ref(),
        )?
    };

    // 4. Snapshot the blueprint bits we need after it's moved into the world.
    let agent_name = blueprint.name.clone();
    let num_stages = blueprint.stages.len();
    let compaction = blueprint.compaction_config.clone();
    let max_child_depth = blueprint.max_child_depth.unwrap_or(DEFAULT_SUBAGENT_DEPTH);
    // Taint gate: opt-in via the blueprint's `[security]` block, else the global
    // deps.config's `taint_tracking`, else off. Cascading through
    // Taint: whether this agent's outbound calls are gated, and against what.
    let TaintSetup {
        security,
        mcp_overrides,
        tool_sensitivities,
    } = resolve_taint_setup(
        world,
        &blueprint,
        deps.config,
        &all_tool_defs,
        read_paths_granted,
    );

    // Per-stage tool permissions (in stage order) + the entry stage's index, for
    // the tool state's stage-scoped policy layer.
    let stage_perms_by_index: Vec<HashMap<String, String>> = blueprint
        .stages
        .iter()
        .map(|s| s.tool_permissions.clone())
        .collect();
    // Agent-level tool permissions (the manifest's top-level `[tool_permissions]`,
    // recorded in blueprint metadata). Populates the tool state's agent-level
    // policy layer (between stage and global in `resolve_policy`) - without this
    // the manifest's top-level block would be silently ignored.
    let agent_perms = blueprint.agent_tool_permissions();
    // Each stage's Layer-1 allowlist, captured before the blueprint moves - a
    // `dynamic_tools` agent re-filters against these on refresh.
    //
    // Connector grants are expanded here rather than left for the refresh to
    // redo, so the refresh filters against exactly the list spawn resolved.
    // Re-expanding would ask the same question of a table that cannot have
    // changed - MCP defs are fixed for the run - and could only differ by
    // being wrong.
    let stage_available: Vec<Vec<String>> = blueprint
        .stages
        .iter()
        .map(|s| {
            leviath_runtime::pipeline::expand_connector_grants(
                &s.available_tools,
                &s.available_connectors,
                deps.mcp_tool_owners,
            )
        })
        .collect();
    // Alongside it, each stage's `required_tools` - the human tools it keeps even
    // when nobody is watching - so a refresh re-applies the same unattended cut.
    let stage_required: Vec<Vec<String>> = blueprint
        .stages
        .iter()
        .map(|s| s.required_tools.clone())
        .collect();
    // And which stages hold a global grant, so a refresh can extend the list
    // with a tool installed *during* the run - the snapshot above only knows
    // the global inventory as it stood at spawn.
    let stage_global: Vec<bool> = blueprint
        .stages
        .iter()
        .map(|s| s.available_global_tools)
        .collect();
    // The same list as a lookup set, canonicalised, for the tool state: an
    // interaction for a kept tool has to reach a real person rather than the
    // auto-answering backend, and dispatch tests one name at a time.
    let stage_required_by_index: Vec<HashSet<String>> = stage_required
        .iter()
        .map(|names| {
            names
                .iter()
                .map(|n| leviath_tools::canonical_tool_name(n).to_string())
                .collect()
        })
        .collect();
    let model_label = stages
        .first()
        .map(|s| format!("{}/{}", s.provider_name, s.model));
    // The entry stage's resolved model and everything it would fail over to,
    // flattened to the `(provider, model)` pairs the title lane speaks. Taken
    // here, while `stages` is in hand, because `attach_run_record` sees only
    // the label - and a label is one provider where the run has a whole chain.
    let entry_stage_candidates: Vec<(String, String)> = stages
        .first()
        .map(|s| leviath_runtime::title::stage_pairs(&s.provider_name, &s.model, &s.fallbacks))
        .unwrap_or_default();

    // The tool-permission layers and the Rhai script host, resolved here
    // rather than beside the tool-state registration below because a
    // `seed = { tools = [...] }` needs them: a seeded call answers to the
    // same policy a mid-run call does, and a seeded *script* tool needs the
    // host it would run under. Everything they read is already bound.
    // Launch overrides: `--yolo` allows every tool (`*` wildcard); `--allow X`
    // allows tool `X` outright.
    let mut launch_overrides: HashMap<String, crate::config::ToolPolicy> = HashMap::new();
    if args.yolo {
        launch_overrides.insert("*".to_string(), crate::config::ToolPolicy::Allow);
    }
    for tool in &args.allow {
        launch_overrides.insert(tool.clone(), crate::config::ToolPolicy::Allow);
    }
    // Rhai script-tool host (Layer 3): resolve `[tool_script_permissions]` once,
    // with `read_file`/`shell` `inherit` deferring to the agent's own resolved
    // policy for that built-in (evaluated against the entry stage).
    let entry_stage_perms = stage_perms_by_index
        .get(entry_index)
        .cloned()
        .unwrap_or_default();
    // The agent may carry its own `[tool_script_permissions]` (it can ship its own
    // tool scripts), overlaid per field on the global deps.config.
    let effective_script_perms = crate::daemon::script_host::effective_script_permissions(
        &deps.config.tool_script_permissions,
        &content,
    );
    // Same ceiling `build_tool_state` resolves for the built-in tools: the
    // global `[tool_permissions]` with this agent's `[agent_tool_permissions]`
    // grants overlaid. Passing the raw global map here would silently ignore a
    // per-agent grant when a script tool's `inherit` defers to the built-in.
    let agent_scoped_perms = deps.config.permissions_for_agent(&agent_name);
    let script_allow = crate::daemon::script_host::resolve_script_permissions(
        &effective_script_perms,
        &|builtin| {
            crate::tools::resolve_policy(
                builtin,
                true,
                &launch_overrides,
                &entry_stage_perms,
                &agent_perms,
                &agent_scoped_perms,
                deps.config.security.allow_blueprint_permissions,
            )
        },
    );
    // One write budget per run, created before the seeds and the spawn-time
    // scripts so they spend the same ceiling the tool lane checks from turn
    // one. Building it with the tool state would put it after both.
    let writes = Arc::new(crate::daemon::tool_service::WriteBudget::new(
        deps.config.limits.write_limits(),
    ));
    let script_host: Arc<dyn leviath_scripting::ScriptHost> = Arc::new(
        crate::daemon::script_host::DaemonScriptHost::new(
            script_allow,
            std::path::PathBuf::from(&args.workdir),
        )
        .with_write_budget(writes.clone())
        // Route a script `shell()` through the agent's per-stage sandbox (so a
        // script can't escape the isolation the stage declared) and cap it at the
        // configured wall-clock timeout.
        .with_shell(
            sandbox.clone(),
            std::time::Duration::from_secs(deps.config.limits.script_shell_timeout_secs),
            shell_env_policy(deps.config),
        )
        // `[security] allow_local_network`. Off by default, so a `web_fetch` URL
        // the model picked out of attacker-influenced context cannot reach cloud
        // metadata, the user's own `lev serve`, or their LAN.
        .with_local_network(deps.config.security.allow_local_network)
        // `[security] allow_env_vars`. Empty by default, so a script tool cannot
        // read the user's provider keys and post them somewhere.
        .with_env_allowlist(deps.config.security.allow_env_vars.clone()),
    );

    // 5. Resolve region seeds (caller input + blueprint-declared sources) into
    // concrete content. On a fresh spawn (`enforce_seeds`), required caller-input
    // regions that weren't provided fail here - before any inference, so no
    // tokens are spent. On reload the window is restored from a snapshot after
    // this, so seeding is skipped entirely.
    // Command seeds run here, so they inherit the entry stage's
    // sandbox (built in step 2a above) and are refused by either the machine-wide
    // `[security] allow_seed_commands` switch or this run's `--no-seed-commands`.
    let seeds = if enforce_seeds {
        let policy = SeedCommandPolicy::new(
            deps.config.security.allow_seed_commands && !args.no_seed_commands,
            std::time::Duration::from_secs(deps.config.limits.script_shell_timeout_secs),
            // The same pre-approval this run gives the `shell` tool. A seed runs
            // before any prompt exists, so the safe list is the only thing that
            // can have said yes to it.
            Arc::new(
                deps.config
                    .safe_keys_for_agent(&agent_name, blueprint.safe_commands.as_ref())
                    .into_keys()
                    .collect(),
            ),
            sandbox.clone(),
            shell_env_policy(deps.config),
        );
        // A tool seed answers to the same layered resolution the tool lane
        // applies mid-run, so a user's `[tool_permissions]` counts at spawn
        // too - and a seed can reach nothing the agent could not reach.
        let seed_launch = launch_overrides.clone();
        let seed_stage = entry_stage_perms.clone();
        let seed_agent = agent_perms.clone();
        let seed_global = agent_scoped_perms.clone();
        let seed_may_loosen = deps.config.security.allow_blueprint_permissions;
        let tool_policy = crate::daemon::seed_tool::SeedToolPolicy::new(
            crate::daemon::seed_tool::production_runner(
                crate::daemon::seed_tool::SeedToolContext {
                    builtins: builtins.clone(),
                    builtin_names: builtin_names.clone(),
                    script_tools: script_tools.clone(),
                    script_host: script_host.clone(),
                    mcp: deps.shared_mcp.clone(),
                    writes: writes.clone(),
                },
                Arc::new(move |name: &str, is_builtin: bool| {
                    crate::daemon::seed_tool::SeedToolPermissions {
                        launch: &seed_launch,
                        stage: &seed_stage,
                        agent: &seed_agent,
                        global: &seed_global,
                        may_loosen: seed_may_loosen,
                    }
                    .resolve(name, is_builtin)
                }),
            ),
        );
        resolve_seeds(
            &blueprint,
            args,
            &args.workdir,
            &policy,
            &tool_policy,
            &seed_read_paths,
        )?
    } else {
        HashMap::new()
    };

    // 5b. Read + compile-check custom regions' Rhai scripts - once per
    // distinct path, blueprint-dir-relative. Runs on fresh spawns
    // AND reloads (the hooks must work after a restart), and a broken script
    // is a hard error either way.
    let region_scripts = resolve_region_scripts(&blueprint, &args.blueprint_path)?;
    let stage_hooks = resolve_stage_hook_scripts(&blueprint, &args.blueprint_path)?;
    let output_validators = resolve_output_validators(&blueprint, &args.blueprint_path)?;

    // A requested output format that differs from the declared one retires the
    // blueprint's validator and schema for the reshaped stages (see
    // `resolve_output_spec`). Deliberate, but no longer silent: every spawn
    // path funnels through here, so the daemon log always says what was lost.
    warn_retired_output_checks(&args.run_id, &blueprint, args.output.as_ref());

    // Whether any stage can produce a file change the framework would see -
    // asked here, while the blueprint is still in hand, because it cannot
    // change for the rest of the run. A run that could never write is never
    // reported as having written nothing.
    let outcome_flags = leviath_runtime::persistence::RunOutcomeFlags::for_blueprint(&blueprint);
    // Taken here for the same reason: the tool state is built after the
    // blueprint has been handed to the world, and this list cannot change.
    let blueprint_safe = blueprint.safe_commands.clone();
    // And this one, so a resume can recompile the read-path grants against the
    // config as it stands then.
    let blueprint_read_paths = blueprint.read_paths.clone();

    // 6. Spawn the agent.
    let entity = spawn_agent_seeded(
        world,
        leviath_runtime::pipeline::SeededSpawn {
            agent_id: args.run_id.clone(),
            blueprint,
            seeds,
            stages,
            global_hints: leviath_core::config::PromptHints {
                batch_tool: deps.config.batch_tool_hint,
                shell: deps.config.shell_hint,
            },
            global_nudge: deps.config.nudge.clone(),
            region_scripts,
        },
    )?;

    // Stage hooks, only when some stage declares one. Withholding the component
    // rather than attaching an empty map is what makes "no hooks, no cost"
    // literal: the hook systems' queries then skip the agent at the archetype
    // level and never look at it again.
    if !stage_hooks.is_empty() {
        world
            .entity_mut(entity)
            .insert(leviath_runtime::components::StageHookScripts(stage_hooks));
    }

    // 7. Attach run metadata / token totals / persistence watermark (+ optional
    // compaction settings).
    attach_run_record(
        world,
        entity,
        args,
        &deps,
        RunRecordParts {
            agent_name: agent_name.clone(),
            model_label: model_label.clone(),
            num_stages,
            read_path_counts,
            output_validators,
            outcome_flags,
            // `enforce_seeds` is false on the recovery path, which is also the
            // resume path - and a run being paged back in has already had its
            // shot at a title. Without this, every pause/resume cycle buys
            // another titling call for a run that either has a title or has
            // already failed to get one.
            //
            // The chain doubles as the eligibility answer: empty means no
            // titling. It is the entry stage's own candidate list behind
            // whatever `[title]` names, so the title call fails over exactly
            // where the run's inference does.
            title_chain: match enforce_seeds
                && deps.config.title.enabled
                && !args.task.is_empty()
                && args.parent_run_id.is_none()
            {
                true => leviath_runtime::title::title_chain(
                    &deps.config.title,
                    model_label.as_deref(),
                    &entry_stage_candidates,
                ),
                false => Vec::new(),
            },
            compaction,
            tool_sensitivities,
            security: security.clone(),
            mcp_overrides,
        },
    );

    // 8. Register the per-agent tool state.
    let subagent = SubAgentHandle {
        sender: deps.subagent_tx,
        parent_run_id: args.run_id.clone(),
        workdir: args.workdir.clone(),
        max_depth: max_child_depth,
        no_seed_commands: args.no_seed_commands,
        unattended: args.yolo,
        model_override: args.model.clone(),
    };
    // Build the dynamic-tools re-resolution context and tag the entity
    // `DynamicTools` so the runtime polls it for mid-run re-scans.
    let dynamic = dynamic_tools.then(|| {
        world
            .entity_mut(entity)
            .insert(leviath_runtime::pipeline::DynamicTools);
        Arc::new(crate::daemon::tool_service::DynamicToolCtx {
            scan_dirs: script_scan_dirs(&args.blueprint_path, workdir_tools_dir),
            reserved_names: reserved_tool_names(&builtin_names, deps.mcp_tool_defs),
            static_defs: static_tool_defs,
            stage_available,
            stage_required,
            stage_global,
            tools_dir: global_tools_dir,
            unattended: args.yolo,
            dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    });
    let state = build_tool_state(ToolStateParts {
        writes,
        builtins,
        builtin_names,
        mcp: deps.shared_mcp,
        config: deps.config,
        hub: deps.hub,
        run_id: &args.run_id,
        entry_stage: &entry_stage,
        entry_index,
        stage_perms_by_index,
        stage_required_by_index,
        agent_perms,
        agent_name: &agent_name,
        launch_overrides,
        subagent: Some(subagent),
        sandbox,
        script_tools,
        script_tool_names,
        script_host,
        dynamic,
        unattended: args.yolo,
        blueprint_safe: blueprint_safe.as_ref(),
        blueprint_read_paths: blueprint_read_paths.as_ref(),
        workdir: std::path::PathBuf::from(&args.workdir),
    });
    deps.tool_service.register(entity, state);

    Ok(entity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{FakeProvider, fixtures};
    use leviath_core::blueprint::ModelConfig;
    use leviath_runtime::ProviderRegistry;
    use leviath_runtime::world::PipelineWorld;

    /// A throwaway sub-agent op sender for tests that don't exercise the bridge.
    fn sub_tx() -> UnboundedSender<SubAgentOp> {
        tokio::sync::mpsc::unbounded_channel().0
    }

    /// What the daemon logs about a blueprint at spawn. A run is never refused
    /// for a lint finding, so the only way this surfaces is the log line - which
    /// makes it worth exercising directly rather than through a whole spawn.
    #[test]
    fn log_blueprint_lint_warns_about_findings_and_skips_notes() {
        crate::test_support::with_tracing(|| {});
        let home = tempfile::tempdir().unwrap();
        temp_env::with_var("LEVIATH_HOME", Some(home.path().to_str().unwrap()), || {
            // No `mode`, no `max_iterations`, an unattended `ask_user_text` and
            // a `[read_paths]` block: three warnings and one note.
            let manifest = r#"
[agent]
name = "noisy"
version = "0.1.0"

[stages.main]
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
available_tools = ["ask_user_text"]

[read_paths]
allow = ["~/.leviath/runs"]

[context.regions]
system = { kind = "pinned", max_tokens = 1000 }
"#;
            let bp = leviath_core::manifest::parse_manifest(manifest).unwrap();
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("agent.leviath");
            std::fs::write(&path, manifest).unwrap();

            // The findings the log walks, so the test asserts what is being
            // logged rather than only that logging did not panic.
            let env = crate::lint::LintEnv::offline(dir.path());
            let findings = crate::lint::lint_manifest(manifest, &bp, &env);
            assert!(
                findings
                    .iter()
                    .any(|f| f.severity == crate::lint::LintSeverity::Note),
                "the fixture needs a note for the skip arm to run"
            );
            assert!(
                findings
                    .iter()
                    .any(|f| f.severity == crate::lint::LintSeverity::Warning),
                "the fixture needs a warning for the log arm to run"
            );

            log_blueprint_lint(manifest, &bp, &path.to_string_lossy());
        });
    }

    /// A blueprint with nothing to say produces no log lines at all.
    #[test]
    fn log_blueprint_lint_is_silent_for_a_clean_blueprint() {
        crate::test_support::with_tracing(|| {});
        let home = tempfile::tempdir().unwrap();
        temp_env::with_var("LEVIATH_HOME", Some(home.path().to_str().unwrap()), || {
            let manifest = r#"
[agent]
name = "quiet"
version = "0.1.0"

[stages.main]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5

[context.regions]
system = { kind = "pinned", max_tokens = 1000 }
"#;
            let bp = leviath_core::manifest::parse_manifest(manifest).unwrap();
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("agent.leviath");
            std::fs::write(&path, manifest).unwrap();
            let env = crate::lint::LintEnv::offline(dir.path());
            assert!(crate::lint::lint_manifest(manifest, &bp, &env).is_empty());
            log_blueprint_lint(manifest, &bp, &path.to_string_lossy());
        });
    }

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
            default_model: Some("deepseek".to_string()),
            ..Default::default()
        };
        config.providers.fallback_order = vec!["anthropic/claude-sonnet-5".to_string()];
        let defaults = model_defaults(&config);
        assert_eq!(defaults.provider, "openrouter");
        assert_eq!(defaults.model.as_deref(), Some("deepseek"));
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
            let (set, names, defs) =
                discover_script_tools(blueprint.to_str().unwrap(), &builtins, &mcp, None);
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

    /// A global grant expands to the tools whose *file* is in the global
    /// directory and that survived discovery: a workdir script shadowing a
    /// global name is not one, a reserved name that discovery dropped is not
    /// one, and the result is sorted. With no global directory there is nothing
    /// to grant.
    #[test]
    fn global_tool_names_are_the_surviving_tools_from_the_global_dir_only() {
        let workdir = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let workdir_tools = workdir.path().join("tools");
        std::fs::create_dir(&workdir_tools).unwrap();
        // `echo` exists in both places; the workdir copy wins the scan.
        std::fs::write(workdir_tools.join("echo.rhai"), "// @tool echo\n\"repo\"").unwrap();
        std::fs::write(global.path().join("echo.rhai"), "// @tool echo\n\"global\"").unwrap();
        std::fs::write(global.path().join("zed.rhai"), "// @tool zed\n1").unwrap();
        std::fs::write(global.path().join("alpha.rhai"), "// @tool alpha\n1").unwrap();
        // Compiled, but a reserved name discovery would have dropped.
        std::fs::write(
            global.path().join("read_file.rhai"),
            "// @tool read_file\n1",
        )
        .unwrap();
        let (set, _skipped) = leviath_scripting::ScriptToolSet::discover(&[
            workdir_tools,
            global.path().to_path_buf(),
        ]);
        let surviving: HashSet<String> = ["echo", "zed", "alpha"]
            .into_iter()
            .map(str::to_string)
            .collect();

        assert_eq!(
            global_tool_names(&set, &surviving, Some(global.path())),
            vec!["alpha".to_string(), "zed".to_string()]
        );
        // A name discovery did not keep is not granted even from the global dir.
        let fewer: HashSet<String> = ["zed".to_string()].into_iter().collect();
        assert_eq!(
            global_tool_names(&set, &fewer, Some(global.path())),
            vec!["zed".to_string()]
        );
        assert!(global_tool_names(&set, &surviving, None).is_empty());
    }

    /// The grant list keeps the blueprint's order, appends what is global and
    /// new, names nothing twice, and is untouched for a stage without the flag.
    #[test]
    fn expand_global_grants_appends_without_duplicates_only_when_allowed() {
        let available = vec!["read_file".to_string(), "echo".to_string()];
        let global = vec!["alpha".to_string(), "echo".to_string()];
        assert_eq!(
            expand_global_grants(&available, true, &global),
            vec![
                "read_file".to_string(),
                "echo".to_string(),
                "alpha".to_string()
            ]
        );
        assert_eq!(expand_global_grants(&available, false, &global), available);
        assert_eq!(expand_global_grants(&[], true, &[]), Vec::<String>::new());
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
            let (set, names, defs) =
                discover_script_tools(blueprint.to_str().unwrap(), &HashSet::new(), &[], None);
            assert!(set.is_empty() && names.is_empty() && defs.is_empty());
        });
    }

    #[test]
    fn discover_script_tools_handles_pathless_blueprint() {
        // A blueprint path with no parent exercises the "no agent dir" arm; the
        // global tools/ scan still runs (empty here).
        let home = tempfile::tempdir().unwrap();
        temp_env::with_var("LEVIATH_HOME", Some(home.path().to_str().unwrap()), || {
            let (set, _n, _d) = discover_script_tools("", &HashSet::new(), &[], None);
            assert!(set.is_empty());
        });
    }
    use leviath_core::blueprint::ModelEntry;

    fn model_cfg(models: Vec<(&str, &str)>) -> ModelConfig {
        ModelConfig {
            models: models
                .into_iter()
                .map(|(p, m)| ModelEntry {
                    provider: p.to_string(),
                    model: m.to_string(),
                })
                .collect(),
            allow_user_default: true,
            parameters: HashMap::new(),
            request_timeout_secs: None,
        }
    }

    fn registry_with(providers: &[&str]) -> ProviderRegistry {
        let mut r = ProviderRegistry::new();
        for p in providers {
            r.register(p.to_string(), Arc::new(fake_provider()));
        }
        r
    }

    fn fake_provider() -> FakeProvider {
        FakeProvider::new().failing("test provider")
    }

    // ── build_agent (full spawn from a manifest) ──

    use leviath_providers::Provider;
    use leviath_runtime::components::AgentStatus;
    use leviath_runtime::inference_pool::InferencePoolConfig;
    use tokio::runtime::Handle;

    fn coder_manifest() -> String {
        // Self-contained fixture - not the shipped blueprint, so these spawn-logic
        // tests stay isolated from agents/coder edits.
        crate::test_support::inline_coder_manifest()
    }

    fn test_world() -> (PipelineWorld, Arc<CliToolService>) {
        let cli = Arc::new(CliToolService::new());
        let world = PipelineWorld::new(
            registry_with(&["anthropic", "openai", "ollama"]),
            cli.clone(),
            InferencePoolConfig::new(),
            1,
            None,
            Handle::current(),
        );
        (world, cli)
    }

    fn spawn_args(path: &str) -> SpawnArgs {
        SpawnArgs {
            run_id: "run-x".to_string(),
            blueprint_path: path.to_string(),
            // No task by default: most of these fixtures declare no region to
            // receive one, and supplying a task a blueprint cannot hold is now
            // refused. Tests that care about the task set it explicitly.
            task: String::new(),
            regions: HashMap::new(),
            model: None,
            workdir: std::env::temp_dir().to_string_lossy().to_string(),
            metadata: HashMap::new(),
            callback_url: None,
            callback_secret: None,
            yolo: false,
            no_seed_commands: false,
            allow: Vec::new(),
            max_depth: None,
            parent_run_id: None,
            output: None,
        }
    }

    // ─── resolve_region_scripts ──────────────────────────────────────────

    /// Manifest with a global custom region and a per-stage one, both
    /// pointing into `hooks/` next to the manifest.
    fn custom_region_manifest() -> &'static str {
        "[agent]\nname = \"cr\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n\
         [context.regions.brain]\nkind = \"custom\"\nscript = \"hooks/brain.rhai\"\nmax_tokens = 4000\n\n\
         [stages.main]\nmodel = { provider = \"anthropic\", model = \"m\" }\n\n\
         [stages.main.context.regions.stage_view]\nkind = \"custom\"\nscript = \"hooks/stage.rhai\"\nmax_tokens = 2000\n"
    }

    // ── output validators ──

    fn validator_blueprint(agent_script: Option<&str>, stage_script: Option<&str>) -> Blueprint {
        let mut bp = leviath_core::manifest::parse_manifest(
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

    /// The daemon-log lines for a format override that retires a declared
    /// validator. The wording lives in `leviath_core::output` and is tested
    /// there; this drives the logging path every spawn walks through, both
    /// when it has something to say and when it has nothing.
    #[test]
    fn warn_retired_output_checks_logs_the_retirement() {
        // Under a real subscriber, so the macro's field expressions run.
        crate::test_support::with_tracing(|| {
            let bp = validator_blueprint(Some("validators/shape.rhai"), None);
            let request = leviath_core::output::OutputSpec {
                format: Some("json".to_string()),
                ..leviath_core::output::OutputSpec::default()
            };
            assert_eq!(
                leviath_core::output::retired_check_warnings(&bp, Some(&request)).len(),
                1,
                "the fixture needs a retirement for the log line to run"
            );
            warn_retired_output_checks("run-1", &bp, Some(&request));
            warn_retired_output_checks("run-1", &bp, None);
        });
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

    fn hooked_manifest(hooks: &str) -> leviath_core::Blueprint {
        leviath_core::manifest::parse_manifest(&format!(
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
        let bp = leviath_core::manifest::parse_manifest(
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
        let bp = leviath_core::manifest::parse_manifest(custom_region_manifest()).unwrap();
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
        let bp = leviath_core::manifest::parse_manifest(
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
        let bp = leviath_core::manifest::parse_manifest(custom_region_manifest()).unwrap();
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
        let bp = leviath_core::manifest::parse_manifest(custom_region_manifest()).unwrap();
        let err = resolve_region_scripts(&bp, &manifest.to_string_lossy()).unwrap_err();
        assert!(err.contains("failed to compile"), "{err}");
        assert!(err.contains("region 'brain'"), "{err}");
    }

    #[tokio::test]
    async fn build_agent_fails_fast_on_a_broken_custom_region_script() {
        // The resolve error propagates out of build_agent before any tokens
        // are spent - a hook that silently never ran would change every
        // inference with no signal.
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(&manifest, custom_region_manifest()).unwrap();

        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        let args = spawn_args(&manifest.to_string_lossy());
        let err = build_agent(
            world.world_mut(),
            SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 100,
                subagent_tx: sub_tx(),
            },
            &args,
        )
        .unwrap_err();
        assert!(err.contains("region 'brain'"), "got: {err}");
        assert!(err.contains("hooks/brain.rhai"), "got: {err}");
    }

    /// The run id becomes a directory name and everything a run writes lands
    /// under it. The persistence lane joins it to the runs directory without
    /// checking, so the check belongs at the boundary that accepts the request.
    ///
    /// The blueprint path here points at nothing, which is the point: the error
    /// must be about the run id, proving the guard runs before anything is read
    /// off disk.
    #[tokio::test]
    async fn build_agent_rejects_a_run_id_that_is_not_a_directory_name() {
        for bad in ["../escape", "a/b", "..", ".", ""] {
            let (mut world, cli) = test_world();
            let hub = InteractionHub::new();
            let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
            let mut args = spawn_args("/nonexistent/agent.leviath");
            args.run_id = bad.to_string();
            let err = build_agent(
                world.world_mut(),
                SpawnDeps {
                    tool_service: cli.as_ref(),
                    config: &Config::default(),
                    shared_mcp: mcp,
                    mcp_tool_defs: &[],
                    mcp_tool_owners: &Default::default(),
                    hub: &hub,
                    now_secs: 100,
                    subagent_tx: sub_tx(),
                },
                &args,
            )
            .unwrap_err();
            assert!(
                err.contains("run id"),
                "run id {bad:?} names a directory and must be refused, got: {err}"
            );
        }
    }

    #[tokio::test]
    async fn build_agent_rejects_a_workdir_that_is_missing_or_not_a_directory() {
        // `ToolContext::new` silently keeps a path it can't canonicalize, so
        // without this check a bogus workdir spawns a healthy-looking agent
        // whose every tool call then fails with ENOENT.
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(
            &manifest,
            "[agent]\nname = \"w\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n\
             [stages.main]\nmodel = { provider = \"anthropic\", model = \"m\" }\n",
        )
        .unwrap();
        let not_a_dir = dir.path().join("a-file");
        std::fs::write(&not_a_dir, "x").unwrap();

        for workdir in [
            dir.path()
                .join("does-not-exist")
                .to_string_lossy()
                .to_string(),
            not_a_dir.to_string_lossy().to_string(),
        ] {
            let (mut world, cli) = test_world();
            let hub = InteractionHub::new();
            let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
            let mut args = spawn_args(&manifest.to_string_lossy());
            args.workdir = workdir.clone();
            let err = build_agent(
                world.world_mut(),
                SpawnDeps {
                    tool_service: cli.as_ref(),
                    config: &Config::default(),
                    shared_mcp: mcp,
                    mcp_tool_defs: &[],
                    mcp_tool_owners: &Default::default(),
                    hub: &hub,
                    now_secs: 100,
                    subagent_tx: sub_tx(),
                },
                &args,
            )
            .unwrap_err();
            assert!(err.contains("workspace"), "got: {err}");
            assert!(err.contains(&workdir), "got: {err}");
        }
    }

    /// A real spawn, end to end: the blueprint seeds a region from a tool, and
    /// the region the agent starts with holds that tool's answer.
    ///
    /// The unit tests above drive `resolve_seeds` with a stub runner, so they
    /// prove the shape but not the wiring. What this one covers is the wiring:
    /// that the spawn path builds a runner over the agent's real tools, that
    /// the policy layers reach it, and that the result lands in the window
    /// rather than in a map nobody reads.
    ///
    /// Multi-thread because the seeded call is awaited on the ambient runtime -
    /// see `block_on_daemon`, which is what a daemon spawn does too.
    #[tokio::test(flavor = "multi_thread")]
    async fn build_agent_seeds_a_region_from_a_real_tool_call() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(
            &manifest,
            "[agent]\nname = \"seeded\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n\
             [stages.main]\nmodel = { provider = \"anthropic\", model = \"m\" }\n\n\
             [context.regions]\n\
             task = { kind = \"pinned\", max_tokens = 4000, seed = \"task_input\" }\n\
             environment = { kind = \"pinned\", max_tokens = 1000, \
             seed = { tools = [\"current_time\", \"locale_info\"] } }\n",
        )
        .unwrap();
        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        let mut args = spawn_args(&manifest.to_string_lossy());
        args.workdir = dir.path().to_string_lossy().to_string();
        let entity = build_agent(
            world.world_mut(),
            SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 100,
                subagent_tx: sub_tx(),
            },
            &args,
        )
        .expect("spawn succeeds");

        let window = world
            .world()
            .get::<leviath_runtime::components::ContextWindow>(entity)
            .expect("the agent has a window");
        let region = window
            .regions
            .iter()
            .find(|r| r.name == "environment")
            .expect("the seeded region exists");
        let content: String = region
            .content
            .iter()
            .map(|e| e.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        // Both calls ran, each under its own heading.
        assert!(content.contains("--- current_time ---"), "{content}");
        assert!(content.contains("--- locale_info ---"), "{content}");
        // And it is the tool's real answer, not a placeholder: the clock's
        // reading parses back as the instant it claims to be.
        let (_, after) = content
            .split_once("--- current_time ---")
            .expect("the clock block");
        let (json, _) = after
            .split_once("--- locale_info ---")
            .unwrap_or((after, ""));
        let v: serde_json::Value =
            serde_json::from_str(json.trim()).expect("the clock answered with JSON");
        assert!(
            chrono::DateTime::parse_from_rfc3339(v["utc"].as_str().expect("utc")).is_ok(),
            "{json}"
        );
    }

    /// A stage with `available_global_tools` is offered a tool installed in the
    /// global directory that its `available_tools` never named - at spawn, in
    /// the resolved stage the runtime reads, not only on a later refresh - and a
    /// stage without the flag is not. The `dynamic_tools` snapshot the refresh
    /// path filters against inherits the same expansion.
    #[tokio::test]
    async fn build_agent_grants_global_tools_to_a_stage_that_opted_in() {
        let home = tempfile::tempdir().unwrap();
        let global = home.path().join(".leviath").join("tools");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::write(
            global.join("echo.rhai"),
            "// @tool echo\n// @description say it back\nparams.x",
        )
        .unwrap();
        temp_env::async_with_vars(
            [("LEVIATH_HOME", Some(home.path().to_str().unwrap()))],
            async {
                let dir = tempfile::tempdir().unwrap();
                let manifest = dir.path().join("agent.leviath");
                std::fs::write(
                    &manifest,
                    "[agent]\nname = \"global\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\
                     dynamic_tools = true\n\n\
                     [stages.main]\nmodel = { provider = \"anthropic\", model = \"m\" }\n\
                     available_tools = [\"read_file\"]\navailable_global_tools = true\n\n\
                     [stages.plain]\nmodel = { provider = \"anthropic\", model = \"m\" }\n\
                     available_tools = [\"read_file\"]\n",
                )
                .unwrap();
                let (mut world, cli) = test_world();
                let hub = InteractionHub::new();
                let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
                let mut args = spawn_args(&manifest.to_string_lossy());
                args.workdir = dir.path().to_string_lossy().to_string();
                let entity = build_agent(
                    world.world_mut(),
                    SpawnDeps {
                        tool_service: cli.as_ref(),
                        config: &Config::default(),
                        shared_mcp: mcp,
                        mcp_tool_defs: &[],
                        mcp_tool_owners: &Default::default(),
                        hub: &hub,
                        now_secs: 100,
                        subagent_tx: sub_tx(),
                    },
                    &args,
                )
                .expect("spawn succeeds");

                // The entry stage's resolved tools, as the runtime will offer them.
                let mut offered: Vec<String> = world
                    .world()
                    .get::<leviath_runtime::pipeline::StageInference>(entity)
                    .expect("the entry stage is resolved")
                    .tools
                    .iter()
                    .map(|t| t.name.clone())
                    .collect();
                offered.sort_unstable();
                assert_eq!(offered, vec!["echo".to_string(), "read_file".to_string()]);

                // The refresh snapshot carries the expansion for the opted-in
                // stage only.
                let state = cli.state_for(entity).expect("registered");
                let dynamic = state.dynamic.as_ref().expect("dynamic_tools agent");
                assert_eq!(
                    dynamic.stage_available,
                    vec![
                        vec!["read_file".to_string(), "echo".to_string()],
                        vec!["read_file".to_string()],
                    ]
                );
                assert_eq!(dynamic.stage_global, vec![true, false]);
                assert_eq!(dynamic.tools_dir.as_deref(), Some(global.as_path()));
            },
        )
        .await;
    }

    /// The global grant is decided by where a script lives, not by its name: a
    /// `dynamic_tools` run whose workdir ships `tools/echo.rhai` under the same
    /// name as a global `echo` has the workdir copy win discovery, and that copy
    /// is repository content the grant must not advertise. The genuinely global
    /// `lint` still is.
    #[tokio::test]
    async fn build_agent_does_not_grant_a_workdir_script_shadowing_a_global_tool() {
        let home = tempfile::tempdir().unwrap();
        let global = home.path().join(".leviath").join("tools");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::write(global.join("echo.rhai"), "// @tool echo\n\"global\"").unwrap();
        std::fs::write(global.join("lint.rhai"), "// @tool lint\n\"ok\"").unwrap();
        temp_env::async_with_vars(
            [("LEVIATH_HOME", Some(home.path().to_str().unwrap()))],
            async {
                let dir = tempfile::tempdir().unwrap();
                let workdir_tools = dir.path().join("tools");
                std::fs::create_dir(&workdir_tools).unwrap();
                std::fs::write(workdir_tools.join("echo.rhai"), "// @tool echo\n\"repo\"").unwrap();
                let manifest = dir.path().join("agent.leviath");
                std::fs::write(
                    &manifest,
                    "[agent]\nname = \"shadowed\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\
                     dynamic_tools = true\n\n\
                     [stages.main]\nmodel = { provider = \"anthropic\", model = \"m\" }\n\
                     available_tools = [\"read_file\"]\navailable_global_tools = true\n",
                )
                .unwrap();
                let (mut world, cli) = test_world();
                let hub = InteractionHub::new();
                let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
                let mut args = spawn_args(&manifest.to_string_lossy());
                args.workdir = dir.path().to_string_lossy().to_string();
                let entity = build_agent(
                    world.world_mut(),
                    SpawnDeps {
                        tool_service: cli.as_ref(),
                        config: &Config::default(),
                        shared_mcp: mcp,
                        mcp_tool_defs: &[],
                        mcp_tool_owners: &Default::default(),
                        hub: &hub,
                        now_secs: 100,
                        subagent_tx: sub_tx(),
                    },
                    &args,
                )
                .expect("spawn succeeds");

                let mut offered: Vec<String> = world
                    .world()
                    .get::<leviath_runtime::pipeline::StageInference>(entity)
                    .expect("the entry stage is resolved")
                    .tools
                    .iter()
                    .map(|t| t.name.clone())
                    .collect();
                offered.sort_unstable();
                assert_eq!(offered, vec!["lint".to_string(), "read_file".to_string()]);
                // The shadowing copy was discovered (it would answer an explicit
                // `available_tools` entry); it simply earned no global grant.
                let state = cli.state_for(entity).expect("registered");
                assert!(state.script_tool_names.lock().unwrap().contains("echo"));
            },
        )
        .await;
    }

    #[tokio::test]
    async fn build_agent_attaches_taint_gate_when_security_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(
            &manifest,
            "[agent]\nname = \"sec\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n\
             [security]\ntaint_tracking = true\n\n\
             [stages.main]\nmodel = { provider = \"anthropic\", model = \"m\" }\n",
        )
        .unwrap();
        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        let entity = build_agent(
            world.world_mut(),
            SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 100,
                subagent_tx: sub_tx(),
            },
            &spawn_args(&manifest.to_string_lossy()),
        )
        .expect("spawn succeeds");

        // Taint opt-in ⇒ gate + sensitivities attached and window tracking on.
        assert!(
            world
                .world()
                .get::<leviath_runtime::TaintGate>(entity)
                .is_some()
        );
        assert!(
            world
                .world()
                .get::<leviath_runtime::pipeline::ToolSensitivities>(entity)
                .is_some()
        );
        assert!(
            world
                .world()
                .get::<leviath_runtime::components::ContextWindow>(entity)
                .unwrap()
                .overall_taint()
                .is_some()
        );
        // Without `--yolo`, the gate stays interactive: no auto-approve marker.
        assert!(
            world
                .world()
                .get::<leviath_runtime::components::GateAutoApprove>(entity)
                .is_none()
        );
    }

    /// A tool the user granted outright still answers to the taint gate.
    ///
    /// `[tool_permissions]` and the gate are separate layers asking separate
    /// questions - "may this agent call `shell` at all" and "may *this* data
    /// reach it" - and only `--yolo` waives the second. Granting a tool must
    /// not quietly grant the data too: measured live, a run with
    /// `shell = "allow"` and a Private read still raised the leak prompt, and
    /// denying it kept the command from running.
    #[tokio::test]
    async fn build_agent_tool_permission_allow_does_not_waive_the_taint_gate() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(
            &manifest,
            "[agent]\nname = \"sec\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n\
             [security]\ntaint_tracking = true\n\n\
             [stages.main]\nmodel = { provider = \"anthropic\", model = \"m\" }\n\
             available_tools = [\"shell\"]\n",
        )
        .unwrap();
        let mut config = Config::default();
        config
            .tool_permissions
            .insert("shell".to_string(), crate::config::ToolPolicy::Allow);
        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        let entity = build_agent(
            world.world_mut(),
            SpawnDeps {
                tool_service: cli.as_ref(),
                config: &config,
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 100,
                subagent_tx: sub_tx(),
            },
            &spawn_args(&manifest.to_string_lossy()),
        )
        .expect("spawn succeeds");

        assert!(
            world
                .world()
                .get::<leviath_runtime::TaintGate>(entity)
                .is_some(),
            "the gate is attached regardless of tool permissions"
        );
        assert!(
            world
                .world()
                .get::<leviath_runtime::components::GateAutoApprove>(entity)
                .is_none(),
            "only --yolo waives the gate; a tool grant does not"
        );
    }

    #[tokio::test]
    async fn build_agent_marks_root_runs_for_titling_but_not_subagents() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(
            &manifest,
            // Titling is gated on a non-empty task, so this blueprint has to
            // accept one - a region named `task` picks it up implicitly.
            "[agent]\nname = \"titler\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n\
             [stages.main]\nmodel = { provider = \"anthropic\", model = \"m\" }\n\n\
             [context.regions]\ntask = { kind = \"pinned\", max_tokens = 1000 }\n",
        )
        .unwrap();
        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();

        // Root run with the default-enabled [title] config: marked.
        let root = build_agent(
            world.world_mut(),
            SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 100,
                subagent_tx: sub_tx(),
            },
            &SpawnArgs {
                task: "title me".to_string(),
                ..spawn_args(&manifest.to_string_lossy())
            },
        )
        .expect("spawn succeeds");
        assert!(
            world
                .world()
                .get::<leviath_runtime::title::PendingTitle>(root)
                .is_some()
        );

        // A sub-agent run is never marked: titles serve the top-level run list.
        let mut child_args = spawn_args(&manifest.to_string_lossy());
        child_args.run_id = "run-child".to_string();
        child_args.parent_run_id = Some("run-x".to_string());
        let child = build_agent(
            world.world_mut(),
            SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 100,
                subagent_tx: sub_tx(),
            },
            &child_args,
        )
        .expect("spawn succeeds");
        assert!(
            world
                .world()
                .get::<leviath_runtime::title::PendingTitle>(child)
                .is_none()
        );

        // Disabled config: not marked.
        let config = Config {
            title: leviath_core::config::TitleConfig {
                enabled: false,
                provider: None,
                model: None,
            },
            ..Config::default()
        };
        let mut off_args = spawn_args(&manifest.to_string_lossy());
        off_args.run_id = "run-off".to_string();
        let off = build_agent(
            world.world_mut(),
            SpawnDeps {
                tool_service: cli.as_ref(),
                config: &config,
                shared_mcp: Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 100,
                subagent_tx: sub_tx(),
            },
            &off_args,
        )
        .expect("spawn succeeds");
        assert!(
            world
                .world()
                .get::<leviath_runtime::title::PendingTitle>(off)
                .is_none()
        );
    }

    #[tokio::test]
    async fn build_agent_applies_policy_mcp_overrides_to_the_gate() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(
            &manifest,
            "[agent]\nname = \"sec-ov\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n\
             [security]\ntaint_tracking = true\n\n\
             [stages.main]\nmodel = { provider = \"anthropic\", model = \"m\" }\n",
        )
        .unwrap();
        let (mut world, cli) = test_world();
        // The daemon loads policy.toml into this resource at setup; an
        // [mcp_overrides] entry there must reach the gate attached at spawn,
        // not just `lev policy list` output.
        world
            .world_mut()
            .insert_resource(leviath_runtime::pipeline::PolicyGate(
                leviath_core::PolicyConfig {
                    allowlist: Vec::new(),
                    mcp_overrides: HashMap::from([(
                        "notes.share".to_string(),
                        leviath_core::policy::McpToolOverride {
                            sensitivity: None,
                            direction: Some("outbound".to_string()),
                            clearance: Some(leviath_core::TaintLevel::Private),
                        },
                    )]),
                },
            ));
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        let entity = build_agent(
            world.world_mut(),
            SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 100,
                subagent_tx: sub_tx(),
            },
            &spawn_args(&manifest.to_string_lossy()),
        )
        .expect("spawn succeeds");

        let gate = world
            .world()
            .get::<leviath_runtime::TaintGate>(entity)
            .expect("gate attached");
        let classification = gate.tool_classification("notes.share");
        assert_eq!(
            classification.direction,
            leviath_core::taint::ToolDirection::Outbound
        );
        assert_eq!(classification.clearance, leviath_core::TaintLevel::Private);
    }

    #[tokio::test]
    async fn build_agent_errors_when_required_caller_region_missing() {
        // A required caller-input region that the request doesn't provide makes
        // build_agent fail (via resolve_seeds) before spawning - no inference.
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(
            &manifest,
            "[agent]\nname = \"needs\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n\
             [stages.main]\nmodel = { provider = \"anthropic\", model = \"m\" }\n\n\
             [context.regions]\n\
             spec = { kind = \"pinned\", max_tokens = 2000, seed = \"input\", required = true }\n\
             conversation = { kind = \"sliding_window\", max_items = 20, max_tokens = 10000 }\n",
        )
        .unwrap();
        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        // spawn_args() provides only the task, not the required `spec` region.
        let err = build_agent(
            world.world_mut(),
            SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 100,
                subagent_tx: sub_tx(),
            },
            &spawn_args(&manifest.to_string_lossy()),
        )
        .unwrap_err();
        assert!(err.contains("spec"), "got: {err}");
    }

    #[tokio::test]
    async fn build_agent_attaches_sandbox_when_configured() {
        // A `namespace` sandbox with `on_unavailable = "warn"` builds on every
        // platform without running any external command, so this deterministically
        // exercises the spawn-side sandbox wiring (manager built + attached).
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(
            &manifest,
            "[agent]\nname = \"sb\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n\
             [sandbox]\nkind = \"namespace\"\non_unavailable = \"warn\"\n\n\
             [stages.main]\nmodel = { provider = \"anthropic\", model = \"m\" }\n",
        )
        .unwrap();
        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        let entity = build_agent(
            world.world_mut(),
            SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 100,
                subagent_tx: sub_tx(),
            },
            &spawn_args(&manifest.to_string_lossy()),
        )
        .expect("spawn succeeds");
        // The agent's tool state carries a sandbox manager.
        let state = cli.take(entity).expect("state registered");
        assert!(state.sandbox.is_some(), "sandbox manager attached");
    }

    /// The spawn hands `install_tool` the run's MCP tool names as reserved: a
    /// script under one of them is dropped at every discovery, so installing
    /// it would tell the model a tool exists that it can never call. Refused
    /// before anything is compiled or written, so no tools directory is
    /// touched here.
    #[tokio::test]
    async fn build_agent_reserves_mcp_tool_names_for_install_tool() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(
            &manifest,
            "[agent]\nname = \"rs\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n\
             [stages.main]\nmodel = { provider = \"anthropic\", model = \"m\" }\n",
        )
        .unwrap();
        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        let mcp_defs = vec![Tool {
            name: "acme_search".to_string(),
            description: "d".to_string(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }];
        let entity = build_agent(
            world.world_mut(),
            SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &mcp_defs,
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 100,
                subagent_tx: sub_tx(),
            },
            &spawn_args(&manifest.to_string_lossy()),
        )
        .expect("spawn succeeds");
        let state = cli.take(entity).expect("state registered");
        let out = state
            .builtins
            .execute(
                "install_tool",
                serde_json::json!({
                    "name": "acme_search",
                    "source": "// @tool acme_search\n// @description d\n1\n",
                }),
            )
            .await;
        assert!(
            out.contains("'acme_search' is the name of a built-in tool"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn build_agent_errors_when_sandbox_runtime_unavailable() {
        // A container sandbox naming a nonexistent engine fails to start on every
        // platform (no runtime needed), so build_agent surfaces the error - this
        // covers the `?` on `SandboxManager::build` uniformly across OSes,
        // independent of which container runtimes happen to be installed.
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(
            &manifest,
            "[agent]\nname = \"sb\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n\
             [sandbox]\nkind = \"container\"\nimage = \"x\"\nengine = \"leviath-no-such-engine\"\n\n\
             [stages.main]\nmodel = { provider = \"anthropic\", model = \"m\" }\n",
        )
        .unwrap();
        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        let err = build_agent(
            world.world_mut(),
            SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 100,
                subagent_tx: sub_tx(),
            },
            &spawn_args(&manifest.to_string_lossy()),
        )
        .expect_err("a nonexistent engine can't start the container");
        assert!(err.contains("sandbox unavailable"), "got: {err}");
    }

    #[tokio::test]
    async fn build_agent_yolo_attaches_gate_auto_approve_when_taint_on() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(
            &manifest,
            "[agent]\nname = \"sec\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n\
             [security]\ntaint_tracking = true\n\n\
             [stages.main]\nmodel = { provider = \"anthropic\", model = \"m\" }\n",
        )
        .unwrap();
        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        let mut args = spawn_args(&manifest.to_string_lossy());
        args.yolo = true;
        let entity = build_agent(
            world.world_mut(),
            SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 100,
                subagent_tx: sub_tx(),
            },
            &args,
        )
        .expect("spawn succeeds");
        // Taint on + `--yolo` ⇒ gate is auto-approved (marker attached) so a
        // headless run never blocks on a gate prompt.
        assert!(
            world
                .world()
                .get::<leviath_runtime::components::GateAutoApprove>(entity)
                .is_some()
        );
        // ...and likewise for the blueprint's own stage-boundary checkpoints and
        // the agent's `ask_user_*` tools: unattended means unattended.
        assert!(
            world
                .world()
                .get::<leviath_runtime::components::InteractionAutoApprove>(entity)
                .is_some()
        );
        assert!(cli.take(entity).expect("tool state registered").unattended);
        // Recorded on the agent, so the sub-agent and fan-out spawners can pass
        // it down and `meta.json` can carry it across a restart.
        assert!(
            world
                .world()
                .get::<RunMetadata>(entity)
                .expect("run metadata attached")
                .unattended
        );
    }

    /// The status a `--yolo` run reports is `active`, not `waiting`: nothing
    /// should be opening a prompt for it in the first place.
    #[tokio::test]
    async fn build_agent_yolo_leaves_the_run_active_and_unattended() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(
            &manifest,
            "[agent]\nname = \"a\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n\
             [stages.main]\nmodel = { provider = \"anthropic\", model = \"m\" }\n",
        )
        .unwrap();
        let (mut world, cli) = test_world();
        let mut args = spawn_args(&manifest.to_string_lossy());
        args.yolo = true;
        let entity = build_agent(
            world.world_mut(),
            SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &InteractionHub::new(),
                now_secs: 100,
                subagent_tx: sub_tx(),
            },
            &args,
        )
        .expect("spawn succeeds");

        assert_eq!(
            world.agent_status(world.own_agent(entity)),
            Some(AgentStatus::Active)
        );
        let meta = world
            .world()
            .get::<RunMetadata>(entity)
            .expect("run metadata attached");
        assert!(meta.unattended);
    }

    /// A stage that kept a human tool through an unattended run has to reach the
    /// tool state with that tool in hand: the cut takes it out of the advertised
    /// set, and this set is what puts a call to it back in front of a person
    /// instead of the auto-answering backend.
    /// A validator that will not compile stops the spawn, before any tokens are
    /// spent. The only other time the script is read is at the end of the run,
    /// which is the worst possible moment to learn the agent cannot hand back
    /// its work.
    #[tokio::test]
    async fn build_agent_refuses_a_validator_that_does_not_compile() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(dir.path().join("shape.rhai"), "fn validate(a, b) { () }").unwrap();
        std::fs::write(
            &manifest,
            "[agent]\nname = \"v\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n\
             [stages.main]\nmodel = { provider = \"anthropic\", model = \"m\" }\n\
             available_tools = [\"submit_output\"]\n\n\
             [stages.main.output]\nformat = \"a2ui\"\nvalidator = \"shape.rhai\"\n",
        )
        .unwrap();

        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        let args = spawn_args(&manifest.to_string_lossy());
        let err = build_agent(
            world.world_mut(),
            SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 100,
                subagent_tx: sub_tx(),
            },
            &args,
        )
        .unwrap_err();

        assert!(err.contains("failed to compile"), "got: {err}");
        assert!(err.contains("exactly one parameter"), "and says why: {err}");
    }

    /// Compiling a validator at spawn is only half of it: it has to reach the
    /// entity, or the script is checked and then never runs, and the run hands
    /// back an answer nothing looked at.
    #[tokio::test]
    async fn build_agent_carries_output_validators_onto_the_entity() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(dir.path().join("shape.rhai"), "fn validate(content) { () }").unwrap();
        std::fs::write(
            &manifest,
            "[agent]\nname = \"v\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n\
             [stages.main]\nmodel = { provider = \"anthropic\", model = \"m\" }\n\
             available_tools = [\"submit_output\"]\n\n\
             [stages.main.output]\nformat = \"a2ui\"\nvalidator = \"shape.rhai\"\n",
        )
        .unwrap();

        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        let args = spawn_args(&manifest.to_string_lossy());
        let entity = build_agent(
            world.world_mut(),
            SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 100,
                subagent_tx: sub_tx(),
            },
            &args,
        )
        .expect("spawns");

        let validators = world
            .world()
            .get::<leviath_runtime::components::OutputValidators>(entity)
            .expect("the compiled validator reaches the entity");
        assert!(validators.compiled.contains_key("shape.rhai"));
    }

    /// And an agent that names none carries none, rather than an empty
    /// component every consumer then has to check.
    #[tokio::test]
    async fn build_agent_carries_no_validators_when_none_are_named() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(
            &manifest,
            "[agent]\nname = \"v\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n\
             [stages.main]\nmodel = { provider = \"anthropic\", model = \"m\" }\n",
        )
        .unwrap();

        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        let args = spawn_args(&manifest.to_string_lossy());
        let entity = build_agent(
            world.world_mut(),
            SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 100,
                subagent_tx: sub_tx(),
            },
            &args,
        )
        .expect("spawns");

        assert!(
            world
                .world()
                .get::<leviath_runtime::components::OutputValidators>(entity)
                .is_none()
        );
    }

    #[tokio::test]
    async fn build_agent_carries_required_tools_into_the_tool_state() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(
            &manifest,
            "[agent]\nname = \"asks\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n\
             [stages.main]\nmodel = { provider = \"anthropic\", model = \"m\" }\n\
             available_tools = [\"read_file\", \"ask_user_text\"]\n\
             required_tools = [\"ask_user_text\"]\n",
        )
        .unwrap();
        let (mut world, cli) = test_world();
        let mut args = spawn_args(&manifest.to_string_lossy());
        args.yolo = true;
        let entity = build_agent(
            world.world_mut(),
            SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &InteractionHub::new(),
                now_secs: 100,
                subagent_tx: sub_tx(),
            },
            &args,
        )
        .expect("spawn succeeds");

        let state = cli.take(entity).expect("tool state registered");
        assert!(
            state
                .stage_required
                .lock()
                .unwrap()
                .contains("ask_user_text")
        );
        assert_eq!(state.stage_required_by_index.len(), 1);
    }

    #[tokio::test]
    async fn build_agent_without_yolo_keeps_prompts_interactive() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(
            &manifest,
            "[agent]\nname = \"plain\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n\
             [stages.main]\nmodel = { provider = \"anthropic\", model = \"m\" }\n",
        )
        .unwrap();
        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        let entity = build_agent(
            world.world_mut(),
            SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 100,
                subagent_tx: sub_tx(),
            },
            &spawn_args(&manifest.to_string_lossy()),
        )
        .expect("spawn succeeds");
        assert!(
            world
                .world()
                .get::<leviath_runtime::components::InteractionAutoApprove>(entity)
                .is_none()
        );
        assert!(!cli.take(entity).expect("tool state registered").unattended);
    }

    #[tokio::test]
    async fn build_agent_no_security_block_leaves_taint_off_by_default() {
        // A blueprint with no `[security]` block and a default (taint-off)
        // global config must NOT attach the taint gate - an
        // `unwrap_or_default()` on the resolved security forces it on for
        // every agent.
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(
            &manifest,
            "[agent]\nname = \"plain\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n\
             [stages.main]\nmodel = { provider = \"anthropic\", model = \"m\" }\n",
        )
        .unwrap();
        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        let entity = build_agent(
            world.world_mut(),
            SpawnDeps {
        tool_service: cli.as_ref(),
        config: &Config::default(),
        shared_mcp: // taint_tracking defaults to false
            mcp,
        mcp_tool_defs: &[],
        mcp_tool_owners: &Default::default(),
        hub: &hub,
        now_secs: 100,
        subagent_tx: sub_tx(),
    },
            &spawn_args(&manifest.to_string_lossy()),
        )
        .expect("spawn succeeds");
        assert!(
            world
                .world()
                .get::<leviath_runtime::TaintGate>(entity)
                .is_none(),
            "no [security] block + global off ⇒ no taint gate"
        );
    }

    /// The `no_output_tools` a freshly built agent carries.
    async fn spawned_no_output_tools(manifest_body: &str) -> bool {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(&manifest, manifest_body).unwrap();
        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        let entity = build_agent(
            world.world_mut(),
            SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 100,
                subagent_tx: sub_tx(),
            },
            &spawn_args(&manifest.to_string_lossy()),
        )
        .expect("spawn succeeds");
        world
            .world()
            .get::<leviath_runtime::persistence::RunOutcomeFlags>(entity)
            .expect("build_agent attaches run outcome flags")
            .0
            .no_output_tools
    }

    #[tokio::test]
    async fn build_agent_records_whether_the_blueprint_can_write_at_all() {
        // A coding agent writes in `implement`, so silence from it is worth
        // reporting.
        assert!(!spawned_no_output_tools(&coder_manifest()).await);
        // A router-shaped agent delegates and never writes. Reporting it as
        // having "modified nothing" is an accusation the framework has no
        // grounds for.
        assert!(
            spawned_no_output_tools(
                "[agent]\nname = \"router\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n\
                 [stages.triage]\nmodel = { provider = \"anthropic\", model = \"m\" }\n\
                 available_tools = [\"read_file\", \"spawn_agent\"]\n",
            )
            .await
        );
    }

    #[tokio::test]
    async fn build_agent_spawns_registers_and_wires_tools() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(&manifest, coder_manifest()).unwrap();

        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        let entity = build_agent(
            world.world_mut(),
            SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 100,
                subagent_tx: sub_tx(),
            },
            &spawn_args(&manifest.to_string_lossy()),
        )
        .expect("spawn succeeds");

        assert_eq!(
            world.agent_status(world.own_agent(entity)),
            Some(AgentStatus::Active)
        );
        // The run metadata was attached.
        let md = world
            .world()
            .get::<RunMetadata>(entity)
            .expect("run metadata");
        assert_eq!(md.run_id, "run-x");
        assert_eq!(md.agent_name, "coder");
        // Tool state was registered: a tool batch dispatches (not "no tool state").
        let out = leviath_runtime::pipeline::ToolService::exec_for(
            cli.as_ref(),
            entity,
            vec![leviath_providers::ToolCall {
                id: "c1".to_string(),
                name: "list_dir".to_string(),
                arguments: serde_json::json!({"path": "."}),
                thought_signature: None,
            }],
            leviath_runtime::pipeline::noop_progress(),
        )()
        .await;
        assert_eq!(out[0].0, "c1");
        assert!(!out[0].1.contains("no tool state"));
    }

    #[tokio::test]
    async fn build_agent_tags_dynamic_tools_agent() {
        // A blueprint opting into dynamic_tools gets the DynamicTools marker so the
        // runtime polls it for mid-run re-scans; the agent's tool state carries the
        // re-resolution context (exercised via refresh_tools).
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(
            &manifest,
            coder_manifest().replace("[agent]", "[agent]\ndynamic_tools = true"),
        )
        .unwrap();

        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        let entity = build_agent(
            world.world_mut(),
            SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 100,
                subagent_tx: sub_tx(),
            },
            &spawn_args(&manifest.to_string_lossy()),
        )
        .expect("spawn succeeds");

        assert!(
            world
                .world()
                .get::<leviath_runtime::pipeline::DynamicTools>(entity)
                .is_some(),
            "dynamic_tools agent must carry the DynamicTools marker"
        );
        // The dynamic context is wired: refresh_tools returns Some for stage 0.
        assert!(
            leviath_runtime::pipeline::ToolService::refresh_tools(cli.as_ref(), entity, 0)
                .is_some()
        );
    }

    #[tokio::test]
    async fn build_agent_applies_yolo_allow_and_max_depth() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(&manifest, coder_manifest()).unwrap();

        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        // The user's config denies read_file. Neither `--yolo` nor an explicit
        // `--allow read_file` lifts that: a deny rule is a decision, and skipping
        // *prompts* is all `--yolo` is for.
        let config = Config {
            tool_permissions: HashMap::from([(
                "read_file".to_string(),
                crate::config::ToolPolicy::Deny,
            )]),
            ..Default::default()
        };
        let mut args = spawn_args(&manifest.to_string_lossy());
        args.yolo = true;
        args.allow = vec!["read_file".to_string()];
        args.max_depth = Some(7);

        let entity = build_agent(
            world.world_mut(),
            SpawnDeps {
                tool_service: cli.as_ref(),
                config: &config,
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 100,
                subagent_tx: sub_tx(),
            },
            &args,
        )
        .expect("spawn succeeds");
        assert_eq!(
            world.agent_status(world.own_agent(entity)),
            Some(AgentStatus::Active)
        );

        // The config deny stands: read_file is refused, not executed.
        let out = leviath_runtime::pipeline::ToolService::exec_for(
            cli.as_ref(),
            entity,
            vec![leviath_providers::ToolCall {
                id: "c1".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "/no/such/file"}),
                thought_signature: None,
            }],
            leviath_runtime::pipeline::noop_progress(),
        )()
        .await;
        let result = out[0].1.clone();
        assert!(
            result.contains("[denied]"),
            "a configured deny must survive --yolo, got: {result}"
        );

        // `--yolo` still does its job for a tool the config did not deny:
        // `list_dir` runs unattended with no approval prompt.
        let out = leviath_runtime::pipeline::ToolService::exec_for(
            cli.as_ref(),
            entity,
            vec![leviath_providers::ToolCall {
                id: "c2".to_string(),
                name: "list_dir".to_string(),
                arguments: serde_json::json!({"path": "."}),
                thought_signature: None,
            }],
            leviath_runtime::pipeline::noop_progress(),
        )()
        .await;
        let result = out[0].1.clone();
        assert!(
            !result.contains("[denied]"),
            "--yolo must still waive approval where nothing denies, got: {result}"
        );
    }

    #[tokio::test]
    async fn build_agent_honors_agent_level_tool_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        // A top-level `[tool_permissions]` block denying a builtin - no stage
        // perms, no launch overrides, no global config deny. Only the agent-level
        // layer can produce the deny, so this proves it is wired through.
        std::fs::write(
            &manifest,
            "[agent]\nname = \"perm\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n\
             [tool_permissions]\nread_file = \"deny\"\n\n\
             [stages.main]\nmodel = { provider = \"anthropic\", model = \"m\" }\n",
        )
        .unwrap();

        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        let entity = build_agent(
            world.world_mut(),
            SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 100,
                subagent_tx: sub_tx(),
            },
            &spawn_args(&manifest.to_string_lossy()),
        )
        .expect("spawn succeeds");

        let out = leviath_runtime::pipeline::ToolService::exec_for(
            cli.as_ref(),
            entity,
            vec![leviath_providers::ToolCall {
                id: "c1".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "/no/such/file"}),
                thought_signature: None,
            }],
            leviath_runtime::pipeline::noop_progress(),
        )()
        .await;
        assert!(
            out[0].1.contains("[denied]"),
            "agent-level deny should block read_file"
        );
    }

    #[tokio::test]
    async fn build_agent_script_host_honors_agent_level_grants() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(
            &manifest,
            "[agent]\nname = \"scriptperm\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n\
             [stages.main]\nmodel = { provider = \"anthropic\", model = \"m\" }\n",
        )
        .unwrap();

        // `write_file` defaults to Ask, and a script-permission `Inherit`
        // permits the host function only on a hard Allow. The grant below
        // lives solely in the user's per-agent block, so the script host can
        // only see it through the agent-scoped ceiling - the raw global
        // `[tool_permissions]` map is empty here.
        let mut config = Config::default();
        config.agent_tool_permissions.insert(
            "scriptperm".to_string(),
            HashMap::from([("write_file".to_string(), crate::config::ToolPolicy::Allow)]),
        );

        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        let mut args = spawn_args(&manifest.to_string_lossy());
        args.workdir = dir.path().to_string_lossy().to_string();
        let entity = build_agent(
            world.world_mut(),
            SpawnDeps {
                tool_service: cli.as_ref(),
                config: &config,
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 100,
                subagent_tx: sub_tx(),
            },
            &args,
        )
        .expect("spawn succeeds");

        let state = cli.take(entity).expect("tool state registered at spawn");
        state
            .script_host
            .write_file("granted.txt", "ok")
            .expect("agent-level write_file grant must reach the script host");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("granted.txt")).unwrap(),
            "ok"
        );
    }

    #[tokio::test]
    async fn build_agent_applies_default_max_iterations_only_when_stage_omits_it() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        // Two stages: one omits max_iterations, one sets it explicitly to 3.
        std::fs::write(
            &manifest,
            "[agent]\nname = \"iters\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n\
             [stages.main]\nmodel = { provider = \"anthropic\", model = \"m\" }\n\n\
             [stages.capped]\nmax_iterations = 3\n\
             model = { provider = \"anthropic\", model = \"m\" }\n",
        )
        .unwrap();

        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        // A non-default cap so the assertion can't accidentally match the built-in.
        let config = Config {
            limits: crate::config::LimitsConfig {
                default_max_iterations: Some(42),
                ..Default::default()
            },
            ..Default::default()
        };
        let entity = build_agent(
            world.world_mut(),
            SpawnDeps {
                tool_service: cli.as_ref(),
                config: &config,
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 100,
                subagent_tx: sub_tx(),
            },
            &spawn_args(&manifest.to_string_lossy()),
        )
        .expect("spawn succeeds");

        let bp = world
            .world()
            .get::<leviath_runtime::pipeline::AgentBlueprint>(entity)
            .expect("blueprint");
        let by_name = |n: &str| {
            bp.0.stages
                .iter()
                .find(|s| s.name == n)
                .unwrap()
                .max_iterations
        };
        // The stage that omitted it inherits the config default …
        assert_eq!(by_name("main"), Some(42));
        // … while an explicit per-stage cap is left untouched.
        assert_eq!(by_name("capped"), Some(3));
    }

    #[tokio::test]
    async fn build_agent_leaves_max_iterations_unset_when_config_default_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(
            &manifest,
            "[agent]\nname = \"nolimit\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n\
             [stages.main]\nmodel = { provider = \"anthropic\", model = \"m\" }\n",
        )
        .unwrap();

        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        // `None` disables the config default entirely - the stage stays uncapped.
        let config = Config {
            limits: crate::config::LimitsConfig {
                default_max_iterations: None,
                ..Default::default()
            },
            ..Default::default()
        };
        let entity = build_agent(
            world.world_mut(),
            SpawnDeps {
                tool_service: cli.as_ref(),
                config: &config,
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 100,
                subagent_tx: sub_tx(),
            },
            &spawn_args(&manifest.to_string_lossy()),
        )
        .expect("spawn succeeds");

        let bp = world
            .world()
            .get::<leviath_runtime::pipeline::AgentBlueprint>(entity)
            .expect("blueprint");
        assert_eq!(bp.0.stages[0].max_iterations, None);
    }

    #[tokio::test]
    async fn fake_provider_methods_are_exercised() {
        let p = fake_provider();
        assert_eq!(p.name(), "fake");
        assert_eq!(p.count_tokens("t", "m").await, 1);
        assert_eq!(p.max_context_tokens("m"), 1000);
        let _ = p.capabilities("m");
        assert!(p.infer(&fixtures::inference_request()).await.is_err());
    }

    // ── [read_paths] policy resolution ────────────────────────────────────

    use std::path::Path;

    fn blueprint_declaring(read_paths: &[&str]) -> Blueprint {
        let stage = leviath_core::Stage::new("s".to_string(), model_cfg(vec![("anthropic", "m")]));
        let layout = leviath_core::layout::ContextLayout::new(vec![], 1000);
        let mut bp = Blueprint::new("cto".to_string(), "d".to_string(), vec![stage], layout);
        if !read_paths.is_empty() {
            bp.read_paths = Some(leviath_core::ReadPathsConfig {
                allow: read_paths.iter().map(|s| s.to_string()).collect(),
            });
        }
        bp
    }

    /// The counts recorded on the run for `lev ps`. A blueprint that declares
    /// nothing has nothing to count, and a grant list that will not compile is
    /// a hard spawn error a line earlier - neither leaves a half-answer behind.
    #[test]
    fn read_path_grant_counts_are_recorded_for_a_declaring_blueprint() {
        let bp = blueprint_declaring(&["/data/runs", "/data/docs"]);
        let mut config = Config::default();
        config.security.read_paths = vec!["/data/runs".to_string()];
        let counts = read_path_grant_counts(&bp, &config, Path::new("/w")).expect("declares paths");
        assert_eq!(counts.declared, 2);
        assert_eq!(counts.granted, 1);

        assert!(
            read_path_grant_counts(&blueprint_declaring(&[]), &config, Path::new("/w")).is_none()
        );

        let mut broken = Config::default();
        broken.security.read_paths = vec!["regex:relative/.*".to_string()];
        assert!(read_path_grant_counts(&bp, &broken, Path::new("/w")).is_none());
    }

    #[test]
    fn read_path_policy_is_inactive_without_declarations() {
        let bp = blueprint_declaring(&[]);
        let (policy, warning) =
            build_read_path_policy(&bp, &Config::default(), Path::new("/w")).unwrap();
        assert!(!policy.is_active());
        assert!(warning.is_none());

        // An explicitly empty `[read_paths]` block is the same as none.
        let mut bp = blueprint_declaring(&[]);
        bp.read_paths = Some(leviath_core::ReadPathsConfig { allow: vec![] });
        let (policy, warning) =
            build_read_path_policy(&bp, &Config::default(), Path::new("/w")).unwrap();
        assert!(!policy.is_active());
        assert!(warning.is_none());
    }

    /// Declared but ungranted: the agent still spawns, and the warning names
    /// the agent and shows both config stanzas that would grant the paths.
    #[test]
    fn read_path_policy_warns_when_nothing_grants() {
        let bp = blueprint_declaring(&["/data/runs", "glob:/data/docs/**"]);
        let (policy, warning) =
            build_read_path_policy(&bp, &Config::default(), Path::new("/w")).unwrap();
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
        let (policy, warning) = build_read_path_policy(&bp, &config, Path::new("/w")).unwrap();
        assert!(policy.is_active());
        assert!(!policy.grants.is_empty());
        assert!(warning.is_none());
    }

    #[test]
    fn read_path_policy_is_quiet_under_the_override() {
        let bp = blueprint_declaring(&["/data/runs"]);
        let mut config = Config::default();
        config.security.allow_blueprint_read_paths = true;
        let (policy, warning) = build_read_path_policy(&bp, &config, Path::new("/w")).unwrap();
        assert!(policy.allow_blueprint);
        assert!(warning.is_none());
    }

    /// A malformed entry is a hard spawn error naming its source - the
    /// blueprint's section or the user's own grant list.
    #[test]
    fn read_path_policy_rejects_bad_entries_loudly() {
        let bp = blueprint_declaring(&["glob:["]);
        let err = build_read_path_policy(&bp, &Config::default(), Path::new("/w")).unwrap_err();
        assert!(err.contains("agent 'cto' [read_paths]"), "{err}");

        let bp = blueprint_declaring(&["/data/runs"]);
        let mut config = Config::default();
        config.security.read_paths = vec!["regex:(".to_string()];
        let err = build_read_path_policy(&bp, &config, Path::new("/w")).unwrap_err();
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

    #[tokio::test]
    async fn build_agent_read_error() {
        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        let err = build_agent(
            world.world_mut(),
            SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 100,
                subagent_tx: sub_tx(),
            },
            &spawn_args("/no/such/manifest.leviath"),
        )
        .unwrap_err();
        assert!(err.contains("read manifest"));
    }

    /// A minimal single-stage manifest with a tiny task region and a `system_prompt`
    /// large enough to overflow it, so stage-0 setup fails in `spawn_agent`.
    const OVERSIZED_MANIFEST: &str = r#"
[agent]
name = "tiny"
version = "0.1.0"
description = "d"
entry_stage = "main"

[context.regions]
task = { kind = "pinned", max_tokens = 20 }

[stages.main]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "m" }] }
description = "d"
available_tools = []
system_prompt = "SYSTEM_PROMPT_PLACEHOLDER"
"#;

    #[tokio::test]
    async fn build_agent_propagates_spawn_error() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("tiny.leviath");
        // A huge prompt that cannot fit the 20-token "task" region.
        let content = OVERSIZED_MANIFEST.replace("SYSTEM_PROMPT_PLACEHOLDER", &"x ".repeat(5000));
        std::fs::write(&manifest, content).unwrap();

        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        let result = build_agent(
            world.world_mut(),
            SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 100,
                subagent_tx: sub_tx(),
            },
            &spawn_args(&manifest.to_string_lossy()),
        );
        assert!(result.is_err(), "expected spawn error, got {result:?}");
    }

    #[tokio::test]
    async fn build_agent_refuses_a_manifest_with_no_usable_provider() {
        // End to end: without this an agent is built pointed at a provider
        // nothing answers to, and then sits at iteration 0 for the life of the
        // daemon.
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("ghostly.leviath");
        std::fs::write(
            &manifest,
            r#"
[agent]
name = "ghostly"
version = "0.1.0"
description = "d"
entry_stage = "main"

[context.regions]
task = { kind = "pinned", max_tokens = 4000 }

[stages.main]
mode = "autonomous"
model = { models = [{ provider = "ghost", model = "m" }], allow_user_default = false }
description = "d"
available_tools = []
"#,
        )
        .unwrap();
        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        let err = build_agent(
            world.world_mut(),
            SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 100,
                subagent_tx: sub_tx(),
            },
            &spawn_args(&manifest.to_string_lossy()),
        )
        .unwrap_err();
        assert!(err.contains("main"), "names the stage: {err}");
        assert!(err.contains("ghost"), "names what it tried: {err}");
    }

    #[tokio::test]
    async fn build_agent_invalid_blueprint() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("bad.leviath");
        // entry_stage names a stage that doesn't exist ⇒ validate() fails.
        std::fs::write(
            &manifest,
            r#"
[agent]
name = "bad"
version = "0.1.0"
description = "d"
entry_stage = "ghost"

[context.regions]
task = { kind = "pinned", max_tokens = 4000 }

[stages.main]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "m" }] }
description = "d"
available_tools = []
"#,
        )
        .unwrap();
        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        let err = build_agent(
            world.world_mut(),
            SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 100,
                subagent_tx: sub_tx(),
            },
            &spawn_args(&manifest.to_string_lossy()),
        )
        .unwrap_err();
        assert!(err.contains("invalid blueprint"));
    }

    #[tokio::test]
    async fn build_agent_without_entry_stage_and_with_compaction() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("mini.leviath");
        // No entry_stage (falls back to the first stage) + a compaction section.
        std::fs::write(
            &manifest,
            r#"
[agent]
name = "mini"
version = "0.1.0"
description = "d"

[compaction]
provider = "anthropic"
model = "claude-x"

[context.regions]
task = { kind = "pinned", max_tokens = 4000 }

[stages.main]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "m" }] }
description = "d"
available_tools = []
system_prompt = "be brief"
"#,
        )
        .unwrap();
        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        let entity = build_agent(
            world.world_mut(),
            SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 100,
                subagent_tx: sub_tx(),
            },
            &spawn_args(&manifest.to_string_lossy()),
        )
        .expect("spawn succeeds");
        assert_eq!(
            world.agent_status(world.own_agent(entity)),
            Some(AgentStatus::Active)
        );
        // Compaction settings were attached.
        assert!(world.world().get::<CompactionSettings>(entity).is_some());
    }

    /// A manifest that returns `[agent] name` and `write` a `read_paths.leviath`
    /// declaring an out-of-workdir read. Used by the wiring tests below.
    fn write_read_paths_manifest(dir: &std::path::Path, allow: &str) -> std::path::PathBuf {
        let manifest = dir.join("reader.leviath");
        std::fs::write(
            &manifest,
            format!(
                r#"
[agent]
name = "reader"
version = "0.1.0"
description = "d"

[read_paths]
allow = [{allow}]

[context.regions]
task = {{ kind = "pinned", max_tokens = 4000 }}

[stages.main]
mode = "autonomous"
model = {{ models = [{{ provider = "anthropic", model = "m" }}] }}
description = "d"
available_tools = []
system_prompt = "be brief"
"#
            ),
        )
        .unwrap();
        manifest
    }

    /// A blueprint declaring a stage hook spawns with the compiled script
    /// attached - the branch that only runs when some stage declared one.
    #[tokio::test]
    async fn build_agent_attaches_declared_stage_hooks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("h.rhai"), "fn on_stage_enter(ctx) { () }").unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(
            &manifest,
            "[agent]\nname = \"h\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n\
             [stages.main]\nmodel = { provider = \"anthropic\", model = \"m\" }\n\
             [stages.main.hooks]\non_stage_enter = \"h.rhai\"\n",
        )
        .unwrap();

        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        let entity = build_agent(
            world.world_mut(),
            SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 100,
                subagent_tx: sub_tx(),
            },
            &spawn_args(&manifest.to_string_lossy()),
        )
        .expect("spawn succeeds");

        let scripts = world
            .world_mut()
            .get::<leviath_runtime::components::StageHookScripts>(entity)
            .expect("the hook script is attached");
        assert!(scripts.0.contains_key("h.rhai"));
    }

    /// A broken hook script fails the spawn rather than the run - the `?` on
    /// the resolver, which is the whole point of resolving at spawn.
    #[tokio::test]
    async fn build_agent_refuses_a_broken_stage_hook() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("h.rhai"), "fn on_stage_enter(ctx) {").unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(
            &manifest,
            "[agent]\nname = \"h\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n\
             [stages.main]\nmodel = { provider = \"anthropic\", model = \"m\" }\n\
             [stages.main.hooks]\non_stage_enter = \"h.rhai\"\n",
        )
        .unwrap();

        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        let err = build_agent(
            world.world_mut(),
            SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 100,
                subagent_tx: sub_tx(),
            },
            &spawn_args(&manifest.to_string_lossy()),
        )
        .expect_err("a broken hook script must fail the spawn");
        assert!(err.contains("failed to compile"), "{err}");
    }

    /// A granted `[read_paths]` spawns cleanly, with taint on so the read-tool
    /// sensitivity bump path runs end to end.
    #[tokio::test]
    async fn build_agent_wires_granted_read_paths() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_read_paths_manifest(dir.path(), "\"/tmp\"");
        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        let mut config = Config::default();
        config.security.allow_blueprint_read_paths = true;
        config.taint_tracking = true;
        let entity = build_agent(
            world.world_mut(),
            SpawnDeps {
                tool_service: cli.as_ref(),
                config: &config,
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 100,
                subagent_tx: sub_tx(),
            },
            &spawn_args(&manifest.to_string_lossy()),
        )
        .expect("spawn succeeds");
        assert_eq!(
            world.agent_status(world.own_agent(entity)),
            Some(AgentStatus::Active)
        );
    }

    /// A declared-but-ungranted `[read_paths]` still spawns; the warning-logging
    /// branch fires.
    #[tokio::test]
    async fn build_agent_wires_ungranted_read_paths() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_read_paths_manifest(dir.path(), "\"/tmp\"");
        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        let entity = build_agent(
            world.world_mut(),
            SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 100,
                subagent_tx: sub_tx(),
            },
            &spawn_args(&manifest.to_string_lossy()),
        )
        .expect("spawn succeeds even when nothing grants the declaration");
        assert_eq!(
            world.agent_status(world.own_agent(entity)),
            Some(AgentStatus::Active)
        );
    }

    /// A malformed grant entry in the user's own config fails the spawn - the
    /// error propagates out of `build_read_path_policy`.
    #[tokio::test]
    async fn build_agent_rejects_a_malformed_config_grant() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_read_paths_manifest(dir.path(), "\"/tmp\"");
        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        let mut config = Config::default();
        config.security.read_paths = vec!["glob:[".to_string()];
        let err = build_agent(
            world.world_mut(),
            SpawnDeps {
                tool_service: cli.as_ref(),
                config: &config,
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 100,
                subagent_tx: sub_tx(),
            },
            &spawn_args(&manifest.to_string_lossy()),
        )
        .expect_err("a broken config grant must fail the spawn");
        assert!(err.contains("config.toml"), "{err}");
    }

    #[tokio::test]
    async fn build_agent_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("bad.leviath");
        std::fs::write(&manifest, "this is not valid toml : : :").unwrap();
        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        let err = build_agent(
            world.world_mut(),
            SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 100,
                subagent_tx: sub_tx(),
            },
            &spawn_args(&manifest.to_string_lossy()),
        )
        .unwrap_err();
        assert!(err.contains("parse manifest"));
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
        leviath_core::manifest::parse_manifest(&toml).unwrap()
    }

    fn args_with(task: &str, regions: HashMap<String, String>, workdir: &str) -> SpawnArgs {
        SpawnArgs {
            run_id: "r".to_string(),
            blueprint_path: "/bp".to_string(),
            task: task.to_string(),
            regions,
            model: None,
            workdir: workdir.to_string(),
            metadata: HashMap::new(),
            callback_url: None,
            callback_secret: None,
            yolo: false,
            no_seed_commands: false,
            allow: Vec::new(),
            max_depth: None,
            parent_run_id: None,
            output: None,
        }
    }

    /// The default policy for the non-command seed tests: command seeds off, so
    /// nothing is ever executed by a test that isn't about command seeds.
    fn seed_policy() -> SeedCommandPolicy {
        SeedCommandPolicy::disabled()
    }

    /// Pre-approves the command the seed fixtures declare, so these tests
    /// exercise the runner arms rather than the pre-approval refusal (which
    /// `seed_command.rs` covers directly).
    fn seed_safe_keys() -> std::sync::Arc<std::collections::HashSet<String>> {
        std::sync::Arc::new(
            ["shell:scan-repo".to_string()]
                .into_iter()
                .collect::<std::collections::HashSet<_>>(),
        )
    }

    /// A blueprint that declares no `[read_paths]`, which is the normal case
    /// and the one where seed paths are confined to the workdir outright.
    /// A tool-seed policy that runs nothing, for the seed tests that are about
    /// the other seed kinds. A `{ tools = [...] }` seed under it fails every
    /// call, which is the same shape as a tool that is not installed.
    fn no_seed_tools() -> crate::daemon::seed_tool::SeedToolPolicy {
        crate::daemon::seed_tool::SeedToolPolicy::disabled()
    }

    fn no_read_paths() -> leviath_core::ReadPathPolicy {
        leviath_core::ReadPathPolicy {
            agent: "a".to_string(),
            blueprint: Default::default(),
            grants: Default::default(),
            allow_blueprint: false,
        }
    }

    /// A policy whose runner is a stub returning `result`, for the command-seed
    /// arms (no real process, deterministic on every platform).
    fn stub_policy(result: Result<String, String>) -> SeedCommandPolicy {
        SeedCommandPolicy {
            allowed: true,
            timeout: std::time::Duration::from_secs(1),
            safe_keys: seed_safe_keys(),
            runner: std::sync::Arc::new(move |_, _, _| result.clone()),
        }
    }

    #[test]
    fn resolve_seeds_fills_task_and_caller_input() {
        let bp = bp(
            r#"task = { kind = "pinned", max_tokens = 4000, seed = "task_input" }
criteria = { kind = "pinned", max_tokens = 2000, seed = "input" }"#,
        );
        let args = args_with(
            "build it",
            HashMap::from([("criteria".to_string(), "be safe".to_string())]),
            "/tmp",
        );
        let seeds = resolve_seeds(
            &bp,
            &args,
            "/tmp",
            &seed_policy(),
            &no_seed_tools(),
            &no_read_paths(),
        )
        .unwrap();
        assert_eq!(seeds.get("task").map(String::as_str), Some("build it"));
        assert_eq!(seeds.get("criteria").map(String::as_str), Some("be safe"));
    }

    #[test]
    fn resolve_seeds_required_caller_input_missing_is_error() {
        let bp =
            bp(r#"spec = { kind = "pinned", max_tokens = 2000, seed = "input", required = true }"#);
        let args = args_with("t", HashMap::new(), "/tmp");
        let err = resolve_seeds(
            &bp,
            &args,
            "/tmp",
            &seed_policy(),
            &no_seed_tools(),
            &no_read_paths(),
        )
        .unwrap_err();
        assert!(err.contains("spec"), "got: {err}");
    }

    #[test]
    fn resolve_seeds_optional_caller_input_missing_is_omitted() {
        let bp = bp(r#"notes = { kind = "pinned", max_tokens = 2000, seed = "input" }"#);
        let args = args_with("t", HashMap::new(), "/tmp");
        let seeds = resolve_seeds(
            &bp,
            &args,
            "/tmp",
            &seed_policy(),
            &no_seed_tools(),
            &no_read_paths(),
        )
        .unwrap();
        assert!(!seeds.contains_key("notes"));
    }

    #[test]
    fn resolve_seeds_literal_and_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha").unwrap();
        std::fs::write(dir.path().join("b.txt"), "beta").unwrap();
        let bp = bp(
            r#"lit = { kind = "pinned", max_tokens = 500, seed = { literal = "hello" } }
docs = { kind = "pinned", max_tokens = 2000, seed = { files = ["a.txt", "b.txt"] } }"#,
        );
        let args = args_with("t", HashMap::new(), &dir.path().to_string_lossy());
        let seeds = resolve_seeds(
            &bp,
            &args,
            &dir.path().to_string_lossy(),
            &seed_policy(),
            &no_seed_tools(),
            &no_read_paths(),
        )
        .unwrap();
        assert_eq!(seeds.get("lit").map(String::as_str), Some("hello"));
        let docs = seeds.get("docs").unwrap();
        assert!(docs.contains("alpha") && docs.contains("beta"));
    }

    /// A seed file is read into the prompt, so it is held to the same size cap
    /// a script's I/O is: uncapped, a multi-megabyte file arrives whole.
    #[test]
    fn resolve_seeds_caps_an_oversized_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("big.txt"), vec![b'x'; 900_001]).unwrap();
        let bp =
            bp(r#"docs = { kind = "pinned", max_tokens = 2000, seed = { files = ["big.txt"] } }"#);
        let wd = dir.path().to_string_lossy().to_string();
        let args = args_with("t", HashMap::new(), &wd);
        let seeds = resolve_seeds(
            &bp,
            &args,
            &wd,
            &seed_policy(),
            &no_seed_tools(),
            &no_read_paths(),
        )
        .unwrap();
        let docs = seeds.get("docs").unwrap();
        let len = docs.len();
        assert!(len < 900_200, "{len} bytes reached the prompt");
        assert!(docs.contains("truncated by leviath"));
    }

    #[test]
    fn resolve_seeds_glob_concatenates_matches() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("specs")).unwrap();
        std::fs::write(dir.path().join("specs/one.md"), "spec one").unwrap();
        std::fs::write(dir.path().join("specs/two.md"), "spec two").unwrap();
        let bp =
            bp(r#"specs = { kind = "pinned", max_tokens = 4000, seed = { glob = "specs/*.md" } }"#);
        let wd = dir.path().to_string_lossy().to_string();
        let args = args_with("t", HashMap::new(), &wd);
        let seeds = resolve_seeds(
            &bp,
            &args,
            &wd,
            &seed_policy(),
            &no_seed_tools(),
            &no_read_paths(),
        )
        .unwrap();
        let specs = seeds.get("specs").unwrap();
        assert!(specs.contains("spec one") && specs.contains("spec two"));
    }

    #[test]
    fn resolve_seeds_rhai_runs_script() {
        let dir = tempfile::tempdir().unwrap();
        // A script that returns the task text uppercased-ish via concatenation.
        std::fs::write(
            dir.path().join("init.rhai"),
            r#""seeded: " + input["task"]"#,
        )
        .unwrap();
        let bp = bp(
            r#"scripted = { kind = "pinned", max_tokens = 500, seed = { rhai = "init.rhai" } }"#,
        );
        let wd = dir.path().to_string_lossy().to_string();
        let args = args_with("hello", HashMap::new(), &wd);
        let seeds = resolve_seeds(
            &bp,
            &args,
            &wd,
            &seed_policy(),
            &no_seed_tools(),
            &no_read_paths(),
        )
        .unwrap();
        assert_eq!(
            seeds.get("scripted").map(String::as_str),
            Some("seeded: hello")
        );
    }

    #[test]
    fn resolve_seeds_files_required_missing_errors_optional_skips() {
        let dir = tempfile::tempdir().unwrap();
        let wd = dir.path().to_string_lossy().to_string();
        // Required + a missing file → error.
        let req = bp(
            r#"docs = { kind = "pinned", max_tokens = 2000, seed = { files = ["missing.txt"] }, required = true }"#,
        );
        let args = args_with("t", HashMap::new(), &wd);
        let err = resolve_seeds(
            &req,
            &args,
            &wd,
            &seed_policy(),
            &no_seed_tools(),
            &no_read_paths(),
        )
        .unwrap_err();
        assert!(err.contains("missing.txt"), "got: {err}");
        // Optional + a missing file → the region is simply omitted.
        let opt = bp(
            r#"docs = { kind = "pinned", max_tokens = 2000, seed = { files = ["missing.txt"] } }"#,
        );
        let seeds = resolve_seeds(
            &opt,
            &args,
            &wd,
            &seed_policy(),
            &no_seed_tools(),
            &no_read_paths(),
        )
        .unwrap();
        assert!(!seeds.contains_key("docs"));
    }

    #[test]
    fn resolve_seeds_glob_no_match_required_errors_optional_skips() {
        let dir = tempfile::tempdir().unwrap();
        let wd = dir.path().to_string_lossy().to_string();
        let args = args_with("t", HashMap::new(), &wd);
        // Required glob with no matches → error.
        let req = bp(
            r#"specs = { kind = "pinned", max_tokens = 2000, seed = { glob = "none/*.md" }, required = true }"#,
        );
        let err = resolve_seeds(
            &req,
            &args,
            &wd,
            &seed_policy(),
            &no_seed_tools(),
            &no_read_paths(),
        )
        .unwrap_err();
        assert!(err.contains("matched no files"), "got: {err}");
        // Optional glob with no matches → region omitted.
        let opt =
            bp(r#"specs = { kind = "pinned", max_tokens = 2000, seed = { glob = "none/*.md" } }"#);
        let seeds = resolve_seeds(
            &opt,
            &args,
            &wd,
            &seed_policy(),
            &no_seed_tools(),
            &no_read_paths(),
        )
        .unwrap();
        assert!(!seeds.contains_key("specs"));
    }

    #[test]
    fn resolve_seeds_bad_glob_pattern_errors() {
        // An unclosed `[` is an invalid glob pattern → `glob::glob` returns Err.
        let dir = tempfile::tempdir().unwrap();
        let wd = dir.path().to_string_lossy().to_string();
        let bp = bp(r#"specs = { kind = "pinned", max_tokens = 2000, seed = { glob = "[" } }"#);
        let args = args_with("t", HashMap::new(), &wd);
        let err = resolve_seeds(
            &bp,
            &args,
            &wd,
            &seed_policy(),
            &no_seed_tools(),
            &no_read_paths(),
        )
        .unwrap_err();
        assert!(err.contains("bad glob"), "got: {err}");
    }

    #[test]
    fn resolve_seeds_rhai_script_error() {
        let dir = tempfile::tempdir().unwrap();
        // A script that calls an undefined function → runtime error.
        std::fs::write(dir.path().join("boom.rhai"), "undefined_func()").unwrap();
        let wd = dir.path().to_string_lossy().to_string();
        let bp = bp(
            r#"scripted = { kind = "pinned", max_tokens = 500, seed = { rhai = "boom.rhai" } }"#,
        );
        let args = args_with("t", HashMap::new(), &wd);
        let err = resolve_seeds(
            &bp,
            &args,
            &wd,
            &seed_policy(),
            &no_seed_tools(),
            &no_read_paths(),
        )
        .unwrap_err();
        assert!(err.contains("rhai seed failed"), "got: {err}");
    }

    // ─── command seeds ───────────────────────────────────────────────────────

    // ── tool seeds ────────────────────────────────────────────────────────

    /// A tool-seed policy whose runner answers from a table, so a test can say
    /// what each tool returns without a live MCP connection or a Rhai engine.
    fn stub_tools(
        answers: Vec<(&'static str, Result<String, String>)>,
    ) -> crate::daemon::seed_tool::SeedToolPolicy {
        let map: HashMap<String, Result<String, String>> = answers
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        crate::daemon::seed_tool::SeedToolPolicy::new(std::sync::Arc::new(move |name, _| {
            map.get(name)
                .cloned()
                .unwrap_or_else(|| Err(format!("no such tool '{name}'")))
        }))
    }

    fn tool_bp(seed: &str, required: bool) -> leviath_core::Blueprint {
        let req = if required { ", required = true" } else { "" };
        bp(&format!(
            r#"environment = {{ kind = "pinned", max_tokens = 500, seed = {seed}{req} }}"#
        ))
    }

    /// Several tools write into one region, in the order the blueprint listed
    /// them, each under a heading naming the tool that produced it. The heading
    /// matters: without it the model reads one undifferentiated blob.
    #[test]
    fn a_tool_seed_writes_every_call_into_one_region_in_order() {
        let bp = tool_bp(r#"{ tools = ["current_time", "system_info"] }"#, false);
        let args = args_with("t", HashMap::new(), "/tmp");
        let seeds = resolve_seeds(
            &bp,
            &args,
            "/tmp",
            &seed_policy(),
            &stub_tools(vec![
                ("current_time", Ok("{\"date\": \"2026-08-18\"}".to_string())),
                ("system_info", Ok("{\"os\": \"macos\"}".to_string())),
            ]),
            &no_read_paths(),
        )
        .unwrap();
        assert_eq!(
            seeds.get("environment").map(String::as_str),
            Some(
                "--- current_time ---\n{\"date\": \"2026-08-18\"}\n\n\
                 --- system_info ---\n{\"os\": \"macos\"}"
            )
        );
    }

    /// One tool being unavailable must not cost the others their output. A
    /// region seeded from three sources where one is missing is still worth
    /// two thirds of what it was for.
    #[test]
    fn a_failed_call_is_skipped_and_the_rest_still_seed() {
        let bp = tool_bp(
            r#"{ tools = ["current_time", "acme__missing", "locale_info"] }"#,
            false,
        );
        let args = args_with("t", HashMap::new(), "/tmp");
        let seeds = resolve_seeds(
            &bp,
            &args,
            "/tmp",
            &seed_policy(),
            &stub_tools(vec![
                ("current_time", Ok("now".to_string())),
                ("locale_info", Ok("en-US".to_string())),
            ]),
            &no_read_paths(),
        )
        .unwrap();
        let content = seeds.get("environment").expect("still seeded");
        assert!(content.contains("--- current_time ---\nnow"));
        assert!(content.contains("--- locale_info ---\nen-US"));
        assert!(!content.contains("acme__missing"), "{content}");
    }

    /// A tool that answers with nothing contributes no block, rather than a
    /// heading over emptiness that reads as "this tool says the answer is
    /// blank".
    #[test]
    fn a_call_that_returns_nothing_contributes_no_block() {
        let bp = tool_bp(r#"{ tools = ["current_time", "quiet"] }"#, false);
        let args = args_with("t", HashMap::new(), "/tmp");
        let seeds = resolve_seeds(
            &bp,
            &args,
            "/tmp",
            &seed_policy(),
            &stub_tools(vec![
                ("current_time", Ok("now".to_string())),
                ("quiet", Ok("   \n".to_string())),
            ]),
            &no_read_paths(),
        )
        .unwrap();
        assert_eq!(
            seeds.get("environment").map(String::as_str),
            Some("--- current_time ---\nnow")
        );
    }

    /// Optional and everything failed: the region is left unseeded rather than
    /// holding an empty string, which downstream reads as content.
    #[test]
    fn an_optional_region_whose_calls_all_fail_stays_unseeded() {
        let bp = tool_bp(r#"{ tool = "current_time" }"#, false);
        let args = args_with("t", HashMap::new(), "/tmp");
        let seeds = resolve_seeds(
            &bp,
            &args,
            "/tmp",
            &seed_policy(),
            &stub_tools(vec![("current_time", Err("denied".to_string()))]),
            &no_read_paths(),
        )
        .unwrap();
        assert!(!seeds.contains_key("environment"));
    }

    /// A `required` region is the author saying the run is not worth starting
    /// without it, so a failed call there is a spawn error naming the tool -
    /// the same stance the files, glob and command seeds take.
    #[test]
    fn a_required_region_fails_the_spawn_when_its_tool_does() {
        let bp = tool_bp(r#"{ tool = "current_time" }"#, true);
        let args = args_with("t", HashMap::new(), "/tmp");
        let err = resolve_seeds(
            &bp,
            &args,
            "/tmp",
            &seed_policy(),
            &stub_tools(vec![(
                "current_time",
                Err("set to `ask`, nobody to prompt".to_string()),
            )]),
            &no_read_paths(),
        )
        .unwrap_err();
        assert!(err.contains("environment"), "{err}");
        assert!(err.contains("current_time"), "{err}");
        assert!(err.contains("nobody to prompt"), "{err}");
    }

    /// Required, and every call merely answered with nothing: still an error,
    /// because an empty required region is the state the flag exists to refuse.
    #[test]
    fn a_required_region_fails_when_nothing_was_produced() {
        let bp = tool_bp(r#"{ tool = "quiet" }"#, true);
        let args = args_with("t", HashMap::new(), "/tmp");
        let err = resolve_seeds(
            &bp,
            &args,
            "/tmp",
            &seed_policy(),
            &stub_tools(vec![("quiet", Ok(String::new()))]),
            &no_read_paths(),
        )
        .unwrap_err();
        assert!(err.contains("no tool seed produced anything"), "{err}");
    }

    /// The arguments a blueprint wrote reach the tool unchanged - the case that
    /// would otherwise silently call `which_command` with no program name.
    #[test]
    fn a_call_carries_the_arguments_the_blueprint_wrote() {
        let bp = tool_bp(
            r#"{ tools = [{ name = "which_command", args = { command = "git" } }] }"#,
            false,
        );
        let args = args_with("t", HashMap::new(), "/tmp");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let captured = seen.clone();
        let policy =
            crate::daemon::seed_tool::SeedToolPolicy::new(std::sync::Arc::new(move |name, a| {
                captured.lock().unwrap().push(format!("{name}:{a}"));
                Ok("found".to_string())
            }));
        let seeds = resolve_seeds(
            &bp,
            &args,
            "/tmp",
            &seed_policy(),
            &policy,
            &no_read_paths(),
        )
        .unwrap();
        assert_eq!(
            seeds.get("environment").map(String::as_str),
            Some("--- which_command ---\nfound")
        );
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            ["which_command:{\"command\":\"git\"}".to_string()]
        );
    }

    /// A blueprint with one command-seeded region, optionally `required`.
    fn command_bp(required: bool) -> leviath_core::Blueprint {
        let req = if required { ", required = true" } else { "" };
        bp(&format!(
            r#"facts = {{ kind = "pinned", max_tokens = 500, seed = {{ command = "scan-repo" }}{req} }}"#
        ))
    }

    #[test]
    fn resolve_seeds_command_stores_output() {
        let bp = command_bp(false);
        let args = args_with("t", HashMap::new(), "/tmp");
        let seeds = resolve_seeds(
            &bp,
            &args,
            "/tmp",
            &stub_policy(Ok("src/lib.rs\nsrc/main.rs".to_string())),
            &no_seed_tools(),
            &no_read_paths(),
        )
        .unwrap();
        assert_eq!(
            seeds.get("facts").map(String::as_str),
            Some("src/lib.rs\nsrc/main.rs")
        );
    }

    #[test]
    fn resolve_seeds_command_receives_the_workdir_and_command() {
        // The declared command and the run's workdir reach the runner verbatim.
        let bp = command_bp(false);
        let args = args_with("t", HashMap::new(), "/work");
        let policy = SeedCommandPolicy {
            allowed: true,
            timeout: std::time::Duration::from_secs(9),
            safe_keys: seed_safe_keys(),
            runner: std::sync::Arc::new(|command, workdir, timeout| {
                Ok(format!(
                    "{command}@{}#{}",
                    workdir.display(),
                    timeout.as_secs()
                ))
            }),
        };
        let seeds = resolve_seeds(
            &bp,
            &args,
            "/work",
            &policy,
            &no_seed_tools(),
            &no_read_paths(),
        )
        .unwrap();
        assert_eq!(
            seeds.get("facts").map(String::as_str),
            Some("scan-repo@/work#9")
        );
    }

    #[test]
    fn resolve_seeds_command_failure_is_skipped_when_optional() {
        let bp = command_bp(false);
        let args = args_with("t", HashMap::new(), "/tmp");
        let seeds = resolve_seeds(
            &bp,
            &args,
            "/tmp",
            &stub_policy(Err("timed out".to_string())),
            &no_seed_tools(),
            &no_read_paths(),
        )
        .unwrap();
        assert!(
            !seeds.contains_key("facts"),
            "an optional command seed must not sink the spawn"
        );
    }

    #[test]
    fn resolve_seeds_command_failure_errors_when_required() {
        let bp = command_bp(true);
        let args = args_with("t", HashMap::new(), "/tmp");
        let err = resolve_seeds(
            &bp,
            &args,
            "/tmp",
            &stub_policy(Err("boom".to_string())),
            &no_seed_tools(),
            &no_read_paths(),
        )
        .unwrap_err();
        assert!(err.contains("scan-repo"), "got: {err}");
        assert!(err.contains("boom"), "got: {err}");
    }

    #[test]
    fn resolve_seeds_command_empty_output_is_skipped_when_optional() {
        let bp = command_bp(false);
        let args = args_with("t", HashMap::new(), "/tmp");
        let seeds = resolve_seeds(
            &bp,
            &args,
            "/tmp",
            &stub_policy(Ok("   \n".to_string())),
            &no_seed_tools(),
            &no_read_paths(),
        )
        .unwrap();
        assert!(!seeds.contains_key("facts"));
    }

    #[test]
    fn resolve_seeds_command_empty_output_errors_when_required() {
        let bp = command_bp(true);
        let args = args_with("t", HashMap::new(), "/tmp");
        let err = resolve_seeds(
            &bp,
            &args,
            "/tmp",
            &stub_policy(Ok(String::new())),
            &no_seed_tools(),
            &no_read_paths(),
        )
        .unwrap_err();
        assert!(err.contains("returned empty"), "got: {err}");
    }

    #[test]
    fn resolve_seeds_command_skipped_when_disabled() {
        // `[security] allow_seed_commands = false` / `--no-seed-commands`: the
        // runner is never consulted. The stub would have produced content, so an
        // empty region proves the seed was skipped rather than merely failing.
        let bp = command_bp(false);
        let args = args_with("t", HashMap::new(), "/tmp");
        let mut policy = stub_policy(Ok("SHOULD NOT BE USED".to_string()));
        policy.allowed = false;
        let seeds = resolve_seeds(
            &bp,
            &args,
            "/tmp",
            &policy,
            &no_seed_tools(),
            &no_read_paths(),
        )
        .unwrap();
        assert!(!seeds.contains_key("facts"));
    }

    #[test]
    fn resolve_seeds_required_command_errors_when_disabled() {
        // A required region can't be silently left empty - the run stops with a
        // message naming the switch that turned command seeds off.
        let bp = command_bp(true);
        let args = args_with("t", HashMap::new(), "/tmp");
        let err = resolve_seeds(
            &bp,
            &args,
            "/tmp",
            &SeedCommandPolicy::disabled(),
            &no_seed_tools(),
            &no_read_paths(),
        )
        .unwrap_err();
        assert!(err.contains("allow_seed_commands"), "got: {err}");
    }

    #[test]
    fn resolve_seeds_glob_matching_directory_required_errors() {
        // A required glob that matches a directory entry → reading it as a file
        // fails, so read_and_concat returns Err and resolve_seeds propagates it.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();
        let wd = dir.path().to_string_lossy().to_string();
        let bp = bp(
            r#"specs = { kind = "pinned", max_tokens = 2000, seed = { glob = "sub*" }, required = true }"#,
        );
        let args = args_with("t", HashMap::new(), &wd);
        let err = resolve_seeds(
            &bp,
            &args,
            &wd,
            &seed_policy(),
            &no_seed_tools(),
            &no_read_paths(),
        )
        .unwrap_err();
        assert!(err.contains("read seed file"), "got: {err}");
    }

    #[test]
    fn resolve_seeds_rhai_read_error() {
        let dir = tempfile::tempdir().unwrap();
        let wd = dir.path().to_string_lossy().to_string();
        let bp = bp(
            r#"scripted = { kind = "pinned", max_tokens = 500, seed = { rhai = "nope.rhai" } }"#,
        );
        let args = args_with("t", HashMap::new(), &wd);
        let err = resolve_seeds(
            &bp,
            &args,
            &wd,
            &seed_policy(),
            &no_seed_tools(),
            &no_read_paths(),
        )
        .unwrap_err();
        assert!(err.contains("read rhai seed"), "got: {err}");
    }

    #[test]
    fn resolve_seeds_rhai_empty_required_errors_optional_skips() {
        let dir = tempfile::tempdir().unwrap();
        // A script returning an empty string.
        std::fs::write(dir.path().join("empty.rhai"), r#""""#).unwrap();
        let wd = dir.path().to_string_lossy().to_string();
        let args = args_with("t", HashMap::new(), &wd);
        let req = bp(
            r#"scripted = { kind = "pinned", max_tokens = 500, seed = { rhai = "empty.rhai" }, required = true }"#,
        );
        let err = resolve_seeds(
            &req,
            &args,
            &wd,
            &seed_policy(),
            &no_seed_tools(),
            &no_read_paths(),
        )
        .unwrap_err();
        assert!(err.contains("returned empty"), "got: {err}");
        // Optional + empty → region omitted (no error).
        let opt = bp(
            r#"scripted = { kind = "pinned", max_tokens = 500, seed = { rhai = "empty.rhai" } }"#,
        );
        let seeds = resolve_seeds(
            &opt,
            &args,
            &wd,
            &seed_policy(),
            &no_seed_tools(),
            &no_read_paths(),
        )
        .unwrap();
        assert!(!seeds.contains_key("scripted"));
    }

    #[test]
    fn resolve_seeds_tolerates_unknown_caller_region() {
        // Unknown caller keys are silently unused (CLI validates client-side;
        // ACP stray markers must not fail the spawn).
        let bp = bp(r#"task = { kind = "pinned", max_tokens = 4000, seed = "task_input" }"#);
        let args = args_with(
            "t",
            HashMap::from([("ghost".to_string(), "x".to_string())]),
            "/tmp",
        );
        let seeds = resolve_seeds(
            &bp,
            &args,
            "/tmp",
            &seed_policy(),
            &no_seed_tools(),
            &no_read_paths(),
        )
        .unwrap();
        assert_eq!(seeds.get("task").map(String::as_str), Some("t"));
        assert!(!seeds.contains_key("ghost"));
    }

    // ─── Blueprint-declared paths stay where they belong ─────────────────────

    /// A workdir with a file beside it that the blueprint has no business
    /// reading, standing in for `~/.leviath/config.toml` and its provider keys.
    fn workdir_with_a_neighbour() -> (tempfile::TempDir, String) {
        let root = tempfile::tempdir().expect("tempdir");
        let work = root.path().join("work");
        std::fs::create_dir_all(&work).expect("dirs");
        std::fs::write(root.path().join("config.toml"), "api_key = \"sk-SECRET\"").expect("write");
        let wd = work.to_string_lossy().to_string();
        (root, wd)
    }

    /// The one that mattered: seeded file contents land in a pinned region, so
    /// an escaping path put the user's provider keys in front of the model.
    #[test]
    fn a_seed_file_outside_the_workdir_is_refused() {
        let (_root, wd) = workdir_with_a_neighbour();
        let bp = bp(
            r#"notes = { kind = "pinned", max_tokens = 2000, seed = { files = ["../config.toml"] } }"#,
        );
        let args = args_with("t", HashMap::new(), &wd);
        let err = resolve_seeds(
            &bp,
            &args,
            &wd,
            &seed_policy(),
            &no_seed_tools(),
            &no_read_paths(),
        )
        .unwrap_err();
        assert!(err.contains("outside the working directory"), "{err}");
        assert!(err.contains("read_paths"), "{err}");
    }

    /// The control, so the test above is not passing because everything is
    /// refused: an ordinary path inside the workdir still seeds.
    #[test]
    fn a_seed_file_inside_the_workdir_still_seeds() {
        let (root, wd) = workdir_with_a_neighbour();
        std::fs::write(root.path().join("work").join("notes.md"), "hello").expect("write");
        let bp = bp(
            r#"notes = { kind = "pinned", max_tokens = 2000, seed = { files = ["notes.md"] } }"#,
        );
        let args = args_with("t", HashMap::new(), &wd);
        let seeds = resolve_seeds(
            &bp,
            &args,
            &wd,
            &seed_policy(),
            &no_seed_tools(),
            &no_read_paths(),
        )
        .unwrap();
        assert!(seeds.get("notes").is_some_and(|s| s.contains("hello")));
    }

    /// A glob is checked per *match*, since `../*.toml` cannot be judged before
    /// it is expanded.
    #[test]
    fn a_glob_that_matches_outside_the_workdir_is_refused() {
        let (_root, wd) = workdir_with_a_neighbour();
        let bp =
            bp(r#"notes = { kind = "pinned", max_tokens = 2000, seed = { glob = "../*.toml" } }"#);
        let args = args_with("t", HashMap::new(), &wd);
        let err = resolve_seeds(
            &bp,
            &args,
            &wd,
            &seed_policy(),
            &no_seed_tools(),
            &no_read_paths(),
        )
        .unwrap_err();
        assert!(err.contains("outside the working directory"), "{err}");
    }

    /// `[read_paths]` is the consent mechanism, so a declared-and-granted path
    /// seeds rather than being refused twice over.
    #[test]
    fn a_granted_read_path_lets_a_seed_file_out() {
        let (root, wd) = workdir_with_a_neighbour();
        let outside = root.path().to_string_lossy().to_string();
        let mut policy = no_read_paths();
        policy.blueprint = leviath_core::ReadPathSet::compile(
            std::slice::from_ref(&outside),
            std::path::Path::new(&wd),
            None,
            cfg!(windows),
        )
        .expect("declaration compiles");
        policy.allow_blueprint = true;

        let bp = bp(
            r#"notes = { kind = "pinned", max_tokens = 2000, seed = { files = ["../config.toml"] } }"#,
        );
        let args = args_with("t", HashMap::new(), &wd);
        let seeds =
            resolve_seeds(&bp, &args, &wd, &seed_policy(), &no_seed_tools(), &policy).unwrap();
        assert!(seeds.get("notes").is_some_and(|s| s.contains("sk-SECRET")));
    }

    /// A script is code the blueprint ships, so it has no `[read_paths]` escape
    /// at all: outside the blueprint's own directory is simply refused.
    #[test]
    fn a_hook_script_outside_the_blueprint_directory_is_refused() {
        let root = tempfile::tempdir().expect("tempdir");
        let bp_dir = root.path().join("agents").join("evil");
        std::fs::create_dir_all(&bp_dir).expect("dirs");
        std::fs::write(root.path().join("outside.txt"), "NOT RHAI").expect("write");

        let mut stage = leviath_core::Stage::new(
            "main".to_string(),
            leviath_core::blueprint::ModelConfig::new("p".to_string(), "m".to_string()),
        );
        stage.hooks.on_stage_enter = Some("../../outside.txt".to_string());
        let blueprint = leviath_core::Blueprint::new(
            "evil".to_string(),
            "d".to_string(),
            vec![stage],
            leviath_core::layout::ContextLayout::new(vec![], 1000),
        );

        let bp_path = bp_dir.join("agent.leviath");
        let err = resolve_stage_hook_scripts(&blueprint, bp_path.to_str().expect("utf8"))
            .expect_err("an escaping script path is refused");
        assert!(err.contains("outside the blueprint's directory"), "{err}");
        // Refused before the read, so the file is never opened: a compile
        // failure here would mean it had already been slurped.
        assert!(!err.contains("failed to compile"), "{err}");
    }

    /// Declared but not granted is still a refusal: `[read_paths]` needs both
    /// halves, and a seed path is not a way to get one of them for free.
    #[test]
    fn a_declared_but_ungranted_read_path_does_not_let_a_seed_file_out() {
        let (root, wd) = workdir_with_a_neighbour();
        let outside = root.path().to_string_lossy().to_string();
        let mut policy = no_read_paths();
        policy.blueprint = leviath_core::ReadPathSet::compile(
            std::slice::from_ref(&outside),
            std::path::Path::new(&wd),
            None,
            cfg!(windows),
        )
        .expect("declaration compiles");
        // allow_blueprint stays false and grants stays empty: nothing granted.

        let bp = bp(
            r#"notes = { kind = "pinned", max_tokens = 2000, seed = { files = ["../config.toml"] } }"#,
        );
        let args = args_with("t", HashMap::new(), &wd);
        let err =
            resolve_seeds(&bp, &args, &wd, &seed_policy(), &no_seed_tools(), &policy).unwrap_err();
        assert!(err.contains("outside the working directory"), "{err}");
    }

    // ─── `blueprint:` seed paths ─────────────────────────────────────────────

    /// A blueprint directory holding shipped seed material, a workdir somewhere
    /// else entirely, and a secret *beside* the blueprint directory that a
    /// prefixed path must never reach. Returns (root, manifest path, workdir).
    fn blueprint_dir_with_files() -> (tempfile::TempDir, String, String) {
        let root = tempfile::tempdir().expect("tempdir");
        let bp_dir = root.path().join("agents").join("pack");
        std::fs::create_dir_all(bp_dir.join("config")).expect("dirs");
        std::fs::write(bp_dir.join("config").join("style.md"), "two spaces").expect("write");
        std::fs::write(root.path().join("agents").join("secret.txt"), "sk-SECRET").expect("write");
        let work = root.path().join("work");
        std::fs::create_dir_all(&work).expect("dirs");
        (
            root,
            bp_dir.join("agent.leviath").to_string_lossy().to_string(),
            work.to_string_lossy().to_string(),
        )
    }

    /// The feature itself: `blueprint:` reads from the blueprint's directory,
    /// not the workdir - the file exists only beside the manifest.
    #[test]
    fn a_blueprint_prefixed_seed_file_reads_from_the_blueprint_directory() {
        let (_root, bp_path, wd) = blueprint_dir_with_files();
        let blueprint = bp(
            r#"style = { kind = "pinned", max_tokens = 2000, seed = { files = ["blueprint:config/style.md"] }, required = true }"#,
        );
        let mut args = args_with("t", HashMap::new(), &wd);
        args.blueprint_path = bp_path;
        let seeds = resolve_seeds(
            &blueprint,
            &args,
            &wd,
            &seed_policy(),
            &no_seed_tools(),
            &no_read_paths(),
        )
        .unwrap();
        assert!(seeds.get("style").is_some_and(|s| s.contains("two spaces")));
    }

    /// `blueprint:` claims "a file I ship", so a path that climbs out of the
    /// blueprint directory is a contradiction and is refused outright.
    #[test]
    fn a_blueprint_prefixed_seed_escaping_the_blueprint_directory_is_refused() {
        let (_root, bp_path, wd) = blueprint_dir_with_files();
        let blueprint = bp(
            r#"style = { kind = "pinned", max_tokens = 2000, seed = { files = ["blueprint:../secret.txt"] } }"#,
        );
        let mut args = args_with("t", HashMap::new(), &wd);
        args.blueprint_path = bp_path;
        let err = resolve_seeds(
            &blueprint,
            &args,
            &wd,
            &seed_policy(),
            &no_seed_tools(),
            &no_read_paths(),
        )
        .unwrap_err();
        assert!(err.contains("outside the blueprint's directory"), "{err}");
    }

    /// The one that keeps the fence honest: `[read_paths]` widens what an agent
    /// may read on the user's machine, not what a package pretends to ship, so
    /// a grant covering the target must NOT rescue an escaping `blueprint:` path.
    #[test]
    fn a_read_paths_grant_does_not_rescue_an_escaping_blueprint_seed() {
        let (root, bp_path, wd) = blueprint_dir_with_files();
        let outside = root.path().to_string_lossy().to_string();
        let mut policy = no_read_paths();
        policy.blueprint = leviath_core::ReadPathSet::compile(
            std::slice::from_ref(&outside),
            std::path::Path::new(&wd),
            None,
            cfg!(windows),
        )
        .expect("declaration compiles");
        policy.allow_blueprint = true;

        let blueprint = bp(
            r#"style = { kind = "pinned", max_tokens = 2000, seed = { files = ["blueprint:../secret.txt"] } }"#,
        );
        let mut args = args_with("t", HashMap::new(), &wd);
        args.blueprint_path = bp_path;
        let err = resolve_seeds(
            &blueprint,
            &args,
            &wd,
            &seed_policy(),
            &no_seed_tools(),
            &policy,
        )
        .unwrap_err();
        assert!(err.contains("outside the blueprint's directory"), "{err}");
    }

    /// A prefixed glob expands against the blueprint directory and picks up the
    /// files shipped there.
    #[test]
    fn a_blueprint_prefixed_glob_seeds_shipped_files() {
        let (root, bp_path, wd) = blueprint_dir_with_files();
        let rubrics = root.path().join("agents").join("pack").join("rubrics");
        std::fs::create_dir_all(&rubrics).expect("dirs");
        std::fs::write(rubrics.join("one.md"), "rubric one").expect("write");
        std::fs::write(rubrics.join("two.md"), "rubric two").expect("write");
        let blueprint = bp(
            r#"rubric = { kind = "pinned", max_tokens = 4000, seed = { glob = "blueprint:rubrics/*.md" } }"#,
        );
        let mut args = args_with("t", HashMap::new(), &wd);
        args.blueprint_path = bp_path;
        let seeds = resolve_seeds(
            &blueprint,
            &args,
            &wd,
            &seed_policy(),
            &no_seed_tools(),
            &no_read_paths(),
        )
        .unwrap();
        let rubric = seeds.get("rubric").expect("region seeded");
        assert!(rubric.contains("rubric one") && rubric.contains("rubric two"));
    }

    /// A prefixed glob is fenced per match, exactly as the workdir form is:
    /// `blueprint:../*` expands fine and every escaping match is refused.
    #[test]
    fn a_blueprint_prefixed_glob_matching_outside_is_refused() {
        let (_root, bp_path, wd) = blueprint_dir_with_files();
        let blueprint = bp(
            r#"rubric = { kind = "pinned", max_tokens = 4000, seed = { glob = "blueprint:../*.txt" } }"#,
        );
        let mut args = args_with("t", HashMap::new(), &wd);
        args.blueprint_path = bp_path;
        let err = resolve_seeds(
            &blueprint,
            &args,
            &wd,
            &seed_policy(),
            &no_seed_tools(),
            &no_read_paths(),
        )
        .unwrap_err();
        assert!(err.contains("outside the blueprint's directory"), "{err}");
    }

    /// A rhai seed ships with the blueprint the way its hooks do: the prefix
    /// finds the script beside the manifest and runs it.
    #[test]
    fn a_blueprint_prefixed_rhai_seed_runs_from_the_blueprint_directory() {
        let (root, bp_path, wd) = blueprint_dir_with_files();
        let seeds_dir = root.path().join("agents").join("pack").join("seeds");
        std::fs::create_dir_all(&seeds_dir).expect("dirs");
        std::fs::write(
            seeds_dir.join("plan.rhai"),
            r#""planned: " + input["task"]"#,
        )
        .expect("write");
        let blueprint = bp(
            r#"plan = { kind = "temporary", max_tokens = 500, seed = { rhai = "blueprint:seeds/plan.rhai" } }"#,
        );
        let mut args = args_with("go", HashMap::new(), &wd);
        args.blueprint_path = bp_path;
        let seeds = resolve_seeds(
            &blueprint,
            &args,
            &wd,
            &seed_policy(),
            &no_seed_tools(),
            &no_read_paths(),
        )
        .unwrap();
        assert_eq!(seeds.get("plan").map(String::as_str), Some("planned: go"));
    }

    /// A missing prefixed file answers to `required` exactly as a workdir one
    /// does: required errors, optional leaves the region out.
    #[test]
    fn a_missing_blueprint_prefixed_file_honors_required_and_optional() {
        let (_root, bp_path, wd) = blueprint_dir_with_files();
        let req = bp(
            r#"style = { kind = "pinned", max_tokens = 2000, seed = { files = ["blueprint:missing.md"] }, required = true }"#,
        );
        let mut args = args_with("t", HashMap::new(), &wd);
        args.blueprint_path = bp_path;
        let err = resolve_seeds(
            &req,
            &args,
            &wd,
            &seed_policy(),
            &no_seed_tools(),
            &no_read_paths(),
        )
        .unwrap_err();
        assert!(err.contains("missing.md"), "got: {err}");
        let opt = bp(
            r#"style = { kind = "pinned", max_tokens = 2000, seed = { files = ["blueprint:missing.md"] } }"#,
        );
        let seeds = resolve_seeds(
            &opt,
            &args,
            &wd,
            &seed_policy(),
            &no_seed_tools(),
            &no_read_paths(),
        )
        .unwrap();
        assert!(!seeds.contains_key("style"));
    }

    #[test]
    fn a_rhai_seed_script_outside_the_workdir_is_refused() {
        let (_root, wd) = workdir_with_a_neighbour();
        let bp = bp(
            r#"notes = { kind = "pinned", max_tokens = 2000, seed = { rhai = "../config.toml" } }"#,
        );
        let args = args_with("t", HashMap::new(), &wd);
        let err = resolve_seeds(
            &bp,
            &args,
            &wd,
            &seed_policy(),
            &no_seed_tools(),
            &no_read_paths(),
        )
        .unwrap_err();
        assert!(err.contains("outside the working directory"), "{err}");
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

        let mut blueprint = leviath_core::Blueprint::new(
            "evil".to_string(),
            "d".to_string(),
            vec![],
            leviath_core::layout::ContextLayout::new(vec![], 1000),
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

        let mut stage = leviath_core::Stage::new(
            "main".to_string(),
            leviath_core::blueprint::ModelConfig::new("p".to_string(), "m".to_string()),
        );
        stage.hooks.on_stage_enter = Some("h.rhai".to_string());
        let blueprint = leviath_core::Blueprint::new(
            "good".to_string(),
            "d".to_string(),
            vec![stage],
            leviath_core::layout::ContextLayout::new(vec![], 1000),
        );

        let bp_path = bp_dir.join("agent.leviath");
        let scripts = resolve_stage_hook_scripts(&blueprint, bp_path.to_str().expect("utf8"))
            .expect("a script beside the blueprint loads");
        assert!(scripts.contains_key("h.rhai"));
    }

    // ─── A task the blueprint cannot hold ───────────────────────────────────

    /// The same fixture as [`bp`] but without the implicit `task` region, for
    /// the tests that are *about* a blueprint which accepts no task.
    fn bp_taking_no_task(regions_toml: &str) -> Blueprint {
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
conversation = {{ kind = "sliding_window", max_items = 20, max_tokens = 10000 }}
"#
        );
        leviath_core::manifest::parse_manifest(&toml).unwrap()
    }

    #[test]
    fn a_task_the_blueprint_cannot_hold_is_refused() {
        // Observed live: an agent handed a task it had no region for answered
        // "I'm ready, what would you like?" and finished successfully, having
        // spent a full turn on a question nobody asked.
        let bp = bp_taking_no_task(r#"notes = { kind = "pinned", max_tokens = 100 }"#);
        let args = args_with("do the thing", HashMap::new(), "/w");
        let err = resolve_seeds(
            &bp,
            &args,
            "/w",
            &seed_policy(),
            &no_seed_tools(),
            &no_read_paths(),
        )
        .expect_err("a task with nowhere to go should be refused");
        assert!(err.contains("declares no region to put it in"), "{err}");
        // The message has to say what the agent *does* take, or the user is left
        // guessing which flag to reach for instead.
        assert!(err.contains("takes no caller input at all"), "{err}");
    }

    #[test]
    fn the_refusal_names_the_input_the_agent_does_take() {
        let bp =
            bp_taking_no_task(r#"diff = { kind = "pinned", max_tokens = 100, seed = "diff" }"#);
        let args = args_with("do the thing", HashMap::new(), "/w");
        let err = resolve_seeds(
            &bp,
            &args,
            "/w",
            &seed_policy(),
            &no_seed_tools(),
            &no_read_paths(),
        )
        .expect_err("refused");
        assert!(err.contains("it takes: diff"), "{err}");
    }

    #[test]
    fn an_agent_driven_by_named_regions_still_spawns_with_no_task() {
        // `lev run reviewer --diff @x.patch` supplies no task at all. Refusing
        // *that* would break every agent that takes named input instead.
        let bp =
            bp_taking_no_task(r#"diff = { kind = "pinned", max_tokens = 100, seed = "diff" }"#);
        let mut regions = HashMap::new();
        regions.insert("diff".to_string(), "a patch".to_string());
        let args = args_with("", regions, "/w");
        let seeds = resolve_seeds(
            &bp,
            &args,
            "/w",
            &seed_policy(),
            &no_seed_tools(),
            &no_read_paths(),
        )
        .expect("no task was supplied, so there is nothing to refuse");
        assert_eq!(seeds.get("diff").map(String::as_str), Some("a patch"));
    }

    #[test]
    fn a_whitespace_only_task_is_not_treated_as_a_task() {
        let bp = bp_taking_no_task(r#"notes = { kind = "pinned", max_tokens = 100 }"#);
        let args = args_with("   \n ", HashMap::new(), "/w");
        resolve_seeds(
            &bp,
            &args,
            "/w",
            &seed_policy(),
            &no_seed_tools(),
            &no_read_paths(),
        )
        .expect("blank is the same as absent");
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
            let bp = leviath_core::manifest::parse_manifest(content)
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
