//! The daemon spawner: turns a [`SpawnArgs`] request into a live agent in the
//! shared world - the CLI-side policy the runtime host calls for a `Spawn`
//! control op.
//!
//! It loads the blueprint, resolves each stage's provider/model (against the
//! world's registered providers) and effective tool set, spawns the agent via
//! [`leviath_runtime::pipeline::spawn_agent`], attaches its run metadata /
//! token totals / compaction settings, and registers its per-agent tool state
//! with the [`CliToolService`]. The heavy MCP connections are shared (built once
//! at daemon startup), so this whole path is synchronous - which lets it run
//! straight from the host's control loop.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};

use bevy_ecs::entity::Entity;
use bevy_ecs::world::World;
use leviath_core::blueprint::{Blueprint, ModelConfig};
use leviath_providers::Tool;
use leviath_runtime::ProviderRegistry;
use leviath_runtime::host::{SpawnArgs, SubAgentOp};
use leviath_runtime::interaction_hub::InteractionHub;
use leviath_runtime::persistence::{RunMetadata, TokenTotals};
use leviath_runtime::pipeline::{
    CompactionSettings, PersistWatermark, Providers, ResolvedStage, spawn_agent_seeded,
};
use tokio::sync::Mutex;
use tokio::sync::mpsc::UnboundedSender;

use crate::config::Config;
use crate::daemon::seed_command::SeedCommandPolicy;
use crate::daemon::subagent::SubAgentHandle;
use crate::daemon::tool_service::{AgentToolState, CliToolService};

/// Default max sub-agent tree depth when a blueprint doesn't set one.
const DEFAULT_SUBAGENT_DEPTH: usize = 3;

/// Resolve a stage's [`ModelConfig`] to a concrete `(provider, model)` against
/// the registered providers. Honors a `--model` override (`provider/model` or a
/// bare `model`), otherwise picks the first listed model whose provider is
/// registered, then falls back to the user default (when `allow_user_default`),
/// and finally to the config's first listed entry. (Ported from the executor's
/// inline resolution.)
pub fn resolve_stage_model(
    model_cfg: &ModelConfig,
    model_override: Option<&str>,
    config: &Config,
    registry: &ProviderRegistry,
) -> (String, String) {
    let (override_provider, override_model) = match model_override {
        Some(ov) if ov.contains('/') => {
            let (p, m) = ov.split_once('/').unwrap();
            (Some(p.to_string()), Some(m.to_string()))
        }
        Some(ov) => (None, Some(ov.to_string())),
        None => (None, None),
    };

    // Full provider/model override wins outright.
    if let Some(provider) = override_provider {
        return (provider, override_model.unwrap_or_default());
    }

    // First listed model whose provider is registered.
    for entry in &model_cfg.models {
        if registry.has(&entry.provider) {
            let model = override_model
                .clone()
                .unwrap_or_else(|| entry.model.clone());
            return (entry.provider.clone(), model);
        }
    }

    // Fall back to the user's default model, or finally the first listed entry.
    user_default_model(model_cfg, override_model.as_deref(), config, registry).unwrap_or_else(
        || {
            (
                model_cfg.provider().to_string(),
                model_cfg.model().to_string(),
            )
        },
    )
}

/// The user-default fallback for [`resolve_stage_model`]: `None` when the stage
/// forbids it or no usable default exists.
fn user_default_model(
    model_cfg: &ModelConfig,
    override_model: Option<&str>,
    config: &Config,
    registry: &ProviderRegistry,
) -> Option<(String, String)> {
    if !model_cfg.allow_user_default {
        return None;
    }
    if let Some(model) = override_model {
        return Some((config.default_provider.clone(), model.to_string()));
    }
    if let Some(default_model) = &config.default_model
        && registry.has(&config.default_provider)
    {
        return Some((config.default_provider.clone(), default_model.clone()));
    }
    None
}

/// Resolve every stage's provider/model + effective tool set from the blueprint.
fn resolve_stages(
    blueprint: &Blueprint,
    model_override: Option<&str>,
    config: &Config,
    registry: &ProviderRegistry,
    all_tool_defs: &[Tool],
) -> Vec<ResolvedStage> {
    blueprint
        .stages
        .iter()
        .map(|stage| {
            let (provider_name, model) =
                resolve_stage_model(&stage.model, model_override, config, registry);
            // Empty `available_tools` exposes no tools; otherwise filter the full
            // set by name (alias-resolved). A name matching nothing (a typo, or an
            // MCP tool whose server isn't installed) is simply omitted.
            let tools = filter_tools_by_available(all_tool_defs, &stage.available_tools);
            ResolvedStage {
                provider_name,
                model,
                tools,
            }
        })
        .collect()
}

/// The directories scanned for an agent's Rhai script tools, in precedence order
/// (earlier wins on a name collision): the agent's own `<agent_dir>/tools/`, then
/// `extra` (the run workdir's `tools/`, only for `dynamic_tools` agents so a
/// mid-run write is picked up), then the global `~/.leviath/tools/`. `Option`'s
/// iterator flattens the "no parent" / "no home" cases without a dangling
/// `if let` else region.
fn script_scan_dirs(
    blueprint_path: &str,
    extra: Option<std::path::PathBuf>,
) -> Vec<std::path::PathBuf> {
    std::path::Path::new(blueprint_path)
        .parent()
        .map(|d| d.join("tools"))
        .into_iter()
        .chain(extra)
        .chain(leviath_core::tools_dir())
        .collect()
}

/// Read and compile every custom region's Rhai script declared by `blueprint`
/// (global layout plus each stage's per-stage layout), keyed by the script
/// path as written. Paths resolve relative to the blueprint's directory (the
/// script-tool convention - the script travels with the agent), with absolute
/// paths passing through `Path::join` unchanged. Each distinct path is read
/// and compiled once; regions sharing a script share the compiled AST.
///
/// A missing or uncompilable script is a **hard spawn error** (fail fast,
/// before any tokens are spent): a hook that silently never ran would change
/// every inference with no signal. Runtime hook *eval* failures, by contrast,
/// warn and fall back per hook.
pub(crate) fn resolve_region_scripts(
    blueprint: &Blueprint,
    blueprint_path: &str,
) -> Result<HashMap<String, Arc<leviath_scripting::region_hook::RegionScript>>, String> {
    let base = std::path::Path::new(blueprint_path)
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    let mut scripts = HashMap::new();

    let layouts = std::iter::once(&blueprint.context_layout).chain(
        blueprint
            .stages
            .iter()
            .filter_map(|s| s.context_layout.as_ref()),
    );
    for layout in layouts {
        for region in &layout.regions {
            let leviath_core::RegionKind::Custom { script, .. } = &region.kind else {
                continue;
            };
            if scripts.contains_key(script) {
                continue;
            }
            let path = base.join(script);
            let source = std::fs::read_to_string(&path).map_err(|e| {
                format!(
                    "region '{}': cannot read custom region script '{}': {e}",
                    region.name,
                    path.display()
                )
            })?;
            let compiled =
                leviath_scripting::region_hook::compile(script, &source).map_err(|e| {
                    format!(
                        "region '{}': custom region script failed to compile: {e}",
                        region.name
                    )
                })?;
            scripts.insert(script.clone(), Arc::new(compiled));
        }
    }
    Ok(scripts)
}

