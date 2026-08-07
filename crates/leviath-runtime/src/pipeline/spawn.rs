//! Spawning an agent: the caller-resolved per-stage inputs
//! ([`ResolvedStage`]), the per-stage setup derived from them, and the two
//! spawn entry points.

use super::*;

/// A blueprint stage resolved to a concrete provider, model, and effective tool
/// set - the per-stage input to [`spawn_agent`]. The caller (CLI / daemon) owns
/// the model-selection policy (overrides, availability, user defaults) and tool
/// filtering; the runtime just turns the result into agent data.
#[derive(Debug)]
pub struct ResolvedStage {
    /// The provider to call for this stage.
    pub provider_name: String,
    /// The resolved model name.
    pub model: String,
    /// The effective tool set for this stage (already filtered).
    pub tools: Vec<Tool>,
    /// Where to go if `provider_name` turns out to be unusable, best first.
    /// See [`crate::pipeline::resolve_stage_candidates`].
    pub fallbacks: Vec<leviath_core::blueprint::ModelEntry>,
    /// The output shape resolved for this stage: the blueprint's default, the
    /// stage's override, and the launching caller's request, combined. Resolved
    /// caller-side (like the model and tool choices beside it) because only the
    /// caller knows what was asked for at launch.
    pub output: Option<leviath_core::output::OutputSpec>,
}

/// Fallback context window used when a stage's provider isn't registered (so
/// percentage budgets can't be resolved against a real model). Matches
/// [`leviath_providers::ModelCapabilities`]'s default `max_context_tokens`.
pub(crate) const DEFAULT_CONTEXT_WINDOW_TOKENS: usize = 8192;

/// Look up a model's context window (for resolving percentage region budgets)
/// via the registered [`Providers`]. Falls back to
/// [`DEFAULT_CONTEXT_WINDOW_TOKENS`] with a warning when the provider isn't
/// registered - non-fatal, and `min_tokens` floors still protect regions.
pub(crate) fn context_window_tokens(world: &World, provider_name: &str, model: &str) -> usize {
    match world
        .get_resource::<Providers>()
        .and_then(|p| p.0.get(provider_name))
    {
        Some(provider) => provider.max_context_tokens(model),
        None => {
            tracing::warn!(
                provider = provider_name,
                model,
                "provider not registered; using default context window for percentage budgets"
            );
            DEFAULT_CONTEXT_WINDOW_TOKENS
        }
    }
}

/// Build a stage's [`StageSetup`] from its blueprint definition: inference config
/// (from the model parameters), tool-result routing, accepts-messages, layout,
/// and system prompt.
///
/// `global_hints` is the caller's config-level toggle for each system-prompt
/// hint; `agent_hints` the blueprint's agent-level override of the same. Each
/// one cascades stage → agent → global here.
pub(crate) fn stage_setup_from(
    stage: &leviath_core::Stage,
    global_hints: leviath_core::config::PromptHints,
    agent_hints: leviath_core::config::PromptHintOverrides,
    output: Option<leviath_core::output::OutputSpec>,
) -> StageSetup {
    let temperature = stage
        .model
        .parameters
        .get("temperature")
        .and_then(|v| v.as_f64())
        .map(|t| t as f32);
    // Every other model parameter (top_p, stop, seed, frequency_penalty, …) is
    // passed through to the provider verbatim; only temperature/max_output_tokens
    // are consumed specially above.
    let extra_params: serde_json::Map<String, serde_json::Value> = stage
        .model
        .parameters
        .iter()
        .filter(|(k, _)| k.as_str() != "temperature" && k.as_str() != "max_output_tokens")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let max_output_tokens = stage
        .model
        .parameters
        .get("max_output_tokens")
        .and_then(|v| v.as_u64())
        .map(|t| t as usize);
    let base_prompt = stage
        .config
        .get("system_prompt")
        .and_then(|v| v.as_str())
        .map(String::from);
    // A fan-out stage's single inference IS the "split": fold its `split_prompt`
    // (which asks for the JSON array of work items) onto any base instructions so
    // the stage's normal inference produces the work items the split system parses.
    let system_prompt = match &stage.mode {
        leviath_core::blueprint::StageMode::FanOut { config }
            if !config.split_prompt.trim().is_empty() =>
        {
            Some(match base_prompt {
                Some(base) => format!("{base}\n\n{}", config.split_prompt),
                None => config.split_prompt.clone(),
            })
        }
        _ => base_prompt,
    };
    // A stage that must hand something back says so in its own instructions, on
    // top of the `submit_output` tool description carrying the same shape. Both,
    // because a format the model has no prior knowledge of - a2ui, a house
    // schema - is exactly the case where one mention is easy to miss, and there
    // is no parser downstream to catch a near miss.
    let system_prompt = match (&output, stage.require_output) {
        (Some(spec), true) => {
            let described = leviath_core::describe_spec(spec);
            let demand = match described.is_empty() {
                true => format!(
                    "Before this stage ends you must call `{tool}` with your final answer. It is \
                     the only thing the caller receives.",
                    tool = leviath_core::blueprint::SUBMIT_OUTPUT_TOOL
                ),
                false => format!(
                    "Before this stage ends you must call `{tool}` with your final answer. It is \
                     the only thing the caller receives.\n\n{described}",
                    tool = leviath_core::blueprint::SUBMIT_OUTPUT_TOOL
                ),
            };
            Some(match system_prompt {
                Some(base) => format!("{base}\n\n{demand}"),
                None => demand,
            })
        }
        _ => system_prompt,
    };
    // Cascade each hint toggle: stage > agent > global (both default on).
    let batch_tool_hint = leviath_core::taint::resolve_batch_tool_hint(
        global_hints.batch_tool,
        agent_hints.batch_tool,
        stage.batch_tool_hint,
    );
    let shell_hint = leviath_core::taint::resolve_shell_hint(
        global_hints.shell,
        agent_hints.shell,
        stage.shell_hint,
    );
    StageSetup {
        inference_config: InferenceConfig {
            temperature,
            max_output_tokens,
            extra_params,
            batch_tool_hint,
            shell_hint,
            request_timeout_secs: stage.model.request_timeout_secs,
        },
        routing: stage.tool_result_routing.clone(),
        accepts_messages: stage.accepts_messages,
        context_layout: stage.context_layout.clone(),
        system_prompt,
        output,
    }
}

