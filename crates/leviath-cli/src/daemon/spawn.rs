//! The daemon spawner: turns a [`SpawnArgs`] request into a live agent in the
//! shared world — the CLI-side policy the runtime host calls for a `Spawn`
//! control op.
//!
//! It loads the blueprint, resolves each stage's provider/model (against the
//! world's registered providers) and effective tool set, spawns the agent via
//! [`leviath_runtime::pipeline::spawn_agent`], attaches its run metadata /
//! token totals / compaction settings, and registers its per-agent tool state
//! with the [`CliToolService`]. The heavy MCP connections are shared (built once
//! at daemon startup), so this whole path is synchronous — which lets it run
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
            // An empty `available_tools` means the stage exposes no tools (matches
            // the imperative executor).
            let tools = if stage.available_tools.is_empty() {
                Vec::new()
            } else {
                // Resolve aliases (e.g. `bash` → `shell`) so a stage that names a
                // tool by an alias still selects its canonical definition. A name
                // that matches nothing (an alias-free typo, or an MCP tool whose
                // server isn't installed) is simply omitted — no error.
                all_tool_defs
                    .iter()
                    .filter(|t| {
                        stage
                            .available_tools
                            .iter()
                            .any(|n| leviath_tools::canonical_tool_name(n) == t.name)
                    })
                    .cloned()
                    .collect()
            };
            ResolvedStage {
                provider_name,
                model,
                tools,
            }
        })
        .collect()
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
    launch_overrides: HashMap<String, crate::config::ToolPolicy>,
    subagent: Option<SubAgentHandle>,
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
        global_perms: Arc::new(config.tool_permissions.clone()),
        interaction: hub.backend_for(run_id),
        stage_name: Arc::new(StdMutex::new(entry_stage.to_string())),
        subagent,
    })
}