/// Names already claimed by a built-in, sub-agent, or MCP tool - a discovered
/// script tool colliding with one of these is dropped (never shadows a core tool).
fn reserved_tool_names(builtin_names: &HashSet<String>, mcp_tool_defs: &[Tool]) -> HashSet<String> {
    let mut reserved: HashSet<String> = builtin_names.clone();
    reserved.extend(leviath_tools::BuiltinTools::subagent_tool_names());
    reserved.extend(mcp_tool_defs.iter().map(|t| t.name.clone()));
    reserved
}

/// Map a script's self-declared `@requires` capability name to the platform
/// [`ToolCapability`] it corresponds to. An unrecognized name returns `None`,
/// which the discovery pass treats as unsatisfiable (the tool is dropped) so a
/// typo can't silently slip a tool through the platform gate.
fn script_cap(name: &str) -> Option<leviath_tools::ToolCapability> {
    match name {
        "network" | "net" | "http" => Some(leviath_tools::ToolCapability::Network),
        "shell" | "process" | "process_spawn" => Some(leviath_tools::ToolCapability::ProcessSpawn),
        "filesystem" | "file" | "fs" => Some(leviath_tools::ToolCapability::FileSystem),
        _ => None,
    }
}

/// Whether `platform` can satisfy every capability a script `@requires`. An
/// unknown capability name is never satisfiable.
fn platform_satisfies_caps(
    platform: &leviath_tools::PlatformCapabilities,
    required_caps: &[String],
) -> bool {
    required_caps
        .iter()
        .all(|c| script_cap(c).is_some_and(|cap| platform.supports(cap)))
}

/// Whether the *current* platform can satisfy a script's `@requires` - the same
/// gate `discover_script_tools_in` applies at spawn. Exposed so the read-only CLI
/// surfaces (`lev tools`, `lev validate`, `lev mcp list`) report a tool's real
/// availability (and flag an unknown/typo'd capability) instead of listing a tool
/// the daemon would silently drop.
pub(crate) fn current_platform_satisfies(required_caps: &[String]) -> bool {
    platform_satisfies_caps(
        &leviath_tools::PlatformCapabilities::current(),
        required_caps,
    )
}

/// Discover and compile the script tools in `dirs`, returning the compiled set,
/// the routable names (collisions against `reserved` excluded), and the
/// advertised `Tool` defs.
///
/// A tool whose `@requires` capabilities the current platform can't satisfy is
/// dropped here (self-declared platform gating) - mirroring how
/// built-ins filter against [`PlatformCapabilities`].
pub(crate) fn discover_script_tools_in(
    dirs: &[std::path::PathBuf],
    reserved: &HashSet<String>,
) -> (leviath_scripting::ScriptToolSet, HashSet<String>, Vec<Tool>) {
    let (set, skipped) = leviath_scripting::ScriptToolSet::discover(dirs);
    for s in &skipped {
        // Pre-format the path to a plain string so the `tracing` field carries no
        // inline method call (an inline `%s.path.display()` leaves a macro
        // sub-region llvm-cov can't attribute even with the event enabled).
        let path = s.path.display().to_string();
        tracing::warn!(tool = %path, reason = %s.reason, "skipping invalid script tool");
    }
    let platform = leviath_tools::PlatformCapabilities::current();
    let mut names = HashSet::new();
    let mut defs = Vec::new();
    for meta in set.metas() {
        if reserved.contains(&meta.name) {
            tracing::warn!(tool = %meta.name, "script tool name collides with an existing tool - ignoring");
            continue;
        }
        if !platform_satisfies_caps(&platform, &meta.required_caps) {
            let caps = meta.required_caps.join(", ");
            tracing::warn!(tool = %meta.name, requires = %caps, "script tool requires a capability this platform lacks - ignoring");
            continue;
        }
        names.insert(meta.name.clone());
        defs.push(Tool {
            name: meta.name.clone(),
            description: meta.description.clone(),
            parameters: meta.parameters_schema(),
        });
    }
    (set, names, defs)
}

/// Filter `all` tool defs down to those a stage's `available_tools` names
/// (alias-resolved). Shared by spawn-time stage resolution and the mid-run
/// tool-service refresh so both apply Layer-1 identically.
pub fn filter_tools_by_available(all: &[Tool], available: &[String]) -> Vec<Tool> {
    if available.is_empty() {
        return Vec::new();
    }
    all.iter()
        .filter(|t| {
            available
                .iter()
                .any(|n| leviath_tools::canonical_tool_name(n) == t.name)
        })
        .cloned()
        .collect()
}

/// Discover the agent's Rhai script tools and build their `Tool`
/// defs (the spawn-time entry point). `extra_dir` adds the run workdir's `tools/`
/// for `dynamic_tools` agents.
fn discover_script_tools(
    blueprint_path: &str,
    builtin_names: &HashSet<String>,
    mcp_tool_defs: &[Tool],
    extra_dir: Option<std::path::PathBuf>,
) -> (leviath_scripting::ScriptToolSet, HashSet<String>, Vec<Tool>) {
    let dirs = script_scan_dirs(blueprint_path, extra_dir);
    let reserved = reserved_tool_names(builtin_names, mcp_tool_defs);
    discover_script_tools_in(&dirs, &reserved)
}

/// Build one agent's [`AgentToolState`] from the shared executors + config.
///
/// `stage_perms_by_index` holds every stage's `[tool_permissions]` (in stage
/// order); the entry stage's map seeds `stage_perms`, and the pipeline's
/// `sync_stage` swaps in the right one as the agent changes stage.
#[allow(clippy::too_many_arguments)]
fn build_tool_state(
    builtins: Arc<leviath_tools::BuiltinTools>,
    builtin_names: HashSet<String>,
    mcp: Arc<Mutex<leviath_mcp::ToolExecutor>>,
    config: &Config,
    hub: &InteractionHub,
    run_id: &str,
    entry_stage: &str,
    entry_index: usize,
    stage_perms_by_index: Vec<HashMap<String, String>>,
    agent_perms: HashMap<String, String>,
    agent_name: &str,
    launch_overrides: HashMap<String, crate::config::ToolPolicy>,
    subagent: Option<SubAgentHandle>,
    sandbox: Option<Arc<crate::daemon::sandbox_manager::SandboxManager>>,
    script_tools: leviath_scripting::ScriptToolSet,
    script_tool_names: HashSet<String>,
    script_host: Arc<dyn leviath_scripting::ScriptHost>,
    dynamic: Option<Arc<crate::daemon::tool_service::DynamicToolCtx>>,
    unattended: bool,
) -> Arc<AgentToolState> {
    let entry_perms = stage_perms_by_index
        .get(entry_index)
        .cloned()
        .unwrap_or_default();
    Arc::new(AgentToolState {
        builtins,
        mcp,
        builtin_names,
        launch_overrides: Arc::new(launch_overrides),
        session_allows: Arc::new(Mutex::new(HashSet::new())),
        stage_perms: Arc::new(StdMutex::new(entry_perms)),
        stage_perms_by_index: Arc::new(stage_perms_by_index),
        agent_perms: Arc::new(agent_perms),
        // The ceiling a blueprint may tighten but not loosen: the user's global
        // `[tool_permissions]` plus any `[agent_tool_permissions.<name>]` grant
        // they made for this specific agent. Resolved once here so every later
        // `resolve_policy` reads one flat map.
        global_perms: Arc::new(config.permissions_for_agent(agent_name)),
        interaction: hub.backend_for(run_id),
        unattended,
        stage_name: Arc::new(StdMutex::new(entry_stage.to_string())),
        subagent,
        sandbox,
        script_tools: Arc::new(StdMutex::new(script_tools)),
        script_tool_names: Arc::new(StdMutex::new(script_tool_names)),
        script_host,
        dynamic,
    })
}