/// Spawn a fully-formed agent into `world` from its blueprint, task, and
/// per-stage resolution, and return its entity. Builds every stage's
/// `StageInference`/`StageSetup` up front (so transitions are pure component
/// swaps), seeds the context window, applies the **first** stage's setup (its
/// layout and system prompt), pre-counts the first stage's visit, and marks the
/// agent `ReadyToInfer`. Returns `Err` if the first stage's system prompt doesn't fit
/// its region (the same hard failure the imperative loop raises at stage 0).
///
/// `stages` must be aligned with `blueprint.stages` (one [`ResolvedStage`] each).
///
/// `global_hints` is the caller's global config toggle for each system-prompt
/// hint; each is resolved per stage against the blueprint's agent-level and
/// per-stage override of the same name.
pub fn spawn_agent(
    world: &mut World,
    agent_id: String,
    blueprint: leviath_core::Blueprint,
    task: &str,
    stages: Vec<ResolvedStage>,
    global_hints: leviath_core::config::PromptHints,
) -> Result<Entity, String> {
    let seeds = std::collections::HashMap::from([("task".to_string(), task.to_string())]);
    // No compiled custom-region scripts on this path: script-backed regions
    // require the seeded spawn (the CLI resolves and compiles them). A custom
    // region spawned through here renders its fallback shape. Global nudge
    // defaults are likewise a seeded-spawn concern (the CLI reads them from
    // config.toml); agents spawned through here cascade straight from the
    // blueprint to the built-in defaults.
    spawn_agent_seeded(
        world,
        agent_id,
        blueprint,
        &seeds,
        stages,
        global_hints,
        leviath_core::NudgeConfig::default(),
        std::collections::HashMap::new(),
    )
}