/// Resolve every region's initial content from its blueprint-declared
/// [`RegionSeed`] plus the caller-provided values on `args`, into a
/// name→content map ready for [`spawn_agent_seeded`].
///
/// The caller map is `{ "task": args.task } ∪ args.regions` (a `regions["task"]`
/// wins). Then:
/// - `CallerInput { name }` pulls from the caller map; if the region is
///   `required` and the value is missing/blank this returns `Err` — the
///   required-at-spawn gate, before any inference.
/// - `Files` / `Glob` read workdir files; `Literal` is verbatim; `Rhai` runs a
///   workdir script whose `String` return seeds the region.
/// - Any caller key (other than `task`) that isn't a declared `CallerInput`
///   region is rejected (typo protection, mirrors the CLI-side check).
fn resolve_seeds(
    blueprint: &Blueprint,
    args: &SpawnArgs,
    workdir: &str,
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
                    // Optional and unprovided — leave the region empty.
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

/// Load the blueprint at `args.blueprint_path`, spawn the agent into `world`,
/// register its tool state, and return the new entity. Operates on the raw ECS
/// [`World`] so it is callable both from the host's spawner (via
/// `PipelineWorld::world_mut`) and from a fan-out world-system.
///
/// Enforces the required-at-spawn region gate — a fresh spawn whose required
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

/// Like [`build_agent`], but skips the required-at-spawn region gate — used by
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
            stage.max_iterations.get_or_insert(default_max);
        }
    }

    // 2. Per-agent built-in tools (over the agent's workdir).
    let builtins = Arc::new(leviath_tools::BuiltinTools::new(
        leviath_tools::ToolContext::new(std::path::PathBuf::from(&args.workdir)),
    ));
    let builtin_names: HashSet<String> = builtins.names().into_iter().collect();
    let mut all_tool_defs = builtins.tool_defs();
    all_tool_defs.extend(leviath_tools::BuiltinTools::subagent_tool_defs());
    all_tool_defs.extend(mcp_tool_defs.iter().cloned());

    // 3. Resolve stages against the world's providers.
    let entry_stage = blueprint
        .entry_stage
        .clone()
        .or_else(|| blueprint.stages.first().map(|s| s.name.clone()))
        .unwrap_or_default();
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
    // blueprint with no `[security]` block correctly inherits the global setting
    // — off by default. When on, the agent's outbound tool calls are gated
    // against its context taint + the policy allowlist; when off no gate is
    // attached (zero enforcement overhead).
    let security = leviath_core::taint::resolve_security(
        config.taint_tracking,
        blueprint.security.as_ref(),
        None,
    );
    let tool_sensitivities: Option<HashMap<String, leviath_core::TaintLevel>> =
        security.taint_tracking.then(|| {
            let gate = leviath_runtime::TaintGate::new(security.clone());
            all_tool_defs
                .iter()
                .map(|t| {
                    (
                        t.name.clone(),
                        gate.tool_classification(&t.name).sensitivity,
                    )
                })
                .collect()
        });
    // Per-stage tool permissions (in stage order) + the entry stage's index, for
    // the tool state's stage-scoped policy layer.
    let stage_perms_by_index: Vec<HashMap<String, String>> = blueprint
        .stages
        .iter()
        .map(|s| s.tool_permissions.clone())
        .collect();
    let entry_index = blueprint
        .stages
        .iter()
        .position(|s| s.name == entry_stage)
        .unwrap_or(0);
    // Agent-level tool permissions (the manifest's top-level `[tool_permissions]`,
    // recorded in blueprint metadata). Populates the tool state's agent-level
    // policy layer (between stage and global in `resolve_policy`), which was
    // previously always empty.
    let agent_perms = blueprint.agent_tool_permissions();
    let model_label = stages
        .first()
        .map(|s| format!("{}/{}", s.provider_name, s.model));

    // 5. Resolve region seeds (caller input + blueprint-declared sources) into
    // concrete content. On a fresh spawn (`enforce_seeds`), required caller-input
    // regions that weren't provided fail here — before any inference, so no
    // tokens are spent. On reload the window is restored from a snapshot after
    // this, so seeding is skipped entirely.
    let seeds = if enforce_seeds {
        resolve_seeds(&blueprint, args, &args.workdir)?
    } else {
        HashMap::new()
    };

    // 6. Spawn the agent.
    let entity = spawn_agent_seeded(
        world,
        args.run_id.clone(),
        blueprint,
        &seeds,
        stages,
        config.batch_tool_hint,
    )?;

    // 7. Attach run metadata / token totals / persistence watermark (+ optional
    // compaction settings).
    let metadata = RunMetadata {
        run_id: args.run_id.clone(),
        agent_name,
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
        ));
        // `Option`'s iterator inserts compaction settings when present without a
        // dangling `if let` block-end region.
        compaction.into_iter().for_each(|cc| {
            entity_mut.insert(CompactionSettings(cc));
        });
        // Attach the taint gate + per-tool sensitivities and turn on the window's
        // taint tracking when the blueprint opts in (`Option`'s iterator keeps the
        // enforcement path region-free when taint is off).
        tool_sensitivities.into_iter().for_each(|sensitivities| {
            entity_mut.insert((
                leviath_runtime::TaintGate::new(security.clone()),
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
    };
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
        launch_overrides,
        Some(subagent),
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
        std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../agents/coder/agent.leviath"),
        )
        .expect("read coder manifest")
    }

    fn test_world() -> (PipelineWorld, Arc<CliToolService>) {
        let cli = Arc::new(CliToolService::new());
        let world = PipelineWorld::new(
            registry_with(&["anthropic", "openai", "ollama"]),
            cli.clone(),
            InferencePoolConfig::new(),
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
            allow: Vec::new(),
            max_depth: None,
            parent_run_id: None,
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
    async fn build_agent_errors_when_required_caller_region_missing() {
        // A required caller-input region that the request doesn't provide makes
        // build_agent fail (via resolve_seeds) before spawning — no inference.
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
    }

    #[tokio::test]
    async fn build_agent_no_security_block_leaves_taint_off_by_default() {
        // Bug regression: a blueprint with no `[security]` block and a default
        // (taint-off) global config must NOT attach the taint gate — previously
        // `unwrap_or_default()` forced it on for every agent.
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
            }],
        )()
        .await;
        assert_eq!(out[0].0, "c1");
        assert!(!out[0].1.contains("no tool state"));
    }

    #[tokio::test]
    async fn build_agent_applies_yolo_allow_and_max_depth() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(&manifest, coder_manifest()).unwrap();

        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        // Config denies read_file; the launch overrides must win.
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

        // `--yolo` overrides the config deny: read_file executes (an error reading
        // a missing file) rather than being `[denied]`.
        let out = leviath_runtime::pipeline::ToolService::exec_for(
            cli.as_ref(),
            entity,
            vec![leviath_providers::ToolCall {
                id: "c1".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "/no/such/file"}),
            }],
        )()
        .await;
        assert!(!out[0].1.contains("[denied]"));
    }

    #[tokio::test]
    async fn build_agent_honors_agent_level_tool_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        // A top-level `[tool_permissions]` block denying a builtin — no stage
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
            }],
        )()
        .await;
        assert!(
            out[0].1.contains("[denied]"),
            "agent-level deny should block read_file"
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
        // `None` disables the config default entirely — the stage stays uncapped.
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
            })
            .await
            .is_err()
        );
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
            allow: Vec::new(),
            max_depth: None,
            parent_run_id: None,
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
        let seeds = resolve_seeds(&bp, &args, "/tmp").unwrap();
        assert_eq!(seeds.get("task").map(String::as_str), Some("build it"));
        assert_eq!(seeds.get("criteria").map(String::as_str), Some("be safe"));
    }

    #[test]
    fn resolve_seeds_required_caller_input_missing_is_error() {
        let bp =
            bp(r#"spec = { kind = "pinned", max_tokens = 2000, seed = "input", required = true }"#);
        let args = args_with("t", HashMap::new(), "/tmp");
        let err = resolve_seeds(&bp, &args, "/tmp").unwrap_err();
        assert!(err.contains("spec"), "got: {err}");
    }

    #[test]
    fn resolve_seeds_optional_caller_input_missing_is_omitted() {
        let bp = bp(r#"notes = { kind = "pinned", max_tokens = 2000, seed = "input" }"#);
        let args = args_with("t", HashMap::new(), "/tmp");
        let seeds = resolve_seeds(&bp, &args, "/tmp").unwrap();
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
        let seeds = resolve_seeds(&bp, &args, &dir.path().to_string_lossy()).unwrap();
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
        let seeds = resolve_seeds(&bp, &args, &wd).unwrap();
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
        let seeds = resolve_seeds(&bp, &args, &wd).unwrap();
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
        let err = resolve_seeds(&req, &args, &wd).unwrap_err();
        assert!(err.contains("missing.txt"), "got: {err}");
        // Optional + a missing file → the region is simply omitted.
        let opt = bp(
            r#"docs = { kind = "pinned", max_tokens = 2000, seed = { files = ["missing.txt"] } }"#,
        );
        let seeds = resolve_seeds(&opt, &args, &wd).unwrap();
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
        let err = resolve_seeds(&req, &args, &wd).unwrap_err();
        assert!(err.contains("matched no files"), "got: {err}");
        // Optional glob with no matches → region omitted.
        let opt =
            bp(r#"specs = { kind = "pinned", max_tokens = 2000, seed = { glob = "none/*.md" } }"#);
        let seeds = resolve_seeds(&opt, &args, &wd).unwrap();
        assert!(!seeds.contains_key("specs"));
    }

    #[test]
    fn resolve_seeds_bad_glob_pattern_errors() {
        // An unclosed `[` is an invalid glob pattern → `glob::glob` returns Err.
        let dir = tempfile::tempdir().unwrap();
        let wd = dir.path().to_string_lossy().to_string();
        let bp = bp(r#"specs = { kind = "pinned", max_tokens = 2000, seed = { glob = "[" } }"#);
        let args = args_with("t", HashMap::new(), &wd);
        let err = resolve_seeds(&bp, &args, &wd).unwrap_err();
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
        let err = resolve_seeds(&bp, &args, &wd).unwrap_err();
        assert!(err.contains("rhai seed failed"), "got: {err}");
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
        let err = resolve_seeds(&bp, &args, &wd).unwrap_err();
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
        let err = resolve_seeds(&bp, &args, &wd).unwrap_err();
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
        let err = resolve_seeds(&req, &args, &wd).unwrap_err();
        assert!(err.contains("returned empty"), "got: {err}");
        // Optional + empty → region omitted (no error).
        let opt = bp(
            r#"scripted = { kind = "pinned", max_tokens = 500, seed = { rhai = "empty.rhai" } }"#,
        );
        let seeds = resolve_seeds(&opt, &args, &wd).unwrap();
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
        let seeds = resolve_seeds(&bp, &args, "/tmp").unwrap();
        assert_eq!(seeds.get("task").map(String::as_str), Some("t"));
        assert!(!seeds.contains_key("ghost"));
    }
}