/// Resolve every region's initial content from its blueprint-declared
/// [`RegionSeed`] plus the caller-provided values on `args`, into a
/// name→content map ready for [`spawn_agent_seeded`].
///
/// The caller map is `{ "task": args.task } ∪ args.regions` (a `regions["task"]`
/// wins). Then:
/// - `CallerInput { name }` pulls from the caller map; if the region is
///   `required` and the value is missing/blank this returns `Err` - the
///   required-at-spawn gate, before any inference.
/// - `Files` / `Glob` read workdir files; `Literal` is verbatim; `Rhai` runs a
///   workdir script whose `String` return seeds the region.
/// - `Command` runs a shell command in the workdir under `commands` -
///   sandboxed, time- and size-capped, and skippable. Every failure is
///   non-fatal unless the region is `required`.
/// - Any caller key (other than `task`) that isn't a declared `CallerInput`
///   region is rejected (typo protection, mirrors the CLI-side check).
fn resolve_seeds(
    blueprint: &Blueprint,
    args: &SpawnArgs,
    workdir: &str,
    commands: &SeedCommandPolicy,
) -> Result<HashMap<String, String>, String> {
    use leviath_core::layout::RegionSeed;

    // The effective caller-supplied values: task text plus any named regions.
    let mut caller: HashMap<String, String> = HashMap::new();
    caller.insert("task".to_string(), args.task.clone());
    for (k, v) in &args.regions {
        caller.insert(k.clone(), v.clone());
    }

    // Unknown caller keys are tolerated here (silently unused): the CLI already
    // rejects typos client-side in `resolve_spawn_args`, and an ACP host sending
    // a stray `---region:...---` marker shouldn't fail the whole turn over it.

    let base = std::path::Path::new(workdir);
    let mut seeds: HashMap<String, String> = HashMap::new();

    for region in &blueprint.context_layout.regions {
        let Some(seed) = &region.seed else { continue };
        match seed {
            RegionSeed::CallerInput { name } => {
                let value = caller.get(name).map(|s| s.as_str()).unwrap_or("");
                if value.trim().is_empty() {
                    if region.required {
                        return Err(region.required_message.clone().unwrap_or_else(|| {
                            format!(
                                "required region '{}' was not provided; supply it via \
                                 --{name} <text|@file> (CLI), a ---region:{name}--- block \
                                 (ACP), or the API `regions` field",
                                region.name
                            )
                        }));
                    }
                    // Optional and unprovided - leave the region empty.
                    continue;
                }
                seeds.insert(region.name.clone(), value.to_string());
            }
            RegionSeed::Literal { text } => {
                seeds.insert(region.name.clone(), text.clone());
            }
            RegionSeed::Files { paths } => {
                let content = read_and_concat(
                    &region.name,
                    paths.iter().map(|p| base.join(p)),
                    region.required,
                )?;
                if let Some(content) = content {
                    seeds.insert(region.name.clone(), content);
                }
            }
            RegionSeed::Glob { pattern } => {
                let full = base.join(pattern);
                let full = full.to_string_lossy();
                let matches = glob::glob(&full)
                    .map_err(|e| format!("region '{}': bad glob '{pattern}': {e}", region.name))?;
                let paths: Vec<std::path::PathBuf> = matches.filter_map(|m| m.ok()).collect();
                let content = read_and_concat(&region.name, paths.into_iter(), region.required)?;
                match content {
                    Some(content) => {
                        seeds.insert(region.name.clone(), content);
                    }
                    None if region.required => {
                        return Err(format!(
                            "required region '{}': glob '{pattern}' matched no files",
                            region.name
                        ));
                    }
                    None => {}
                }
            }
            RegionSeed::Rhai { script } => {
                let path = base.join(script);
                let src = std::fs::read_to_string(&path).map_err(|e| {
                    format!(
                        "region '{}': read rhai seed '{}': {e}",
                        region.name,
                        path.display()
                    )
                })?;
                let mut input = rhai::Map::new();
                input.insert("task".into(), rhai::Dynamic::from(args.task.clone()));
                input.insert("workdir".into(), rhai::Dynamic::from(workdir.to_string()));
                let out = leviath_scripting::ScriptEngine::new()
                    .transform(&src, input)
                    .map_err(|e| format!("region '{}': rhai seed failed: {e}", region.name))?;
                if !out.trim().is_empty() {
                    seeds.insert(region.name.clone(), out);
                } else if region.required {
                    return Err(format!(
                        "required region '{}': rhai seed '{script}' returned empty",
                        region.name
                    ));
                }
            }
            // A command seed *executes* at spawn, before any inference and so
            // before any tool-approval prompt. It is therefore skipped outright
            // when disabled, and every failure mode is non-fatal unless the
            // region is `required` (mirroring the Files/Glob arms above): a
            // discovery nicety must never be able to sink a run.
            RegionSeed::Command { command } => {
                if !commands.allowed {
                    if region.required {
                        return Err(format!(
                            "required region '{}': command seeds are disabled \
                             (`[security] allow_seed_commands = false` or --no-seed-commands)",
                            region.name
                        ));
                    }
                    tracing::warn!(
                        region = %region.name,
                        "command seed skipped: command seeds are disabled"
                    );
                    continue;
                }
                match commands.run(command, base) {
                    Ok(out) if !out.trim().is_empty() => {
                        seeds.insert(region.name.clone(), out);
                    }
                    Ok(_) => {
                        if region.required {
                            return Err(format!(
                                "required region '{}': command seed '{command}' returned empty",
                                region.name
                            ));
                        }
                        tracing::warn!(
                            region = %region.name,
                            command = %command,
                            "command seed returned no output; region left empty"
                        );
                    }
                    Err(e) => {
                        if region.required {
                            return Err(format!(
                                "required region '{}': command seed '{command}' failed: {e}",
                                region.name
                            ));
                        }
                        tracing::warn!(
                            region = %region.name,
                            command = %command,
                            error = %e,
                            "command seed failed; region left empty"
                        );
                    }
                }
            }
        }
    }

    Ok(seeds)
}

/// Read each file and concatenate with `--- <path> ---` headers. Returns
/// `Ok(None)` when the list is empty; a missing/unreadable file is an error only
/// when `required`, else it is skipped.
fn read_and_concat(
    region: &str,
    paths: impl Iterator<Item = std::path::PathBuf>,
    required: bool,
) -> Result<Option<String>, String> {
    let mut parts: Vec<String> = Vec::new();
    for path in paths {
        match std::fs::read_to_string(&path) {
            Ok(text) => parts.push(format!("--- {} ---\n{}", path.display(), text)),
            Err(e) => {
                if required {
                    return Err(format!(
                        "region '{region}': read seed file '{}': {e}",
                        path.display()
                    ));
                }
            }
        }
    }
    Ok((!parts.is_empty()).then(|| parts.join("\n\n")))
}