/// Like [`spawn_agent`], but seeds the context window from a name→content map
/// (caller-input regions filled by the CLI/ACP/API, plus blueprint-resolved
/// seeds) rather than a single task string. `spawn_agent` is the thin wrapper
/// that seeds only the `task` key.
///
/// `global_nudge` is the caller's config-level `[nudge]` defaults, captured on
/// the agent as a [`crate::pipeline::response::GlobalNudge`] component; each
/// field is resolved per stage against the blueprint's agent-level and
/// per-stage nudge settings when an empty response is handled.
#[allow(clippy::too_many_arguments)]
pub fn spawn_agent_seeded(
    world: &mut World,
    agent_id: String,
    mut blueprint: leviath_core::Blueprint,
    seeds: &std::collections::HashMap<String, String>,
    stages: Vec<ResolvedStage>,
    global_hints: leviath_core::config::PromptHints,
    global_nudge: leviath_core::NudgeConfig,
    region_scripts: std::collections::HashMap<
        String,
        std::sync::Arc<leviath_scripting::region_hook::RegionScript>,
    >,
) -> Result<Entity, String> {
    // Resolve any percentage region budgets against each stage's model context
    // window (the only place the model - and hence the window - is known). The
    // global layout resolves against the entry stage (stage 0); each per-stage
    // layout resolves against that stage's own model. Absolute layouts resolve to
    // themselves, so this is a no-op for legacy blueprints.
    let stage_windows: Vec<usize> = stages
        .iter()
        .map(|rs| context_window_tokens(world, &rs.provider_name, &rs.model))
        .collect();
    blueprint.context_layout = blueprint.context_layout.resolved(stage_windows[0]);
    for (i, stage) in blueprint.stages.iter_mut().enumerate() {
        if let Some(layout) = &stage.context_layout {
            stage.context_layout = Some(layout.resolved(stage_windows[i]));
        }
    }
    // Validate the resolved (fully-absolute) layouts, now that percentages are
    // concrete numbers judged against the real model window.
    blueprint
        .context_layout
        .validate()
        .map_err(|e| e.to_string())?;
    for stage in &blueprint.stages {
        if let Some(layout) = &stage.context_layout {
            layout.validate().map_err(|e| e.to_string())?;
        }
    }

    // Kept before `stages` is consumed, so each stage's setup can fold the same
    // shape into its system prompt that its tool description already carries.
    let stage_outputs: Vec<Option<leviath_core::output::OutputSpec>> =
        stages.iter().map(|rs| rs.output.clone()).collect();
    let stage_infs: Vec<StageInference> = stages
        .into_iter()
        .map(|rs| StageInference {
            provider_name: rs.provider_name,
            model: rs.model,
            tools: rs.tools,
            tool_filter: None, // tools already resolved to the effective set
            fallbacks: rs.fallbacks,
            output: rs.output,
        })
        .collect();
    let agent_hints = leviath_core::config::PromptHintOverrides {
        batch_tool: blueprint.batch_tool_hint,
        shell: blueprint.shell_hint,
    };
    let setups: Vec<StageSetup> = blueprint
        .stages
        .iter()
        .zip(stage_outputs)
        .map(|(s, output)| stage_setup_from(s, global_hints, agent_hints, output))
        .collect();

    // Seed the window from the blueprint layout + task, then apply stage 0's
    // context setup (layout swap + system-prompt injection) just as entering any
    // later stage would.
    let mut window = ContextWindow::new(blueprint.context_layout.total_budget_tokens);
    // Attach compiled custom-region scripts BEFORE seeding, so seed writes
    // pass through each region's on_write hook like any other entry.
    window.region_scripts = region_scripts;
    crate::context_setup::init_window_seeded(&mut window, &blueprint, seeds);
    apply_stage_context(&setups[0], &mut window)?;

    let stage0_name = blueprint.stages[0].name.clone();
    let stage0_inf = stage_infs[0].clone();
    let setup0 = &setups[0];
    let stage0_cfg = setup0.inference_config.clone();
    let stage0_routing = setup0.routing.clone();
    let accepts_messages = setup0.accepts_messages;

    // Pre-count stage 0's visit: the imperative loop bumps a stage's visit after
    // it runs and before resolving its transition, so stage 0 must read as
    // visited once by the time its first transition resolves.
    let mut visits = VisitCounts::default();
    *visits.0.entry(stage0_name.clone()).or_insert(0) += 1;

    // Seed the per-stage ledger (names + Pending) so the dashboard shows every
    // stage's real name from the first persist, not just the active one.
    let ledger = StageLedger(
        blueprint
            .stages
            .iter()
            .enumerate()
            .map(|(i, s)| leviath_core::run_meta::StageRecord::new(s.name.clone(), i))
            .collect(),
    );

    // Repetition detection is opt-in per blueprint.
    let repetition = blueprint
        .repetition_detection
        .as_ref()
        .map(crate::repetition::RepetitionDetector::from_detection_config);

    let entity = world
        .spawn((
            AgentBlueprint(blueprint),
            AgentState {
                agent_id,
                current_stage: stage0_name,
                iteration: 0,
                status: AgentStatus::Active,
                spawned_children_ids: vec![],
                pending_wait: None,
                accepts_messages,
            },
            MessageInbox::default(),
            StageCursor { index: 0 },
            StageProgress::default(),
            StageInferences(stage_infs),
            StageSetups(setups),
            visits,
            window,
            stage0_inf,
            stage0_cfg,
            ReadyToInfer,
        ))
        .id();
    // Inserted after spawn: the bundle above is already at bevy's 15-tuple limit.
    world.entity_mut(entity).insert((
        ledger,
        StageIoBuffer::default(),
        crate::pipeline::response::GlobalNudge(global_nudge),
    ));
    if let Some(detector) = repetition {
        world.entity_mut(entity).insert(detector);
    }
    if let Some(routing) = stage0_routing {
        world
            .entity_mut(entity)
            .insert(crate::components::ToolResultRoutingComponent { routing });
    }
    Ok(entity)
}