/// Resolve an agent's `[read_paths]` declarations against the user's config
/// into the policy its file tools enforce, plus a warning to surface when the
/// declarations exist but nothing grants them.
///
/// A declared-but-ungranted agent still spawns - its out-of-workdir reads are
/// refused per path with the same guidance - but the warning fires once here
/// so the user learns about it at spawn rather than from a mid-run tool error.
/// A malformed entry (in the blueprint or in the user's own grant list) is a
/// hard spawn error: silently dropping it would either under-grant or run the
/// agent with less vision than its author designed for.
fn build_read_path_policy(
    blueprint: &leviath_core::Blueprint,
    config: &crate::config::Config,
    workdir: &std::path::Path,
) -> Result<(leviath_core::ReadPathPolicy, Option<String>), String> {
    let Some(rp) = blueprint
        .read_paths
        .as_ref()
        .filter(|rp| !rp.allow.is_empty())
    else {
        return Ok((leviath_core::ReadPathPolicy::inactive(), None));
    };
    let home = leviath_core::home_dir();
    let declared =
        leviath_core::ReadPathSet::compile(&rp.allow, workdir, home.as_deref(), cfg!(windows))
            .map_err(|e| format!("agent '{}' [read_paths]: {e}", blueprint.name))?;
    let grant_entries = config.read_path_grants_for_agent(&blueprint.name);
    let grants =
        leviath_core::ReadPathSet::compile(&grant_entries, workdir, home.as_deref(), cfg!(windows))
            .map_err(|e| format!("read_paths grant in your config.toml: {e}"))?;
    let allow_blueprint = config.security.allow_blueprint_read_paths;
    let warning = (!allow_blueprint && grants.is_empty()).then(|| {
        let entries = rp
            .allow
            .iter()
            .map(|e| format!("\"{e}\""))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "agent '{name}' declares [read_paths] but nothing grants them; reads outside \
             the workdir will be refused. To grant them, add to your config.toml either:\n\
             [security]\nallow_blueprint_read_paths = true\n\
             or the specific paths:\n[agent_read_paths.{name}]\nallow = [{entries}]",
            name = blueprint.name,
        )
    });
    Ok((
        leviath_core::ReadPathPolicy {
            agent: blueprint.name.clone(),
            blueprint: declared,
            grants,
            allow_blueprint,
        },
        warning,
    ))
}

/// Raise the read tools to `Private` for an agent whose `[read_paths]` are
/// actually granted: they can pull in content from outside the workdir -
/// design docs, run archives, whatever else was granted - which the default
/// `Internal` classification (written for workdir files) understates.
fn bump_read_sensitivities(
    map: &mut HashMap<String, leviath_core::TaintLevel>,
    read_paths_granted: bool,
) {
    if !read_paths_granted {
        return;
    }
    for tool in ["read_file", "read_files", "list_dir"] {
        if let Some(level) = map.get_mut(tool) {
            *level = (*level).max(leviath_core::TaintLevel::Private);
        }
    }
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
#[allow(clippy::too_many_arguments)]
pub fn build_agent(
    world: &mut World,
    tool_service: &CliToolService,
    config: &Config,
    shared_mcp: Arc<Mutex<leviath_mcp::ToolExecutor>>,
    mcp_tool_defs: &[Tool],
    hub: &InteractionHub,
    args: &SpawnArgs,
    now_secs: i64,
    subagent_tx: UnboundedSender<SubAgentOp>,
) -> Result<Entity, String> {
    build_agent_inner(
        world,
        tool_service,
        config,
        shared_mcp,
        mcp_tool_defs,
        hub,
        args,
        now_secs,
        subagent_tx,
        true,
    )
}

/// Like [`build_agent`], but skips the required-at-spawn region gate - used by
/// restart recovery, which reloads a run that already passed the gate when first
/// spawned and whose context window is restored from a snapshot after this call.
#[allow(clippy::too_many_arguments)]
pub fn build_agent_for_reload(
    world: &mut World,
    tool_service: &CliToolService,
    config: &Config,
    shared_mcp: Arc<Mutex<leviath_mcp::ToolExecutor>>,
    mcp_tool_defs: &[Tool],
    hub: &InteractionHub,
    args: &SpawnArgs,
    now_secs: i64,
    subagent_tx: UnboundedSender<SubAgentOp>,
) -> Result<Entity, String> {
    build_agent_inner(
        world,
        tool_service,
        config,
        shared_mcp,
        mcp_tool_defs,
        hub,
        args,
        now_secs,
        subagent_tx,
        false,
    )
}

#[allow(clippy::too_many_arguments)]
fn build_agent_inner(
    world: &mut World,
    tool_service: &CliToolService,
    config: &Config,
    shared_mcp: Arc<Mutex<leviath_mcp::ToolExecutor>>,
    mcp_tool_defs: &[Tool],
    hub: &InteractionHub,
    args: &SpawnArgs,
    now_secs: i64,
    subagent_tx: UnboundedSender<SubAgentOp>,
    enforce_seeds: bool,
) -> Result<Entity, String> {
    // 0. The working directory must exist before anything is built over it.
    // `ToolContext::new` silently keeps a path it can't canonicalize, so without
    // this a bogus workdir spawns a healthy-looking agent whose every tool call
    // fails with a message naming the shell rather than the directory (#107).
    if !std::fs::metadata(&args.workdir).is_ok_and(|m| m.is_dir()) {
        return Err(format!(
            "workspace '{}' does not exist or is not a directory",
            args.workdir
        ));
    }

    // 1. Load the blueprint (the client resolves the manifest path).
    let content = std::fs::read_to_string(&args.blueprint_path)
        .map_err(|e| format!("read manifest '{}': {e}", args.blueprint_path))?;
    let mut blueprint = leviath_core::manifest::parse_manifest(&content)
        .map_err(|e| format!("parse manifest: {e}"))?;
    blueprint
        .validate()
        .map_err(|e| format!("invalid blueprint: {e}"))?;
    // A request-level `--max-depth` overrides the blueprint's sub-agent depth cap.
    if let Some(md) = args.max_depth {
        blueprint.max_child_depth = Some(md);
    }
    // Apply the config's `default_max_iterations` to any stage that doesn't set
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

    // 2a. Entry stage + per-stage sandbox resolution. Each stage's effective
    // sandbox cascades stage → agent → global (`resolve_sandbox`); building the
    // manager creates any containers up front and fails here (returning the
    // error to the spawner) when a required runtime is unavailable and the config
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
                config.sandbox.as_ref(),
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
    // `[read_paths]` declarations are resolved against the user's config here -
    // declared AND granted, or the read tools never leave the workdir.
    let (read_path_policy, read_path_warning) =
        build_read_path_policy(&blueprint, config, std::path::Path::new(&args.workdir))?;
    if let Some(warning) = &read_path_warning {
        tracing::warn!(agent_name = %blueprint.name, "{warning}");
    }
    // Whether the agent can actually read outside its workdir - feeds the
    // taint bump below, captured before the policy moves into the context.
    let read_paths_granted = read_path_policy.is_active()
        && (read_path_policy.allow_blueprint || !read_path_policy.grants.is_empty());
    let tool_ctx = leviath_tools::ToolContext::new(std::path::PathBuf::from(&args.workdir))
        .with_read_paths(read_path_policy);
    let mut builtins = leviath_tools::BuiltinTools::new(tool_ctx);
    if let Some(mgr) = &sandbox {
        builtins =
            builtins.with_shell_executor(mgr.clone() as Arc<dyn leviath_tools::ShellExecutor>);
    }
    let builtins = Arc::new(builtins);
    let builtin_names: HashSet<String> = builtins.names().into_iter().collect();
    let mut all_tool_defs = builtins.tool_defs();
    all_tool_defs.extend(leviath_tools::BuiltinTools::subagent_tool_defs());
    all_tool_defs.extend(mcp_tool_defs.iter().cloned());
    // The non-script defs (built-in + sub-agent + MCP), captured before script
    // defs are appended - a `dynamic_tools` agent re-filters against these plus a
    // fresh script scan on each mid-run refresh.
    let static_tool_defs = all_tool_defs.clone();

    // 2c. Rhai script tools (issue #97): discover and compile the agent's
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
        mcp_tool_defs,
        workdir_tools_dir.clone(),
    );
    all_tool_defs.extend(script_defs);

    // 3. Resolve stages against the world's providers.
    let stages = {
        let registry = &world
            .get_resource::<Providers>()
            .expect("Providers resource present in a PipelineWorld")
            .0;
        resolve_stages(
            &blueprint,
            args.model.as_deref(),
            config,
            registry,
            &all_tool_defs,
        )
    };

    // 4. Snapshot the blueprint bits we need after it's moved into the world.
    let agent_name = blueprint.name.clone();
    let num_stages = blueprint.stages.len();
    let compaction = blueprint.compaction_config.clone();
    let max_child_depth = blueprint.max_child_depth.unwrap_or(DEFAULT_SUBAGENT_DEPTH);
    // Taint gate: opt-in via the blueprint's `[security]` block, else the global
    // config's `taint_tracking`, else off. Cascading through
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
    // Each stage's `available_tools` (Layer-1 allowlist), captured before the
    // blueprint moves - a `dynamic_tools` agent re-filters against these on refresh.
    let stage_available: Vec<Vec<String>> = blueprint
        .stages
        .iter()
        .map(|s| s.available_tools.clone())
        .collect();
    let model_label = stages
        .first()
        .map(|s| format!("{}/{}", s.provider_name, s.model));

    // 5. Resolve region seeds (caller input + blueprint-declared sources) into
    // concrete content. On a fresh spawn (`enforce_seeds`), required caller-input
    // regions that weren't provided fail here - before any inference, so no
    // tokens are spent. On reload the window is restored from a snapshot after
    // this, so seeding is skipped entirely.
    // Command seeds (issue #108) run here, so they inherit the entry stage's
    // sandbox (built in step 2a above) and are refused by either the machine-wide
    // `[security] allow_seed_commands` switch or this run's `--no-seed-commands`.
    let seeds = if enforce_seeds {
        let policy = SeedCommandPolicy::new(
            config.security.allow_seed_commands && !args.no_seed_commands,
            std::time::Duration::from_secs(config.limits.script_shell_timeout_secs),
            sandbox.clone(),
        );
        resolve_seeds(&blueprint, args, &args.workdir, &policy)?
    } else {
        HashMap::new()
    };

    // 5b. Read + compile-check custom regions' Rhai scripts (issue #152) -
    // once per distinct path, blueprint-dir-relative. Runs on fresh spawns
    // AND reloads (the hooks must work after a restart), and a broken script
    // is a hard error either way.
    let region_scripts = resolve_region_scripts(&blueprint, &args.blueprint_path)?;

    // 6. Spawn the agent.
    let entity = spawn_agent_seeded(
        world,
        args.run_id.clone(),
        blueprint,
        &seeds,
        stages,
        config.batch_tool_hint,
        region_scripts,
    )?;

    // 7. Attach run metadata / token totals / persistence watermark (+ optional
    // compaction settings).
    let metadata = RunMetadata {
        run_id: args.run_id.clone(),
        agent_name: agent_name.clone(),
        agent_path: args.blueprint_path.clone(),
        task: args.task.clone(),
        model: model_label,
        workdir: args.workdir.clone(),
        num_stages,
        started_at: now_secs,
        parent_run_id: args.parent_run_id.clone(),
        metadata: args.metadata.clone(),
        callback_url: args.callback_url.clone(),
        callback_secret: args.callback_secret.clone(),
        title: None,
    };
    {
        let mut entity_mut = world.entity_mut(entity);
        entity_mut.insert((
            metadata,
            TokenTotals::default(),
            PersistWatermark::default(),
            // Fresh counters; a reloaded run gets its accumulated flags put back
            // by `recovery::reload_persisted_agents`.
            leviath_runtime::persistence::RunOutcomeFlags::default(),
        ));
        // Mark eligible runs for one-shot title generation (the `title` module
        // fills `RunMetadata.title`, which the dashboard displays and
        // searches). Root runs only: sub-agents inherit their parent's context
        // in the run list, and titling every fan-out worker would multiply
        // cheap-but-nonzero LLM calls for no UX gain.
        (config.title.enabled && !args.task.is_empty() && args.parent_run_id.is_none())
            .then_some(leviath_runtime::title::PendingTitle)
            .into_iter()
            .for_each(|marker| {
                entity_mut.insert(marker);
            });
        // `--yolo` means run unattended, so a blueprint's stage-boundary
        // checkpoints are approved rather than parked on a hub nobody is
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
        compaction.into_iter().for_each(|cc| {
            entity_mut.insert(CompactionSettings(cc));
        });
        // Attach the taint gate + per-tool sensitivities and turn on the window's
        // taint tracking when the blueprint opts in (`Option`'s iterator keeps the
        // enforcement path region-free when taint is off).
        tool_sensitivities.into_iter().for_each(|sensitivities| {
            let mut gate = leviath_runtime::TaintGate::new(security.clone());
            gate.apply_mcp_overrides(&mcp_overrides);
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

    // 8. Register the per-agent tool state.
    // Launch overrides: `--yolo` allows every tool (`*` wildcard); `--allow X`
    // allows tool `X` outright.
    let mut launch_overrides: HashMap<String, crate::config::ToolPolicy> = HashMap::new();
    if args.yolo {
        launch_overrides.insert("*".to_string(), crate::config::ToolPolicy::Allow);
    }
    for tool in &args.allow {
        launch_overrides.insert(tool.clone(), crate::config::ToolPolicy::Allow);
    }
    let subagent = SubAgentHandle {
        sender: subagent_tx,
        parent_run_id: args.run_id.clone(),
        workdir: args.workdir.clone(),
        max_depth: max_child_depth,
        no_seed_commands: args.no_seed_commands,
    };
    // Rhai script-tool host (Layer 3): resolve `[tool_script_permissions]` once,
    // with `read_file`/`shell` `inherit` deferring to the agent's own resolved
    // policy for that built-in (evaluated against the entry stage).
    let entry_stage_perms = stage_perms_by_index
        .get(entry_index)
        .cloned()
        .unwrap_or_default();
    // The agent may carry its own `[tool_script_permissions]` (it can ship its own
    // tool scripts), overlaid per field on the global config.
    let effective_script_perms = crate::daemon::script_host::effective_script_permissions(
        &config.tool_script_permissions,
        &content,
    );
    // Same ceiling `build_tool_state` resolves for the built-in tools: the
    // global `[tool_permissions]` with this agent's `[agent_tool_permissions]`
    // grants overlaid. Passing the raw global map here would silently ignore a
    // per-agent grant when a script tool's `inherit` defers to the built-in.
    let agent_scoped_perms = config.permissions_for_agent(&agent_name);
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
            )
        },
    );
    let script_host: Arc<dyn leviath_scripting::ScriptHost> = Arc::new(
        crate::daemon::script_host::DaemonScriptHost::new(
            script_allow,
            std::path::PathBuf::from(&args.workdir),
        )
        // Route a script `shell()` through the agent's per-stage sandbox (so a
        // script can't escape the isolation the stage declared) and cap it at the
        // configured wall-clock timeout.
        .with_shell(
            sandbox.clone(),
            std::time::Duration::from_secs(config.limits.script_shell_timeout_secs),
        )
        // `[security] allow_local_network`. Off by default, so a `web_fetch` URL
        // the model picked out of attacker-influenced context cannot reach cloud
        // metadata, the user's own `lev serve`, or their LAN.
        .with_local_network(config.security.allow_local_network)
        // `[security] allow_env_vars`. Empty by default, so a script tool cannot
        // read the user's provider keys and post them somewhere.
        .with_env_allowlist(config.security.allow_env_vars.clone()),
    );
    // Build the dynamic-tools re-resolution context (issue #97 escape hatch) and
    // tag the entity `DynamicTools` so the runtime polls it for mid-run re-scans.
    let dynamic = dynamic_tools.then(|| {
        world
            .entity_mut(entity)
            .insert(leviath_runtime::pipeline::DynamicTools);
        Arc::new(crate::daemon::tool_service::DynamicToolCtx {
            scan_dirs: script_scan_dirs(&args.blueprint_path, workdir_tools_dir),
            reserved_names: reserved_tool_names(&builtin_names, mcp_tool_defs),
            static_defs: static_tool_defs,
            stage_available,
            dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    });
    let state = build_tool_state(
        builtins,
        builtin_names,
        shared_mcp,
        config,
        hub,
        &args.run_id,
        &entry_stage,
        entry_index,
        stage_perms_by_index,
        agent_perms,
        &agent_name,
        launch_overrides,
        Some(subagent),
        sandbox,
        script_tools,
        script_tool_names,
        script_host,
        dynamic,
        args.yolo,
    );
    tool_service.register(entity, state);

    Ok(entity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_runtime::world::PipelineWorld;

    /// A throwaway sub-agent op sender for tests that don't exercise the bridge.
    fn sub_tx() -> UnboundedSender<SubAgentOp> {
        tokio::sync::mpsc::unbounded_channel().0
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
            r.register(p.to_string(), Arc::new(FakeProvider));
        }
        r
    }

    struct FakeProvider;
    #[async_trait::async_trait]
    impl leviath_providers::Provider for FakeProvider {
        async fn infer(
            &self,
            _r: leviath_providers::InferenceRequest,
        ) -> leviath_providers::Result<leviath_providers::InferenceResponse> {
            Err(leviath_providers::ProviderError::Other(
                "test provider".to_string(),
            ))
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
    fn resolve_full_override_wins() {
        let (p, m) = resolve_stage_model(
            &model_cfg(vec![("anthropic", "x")]),
            Some("openai/gpt-5"),
            &Config::default(),
            &registry_with(&[]),
        );
        assert_eq!((p.as_str(), m.as_str()), ("openai", "gpt-5"));
    }

    #[test]
    fn resolve_first_available_model() {
        // anthropic not registered, openai is → picks openai.
        let (p, m) = resolve_stage_model(
            &model_cfg(vec![("anthropic", "a"), ("openai", "o")]),
            None,
            &Config::default(),
            &registry_with(&["openai"]),
        );
        assert_eq!((p.as_str(), m.as_str()), ("openai", "o"));
    }

    #[test]
    fn resolve_model_only_override_keeps_available_provider() {
        let (p, m) = resolve_stage_model(
            &model_cfg(vec![("openai", "o")]),
            Some("gpt-override"),
            &Config::default(),
            &registry_with(&["openai"]),
        );
        assert_eq!((p.as_str(), m.as_str()), ("openai", "gpt-override"));
    }

    #[test]
    fn resolve_user_default_when_nothing_listed_available() {
        // Listed provider "ghost" is unavailable; anthropic (the default) is.
        let config = Config {
            default_provider: "anthropic".to_string(),
            default_model: Some("claude-default".to_string()),
            ..Default::default()
        };
        let (p, m) = resolve_stage_model(
            &model_cfg(vec![("ghost", "g")]),
            None,
            &config,
            &registry_with(&["anthropic"]),
        );
        assert_eq!((p.as_str(), m.as_str()), ("anthropic", "claude-default"));
    }

    #[test]
    fn resolve_user_default_with_model_override() {
        let config = Config {
            default_provider: "anthropic".to_string(),
            ..Default::default()
        };
        let (p, m) = resolve_stage_model(
            &model_cfg(vec![("ghost", "g")]),
            Some("just-a-model"),
            &config,
            &registry_with(&[]),
        );
        assert_eq!((p.as_str(), m.as_str()), ("anthropic", "just-a-model"));
    }

    #[test]
    fn resolve_user_default_provider_unavailable_falls_through() {
        // allow_user_default, a default_model set, but the default provider isn't
        // registered ⇒ neither user-default branch fires ⇒ last resort.
        let config = Config {
            default_provider: "ghost-default".to_string(),
            default_model: Some("dm".to_string()),
            ..Default::default()
        };
        let (p, m) = resolve_stage_model(
            &model_cfg(vec![("ghost", "g")]),
            None,
            &config,
            &registry_with(&[]),
        );
        assert_eq!((p.as_str(), m.as_str()), ("ghost", "g"));
    }

    #[test]
    fn resolve_last_resort_first_listed() {
        // No override, nothing available, no usable default → first listed entry.
        let config = Config::default(); // default_model None
        let (p, m) = resolve_stage_model(
            &model_cfg(vec![("ghost", "g")]),
            None,
            &config,
            &registry_with(&[]),
        );
        assert_eq!((p.as_str(), m.as_str()), ("ghost", "g"));
    }

    #[test]
    fn resolve_no_user_default_uses_last_resort() {
        let mut cfg = model_cfg(vec![("ghost", "g")]);
        cfg.allow_user_default = false; // forbid the default fallback
        let config = Config {
            default_model: Some("would-be-default".to_string()),
            ..Default::default()
        };
        let (p, m) = resolve_stage_model(&cfg, None, &config, &registry_with(&["anthropic"]));
        assert_eq!((p.as_str(), m.as_str()), ("ghost", "g"));
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
            std::env::temp_dir(),
            Handle::current(),
        );
        (world, cli)
    }

    fn spawn_args(path: &str) -> SpawnArgs {
        SpawnArgs {
            run_id: "run-x".to_string(),
            blueprint_path: path.to_string(),
            task: "do the thing".to_string(),
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
            cli.as_ref(),
            &Config::default(),
            mcp,
            &[],
            &hub,
            &args,
            100,
            sub_tx(),
        )
        .unwrap_err();
        assert!(err.contains("region 'brain'"), "got: {err}");
        assert!(err.contains("hooks/brain.rhai"), "got: {err}");
    }

    #[tokio::test]
    async fn build_agent_rejects_a_workdir_that_is_missing_or_not_a_directory() {
        // `ToolContext::new` silently keeps a path it can't canonicalize, so
        // without this check a bogus workdir spawns a healthy-looking agent
        // whose every tool call then fails with ENOENT (issue #107).
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
                cli.as_ref(),
                &Config::default(),
                mcp,
                &[],
                &hub,
                &args,
                100,
                sub_tx(),
            )
            .unwrap_err();
            assert!(err.contains("workspace"), "got: {err}");
            assert!(err.contains(&workdir), "got: {err}");
        }
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
            cli.as_ref(),
            &Config::default(),
            mcp,
            &[],
            &hub,
            &spawn_args(&manifest.to_string_lossy()),
            100,
            sub_tx(),
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

    #[tokio::test]
    async fn build_agent_marks_root_runs_for_titling_but_not_subagents() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(
            &manifest,
            "[agent]\nname = \"titler\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n\
             [stages.main]\nmodel = { provider = \"anthropic\", model = \"m\" }\n",
        )
        .unwrap();
        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();

        // Root run with the default-enabled [title] config: marked.
        let root = build_agent(
            world.world_mut(),
            cli.as_ref(),
            &Config::default(),
            Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
            &[],
            &hub,
            &spawn_args(&manifest.to_string_lossy()),
            100,
            sub_tx(),
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
            cli.as_ref(),
            &Config::default(),
            Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
            &[],
            &hub,
            &child_args,
            100,
            sub_tx(),
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
            cli.as_ref(),
            &config,
            Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
            &[],
            &hub,
            &off_args,
            100,
            sub_tx(),
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
            cli.as_ref(),
            &Config::default(),
            mcp,
            &[],
            &hub,
            &spawn_args(&manifest.to_string_lossy()),
            100,
            sub_tx(),
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
            cli.as_ref(),
            &Config::default(),
            mcp,
            &[],
            &hub,
            &spawn_args(&manifest.to_string_lossy()),
            100,
            sub_tx(),
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
            cli.as_ref(),
            &Config::default(),
            mcp,
            &[],
            &hub,
            &spawn_args(&manifest.to_string_lossy()),
            100,
            sub_tx(),
        )
        .expect("spawn succeeds");
        // The agent's tool state carries a sandbox manager.
        let state = cli.take(entity).expect("state registered");
        assert!(state.sandbox.is_some(), "sandbox manager attached");
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
            cli.as_ref(),
            &Config::default(),
            mcp,
            &[],
            &hub,
            &spawn_args(&manifest.to_string_lossy()),
            100,
            sub_tx(),
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
            cli.as_ref(),
            &Config::default(),
            mcp,
            &[],
            &hub,
            &args,
            100,
            sub_tx(),
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
        // the agent's `ask_user_*` tools (#107): unattended means unattended.
        assert!(
            world
                .world()
                .get::<leviath_runtime::components::InteractionAutoApprove>(entity)
                .is_some()
        );
        assert!(cli.take(entity).expect("tool state registered").unattended);
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
            cli.as_ref(),
            &Config::default(),
            mcp,
            &[],
            &hub,
            &spawn_args(&manifest.to_string_lossy()),
            100,
            sub_tx(),
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
        // Bug regression: a blueprint with no `[security]` block and a default
        // (taint-off) global config must NOT attach the taint gate - an
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
            cli.as_ref(),
            &Config::default(), // taint_tracking defaults to false
            mcp,
            &[],
            &hub,
            &spawn_args(&manifest.to_string_lossy()),
            100,
            sub_tx(),
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
            cli.as_ref(),
            &Config::default(),
            mcp,
            &[],
            &hub,
            &spawn_args(&manifest.to_string_lossy()),
            100,
            sub_tx(),
        )
        .expect("spawn succeeds");

        assert_eq!(world.agent_status(entity), Some(AgentStatus::Active));
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
            cli.as_ref(),
            &Config::default(),
            mcp,
            &[],
            &hub,
            &spawn_args(&manifest.to_string_lossy()),
            100,
            sub_tx(),
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
            cli.as_ref(),
            &config,
            mcp,
            &[],
            &hub,
            &args,
            100,
            sub_tx(),
        )
        .expect("spawn succeeds");
        assert_eq!(world.agent_status(entity), Some(AgentStatus::Active));

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
            cli.as_ref(),
            &Config::default(),
            mcp,
            &[],
            &hub,
            &spawn_args(&manifest.to_string_lossy()),
            100,
            sub_tx(),
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
            cli.as_ref(),
            &config,
            mcp,
            &[],
            &hub,
            &args,
            100,
            sub_tx(),
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
            cli.as_ref(),
            &config,
            mcp,
            &[],
            &hub,
            &spawn_args(&manifest.to_string_lossy()),
            100,
            sub_tx(),
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
            cli.as_ref(),
            &config,
            mcp,
            &[],
            &hub,
            &spawn_args(&manifest.to_string_lossy()),
            100,
            sub_tx(),
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

    #[test]
    fn resolve_stages_empty_available_tools_gets_none() {
        let mut stage =
            leviath_core::Stage::new("s".to_string(), model_cfg(vec![("anthropic", "m")]));
        stage.available_tools = vec![]; // empty ⇒ no tools
        let layout = leviath_core::layout::ContextLayout::new(vec![], 1000);
        let bp = Blueprint::new("t".to_string(), "d".to_string(), vec![stage], layout);
        let tools = vec![Tool {
            name: "read_file".to_string(),
            description: String::new(),
            parameters: serde_json::Value::Null,
        }];
        let resolved = resolve_stages(
            &bp,
            None,
            &Config::default(),
            &registry_with(&["anthropic"]),
            &tools,
        );
        assert!(resolved[0].tools.is_empty());
    }

    #[test]
    fn resolve_stages_matches_by_alias_and_skips_unknown_names() {
        // A stage names `bash` (an alias) and a not-installed MCP tool. The
        // filter must select the canonical `shell` definition for the alias and
        // silently omit the unknown name (no error, no panic).
        let mut stage =
            leviath_core::Stage::new("s".to_string(), model_cfg(vec![("anthropic", "m")]));
        stage.available_tools = vec!["bash".to_string(), "acme__uninstalled".to_string()];
        let layout = leviath_core::layout::ContextLayout::new(vec![], 1000);
        let bp = Blueprint::new("t".to_string(), "d".to_string(), vec![stage], layout);
        let tools = vec![
            Tool {
                name: "shell".to_string(),
                description: String::new(),
                parameters: serde_json::Value::Null,
            },
            Tool {
                name: "read_file".to_string(),
                description: String::new(),
                parameters: serde_json::Value::Null,
            },
        ];
        let resolved = resolve_stages(
            &bp,
            None,
            &Config::default(),
            &registry_with(&["anthropic"]),
            &tools,
        );
        let selected: Vec<&str> = resolved[0].tools.iter().map(|t| t.name.as_str()).collect();
        // `bash` resolved to `shell`; the unknown MCP name and unlisted
        // `read_file` were both excluded.
        assert_eq!(selected, vec!["shell"]);
    }

    #[tokio::test]
    async fn build_agent_read_error() {
        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        let err = build_agent(
            world.world_mut(),
            cli.as_ref(),
            &Config::default(),
            mcp,
            &[],
            &hub,
            &spawn_args("/no/such/manifest.leviath"),
            100,
            sub_tx(),
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
            cli.as_ref(),
            &Config::default(),
            mcp,
            &[],
            &hub,
            &spawn_args(&manifest.to_string_lossy()),
            100,
            sub_tx(),
        );
        assert!(result.is_err(), "expected spawn error, got {result:?}");
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
            cli.as_ref(),
            &Config::default(),
            mcp,
            &[],
            &hub,
            &spawn_args(&manifest.to_string_lossy()),
            100,
            sub_tx(),
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
            cli.as_ref(),
            &Config::default(),
            mcp,
            &[],
            &hub,
            &spawn_args(&manifest.to_string_lossy()),
            100,
            sub_tx(),
        )
        .expect("spawn succeeds");
        assert_eq!(world.agent_status(entity), Some(AgentStatus::Active));
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
            cli.as_ref(),
            &config,
            mcp,
            &[],
            &hub,
            &spawn_args(&manifest.to_string_lossy()),
            100,
            sub_tx(),
        )
        .expect("spawn succeeds");
        assert_eq!(world.agent_status(entity), Some(AgentStatus::Active));
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
            cli.as_ref(),
            &Config::default(),
            mcp,
            &[],
            &hub,
            &spawn_args(&manifest.to_string_lossy()),
            100,
            sub_tx(),
        )
        .expect("spawn succeeds even when nothing grants the declaration");
        assert_eq!(world.agent_status(entity), Some(AgentStatus::Active));
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
            cli.as_ref(),
            &config,
            mcp,
            &[],
            &hub,
            &spawn_args(&manifest.to_string_lossy()),
            100,
            sub_tx(),
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
            cli.as_ref(),
            &Config::default(),
            mcp,
            &[],
            &hub,
            &spawn_args(&manifest.to_string_lossy()),
            100,
            sub_tx(),
        )
        .unwrap_err();
        assert!(err.contains("parse manifest"));
    }

    // ─── resolve_seeds ────────────────────────────────────────────────────────

    fn bp(regions_toml: &str) -> Blueprint {
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
        }
    }

    /// The default policy for the non-command seed tests: command seeds off, so
    /// nothing is ever executed by a test that isn't about command seeds.
    fn seed_policy() -> SeedCommandPolicy {
        SeedCommandPolicy::disabled()
    }

    /// A policy whose runner is a stub returning `result`, for the command-seed
    /// arms (no real process, deterministic on every platform).
    fn stub_policy(result: Result<String, String>) -> SeedCommandPolicy {
        SeedCommandPolicy {
            allowed: true,
            timeout: std::time::Duration::from_secs(1),
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
        let seeds = resolve_seeds(&bp, &args, "/tmp", &seed_policy()).unwrap();
        assert_eq!(seeds.get("task").map(String::as_str), Some("build it"));
        assert_eq!(seeds.get("criteria").map(String::as_str), Some("be safe"));
    }

    #[test]
    fn resolve_seeds_required_caller_input_missing_is_error() {
        let bp =
            bp(r#"spec = { kind = "pinned", max_tokens = 2000, seed = "input", required = true }"#);
        let args = args_with("t", HashMap::new(), "/tmp");
        let err = resolve_seeds(&bp, &args, "/tmp", &seed_policy()).unwrap_err();
        assert!(err.contains("spec"), "got: {err}");
    }

    #[test]
    fn resolve_seeds_optional_caller_input_missing_is_omitted() {
        let bp = bp(r#"notes = { kind = "pinned", max_tokens = 2000, seed = "input" }"#);
        let args = args_with("t", HashMap::new(), "/tmp");
        let seeds = resolve_seeds(&bp, &args, "/tmp", &seed_policy()).unwrap();
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
        let seeds =
            resolve_seeds(&bp, &args, &dir.path().to_string_lossy(), &seed_policy()).unwrap();
        assert_eq!(seeds.get("lit").map(String::as_str), Some("hello"));
        let docs = seeds.get("docs").unwrap();
        assert!(docs.contains("alpha") && docs.contains("beta"));
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
        let seeds = resolve_seeds(&bp, &args, &wd, &seed_policy()).unwrap();
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
        let seeds = resolve_seeds(&bp, &args, &wd, &seed_policy()).unwrap();
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
        let err = resolve_seeds(&req, &args, &wd, &seed_policy()).unwrap_err();
        assert!(err.contains("missing.txt"), "got: {err}");
        // Optional + a missing file → the region is simply omitted.
        let opt = bp(
            r#"docs = { kind = "pinned", max_tokens = 2000, seed = { files = ["missing.txt"] } }"#,
        );
        let seeds = resolve_seeds(&opt, &args, &wd, &seed_policy()).unwrap();
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
        let err = resolve_seeds(&req, &args, &wd, &seed_policy()).unwrap_err();
        assert!(err.contains("matched no files"), "got: {err}");
        // Optional glob with no matches → region omitted.
        let opt =
            bp(r#"specs = { kind = "pinned", max_tokens = 2000, seed = { glob = "none/*.md" } }"#);
        let seeds = resolve_seeds(&opt, &args, &wd, &seed_policy()).unwrap();
        assert!(!seeds.contains_key("specs"));
    }

    #[test]
    fn resolve_seeds_bad_glob_pattern_errors() {
        // An unclosed `[` is an invalid glob pattern → `glob::glob` returns Err.
        let dir = tempfile::tempdir().unwrap();
        let wd = dir.path().to_string_lossy().to_string();
        let bp = bp(r#"specs = { kind = "pinned", max_tokens = 2000, seed = { glob = "[" } }"#);
        let args = args_with("t", HashMap::new(), &wd);
        let err = resolve_seeds(&bp, &args, &wd, &seed_policy()).unwrap_err();
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
        let err = resolve_seeds(&bp, &args, &wd, &seed_policy()).unwrap_err();
        assert!(err.contains("rhai seed failed"), "got: {err}");
    }

    // ─── command seeds (issue #108) ──────────────────────────────────────────

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
            runner: std::sync::Arc::new(|command, workdir, timeout| {
                Ok(format!(
                    "{command}@{}#{}",
                    workdir.display(),
                    timeout.as_secs()
                ))
            }),
        };
        let seeds = resolve_seeds(&bp, &args, "/work", &policy).unwrap();
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
        let err =
            resolve_seeds(&bp, &args, "/tmp", &stub_policy(Err("boom".to_string()))).unwrap_err();
        assert!(err.contains("scan-repo"), "got: {err}");
        assert!(err.contains("boom"), "got: {err}");
    }

    #[test]
    fn resolve_seeds_command_empty_output_is_skipped_when_optional() {
        let bp = command_bp(false);
        let args = args_with("t", HashMap::new(), "/tmp");
        let seeds =
            resolve_seeds(&bp, &args, "/tmp", &stub_policy(Ok("   \n".to_string()))).unwrap();
        assert!(!seeds.contains_key("facts"));
    }

    #[test]
    fn resolve_seeds_command_empty_output_errors_when_required() {
        let bp = command_bp(true);
        let args = args_with("t", HashMap::new(), "/tmp");
        let err = resolve_seeds(&bp, &args, "/tmp", &stub_policy(Ok(String::new()))).unwrap_err();
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
        let seeds = resolve_seeds(&bp, &args, "/tmp", &policy).unwrap();
        assert!(!seeds.contains_key("facts"));
    }

    #[test]
    fn resolve_seeds_required_command_errors_when_disabled() {
        // A required region can't be silently left empty - the run stops with a
        // message naming the switch that turned command seeds off.
        let bp = command_bp(true);
        let args = args_with("t", HashMap::new(), "/tmp");
        let err = resolve_seeds(&bp, &args, "/tmp", &SeedCommandPolicy::disabled()).unwrap_err();
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
        let err = resolve_seeds(&bp, &args, &wd, &seed_policy()).unwrap_err();
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
        let err = resolve_seeds(&bp, &args, &wd, &seed_policy()).unwrap_err();
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
        let err = resolve_seeds(&req, &args, &wd, &seed_policy()).unwrap_err();
        assert!(err.contains("returned empty"), "got: {err}");
        // Optional + empty → region omitted (no error).
        let opt = bp(
            r#"scripted = { kind = "pinned", max_tokens = 500, seed = { rhai = "empty.rhai" } }"#,
        );
        let seeds = resolve_seeds(&opt, &args, &wd, &seed_policy()).unwrap();
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
        let seeds = resolve_seeds(&bp, &args, "/tmp", &seed_policy()).unwrap();
        assert_eq!(seeds.get("task").map(String::as_str), Some("t"));
        assert!(!seeds.contains_key("ghost"));
    }
}
