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
    // The manifest loader already refused a cap that does not parse, so an
    // error here can only come from a `ModelConfig` built in code; it is
    // treated as "no cap" the way an absent one is.
    let max_output_tokens = stage.model.output_cap().ok().flatten();
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
        context_hide: stage.context_hide.clone(),
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
        SeededSpawn {
            agent_id,
            blueprint,
            seeds,
            stages,
            global_hints,
            global_nudge: leviath_core::NudgeConfig::default(),
            region_scripts: std::collections::HashMap::new(),
        },
    )
}

/// Everything a seeded spawn needs besides the world it spawns into.
///
/// The blueprint and its resolved stages travel with the seeds and the global
/// defaults because all six are the same decision made at different layers:
/// what this agent starts with. The caller resolves them; this consumes them.
pub struct SeededSpawn {
    /// The run id this agent is registered under.
    pub agent_id: String,
    /// The blueprint being spawned.
    pub blueprint: leviath_core::Blueprint,
    /// Content for named caller-input regions, keyed by region name.
    pub seeds: std::collections::HashMap<String, String>,
    /// The blueprint's stages, already resolved against the provider registry.
    pub stages: Vec<ResolvedStage>,
    /// Config-level prompt hints, applied where the blueprint says nothing.
    pub global_hints: leviath_core::config::PromptHints,
    /// The config-level nudge, likewise.
    pub global_nudge: leviath_core::NudgeConfig,
    /// Compiled render hooks, keyed by region name.
    pub region_scripts: std::collections::HashMap<
        String,
        std::sync::Arc<leviath_scripting::region_hook::RegionScript>,
    >,
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
pub fn spawn_agent_seeded(world: &mut World, spawn: SeededSpawn) -> Result<Entity, String> {
    let SeededSpawn {
        agent_id,
        mut blueprint,
        seeds,
        stages,
        global_hints,
        global_nudge,
        region_scripts,
    } = spawn;
    let seeds = &seeds;
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
    // Before stage 0's prompt is injected, so it has somewhere of its own to go
    // rather than being charged to whichever pinned region came first.
    let prompts: Vec<Option<String>> = setups.iter().map(|s| s.system_prompt.clone()).collect();
    crate::context_setup::ensure_stage_instructions_region(&mut window, &prompts);
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
    let mut ledger = StageLedger(
        blueprint
            .stages
            .iter()
            .enumerate()
            .map(|(i, s)| leviath_core::run_meta::StageRecord::new(s.name.clone(), i))
            .collect(),
    );
    // Stage 0 is the one stage no transition enters, so its first visit is
    // opened here for the same reason its `VisitCounts` entry is pre-counted
    // above: without it the two disagree from the first tick.
    ledger.0[0].begin_visit(chrono::Utc::now().timestamp());

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

#[cfg(test)]
mod stage_instructions_fit_tests {
    //! The reported spawn failure, reproduced end to end.

    /// A layout shaped like the reported blueprint: a small `task` region and a
    /// dedicated `stage_instructions` region with room for a stage prompt.
    fn layout(window: usize) -> leviath_core::layout::ContextLayout {
        use leviath_core::layout::{BudgetSpec, ContextLayout, RegionDefinition};
        let pct = |p: f64| BudgetSpec::Percent {
            percent: p,
            min: None,
            max: None,
        };
        let mut task =
            RegionDefinition::new("task".to_string(), leviath_core::RegionKind::Pinned, 0);
        task.budget = pct(0.02);
        let mut instr = RegionDefinition::new(
            leviath_core::layout::STAGE_INSTRUCTIONS_REGION.to_string(),
            leviath_core::RegionKind::Pinned,
            0,
        );
        instr.budget = pct(0.03);
        ContextLayout::new(vec![task, instr], window).resolved(window)
    }

    /// A ~2.9k-token stage prompt: too big for 2% of a 128k window, comfortable
    /// in 3%.
    fn big_prompt() -> String {
        "word ".repeat(2_600)
    }

    #[test]
    fn a_stage_prompt_measured_at_spawn_uses_the_declared_region() {
        let window_tokens = 128_000;
        let layout = layout(window_tokens);
        let task_max = layout
            .regions
            .iter()
            .find(|r| r.name == "task")
            .expect("task")
            .max_tokens;
        let instr_max = layout
            .regions
            .iter()
            .find(|r| r.name == leviath_core::layout::STAGE_INSTRUCTIONS_REGION)
            .expect("stage_instructions")
            .max_tokens;
        let prompt = big_prompt();
        let tokens = leviath_core::estimate_tokens(&format!("[Stage instructions: {prompt}]"));
        assert!(
            tokens > task_max && tokens < instr_max,
            "the fixture must reproduce the reported shape: {tokens} vs task {task_max} / \
             stage_instructions {instr_max}"
        );

        let bp = leviath_core::Blueprint::new(
            "t".to_string(),
            "d".to_string(),
            vec![leviath_core::Stage::new(
                "work".to_string(),
                leviath_core::blueprint::ModelConfig::new("p".to_string(), "m".to_string()),
            )],
            layout,
        );
        let mut window = crate::components::ContextWindow::new(window_tokens);
        crate::context_setup::init_window_seeded(
            &mut window,
            &bp,
            &std::collections::HashMap::new(),
        );
        let setup = crate::pipeline::transition::StageSetup {
            inference_config: crate::components::InferenceConfig {
                temperature: None,
                max_output_tokens: None,
                extra_params: Default::default(),
                batch_tool_hint: false,
                shell_hint: false,
                request_timeout_secs: None,
            },
            routing: None,
            accepts_messages: true,
            context_layout: None,
            context_hide: Vec::new(),
            system_prompt: Some(prompt),
            output: None,
        };
        crate::pipeline::transition::apply_stage_context(&setup, &mut window)
            .expect("the prompt fits the region declared for it");

        let instr = window
            .get_region(leviath_core::layout::STAGE_INSTRUCTIONS_REGION)
            .expect("region exists");
        assert!(
            instr.content.iter().any(|e| e.content.contains("word")),
            "the prompt landed in stage_instructions"
        );
    }

    /// The reported failure itself: a blueprint that declares no
    /// `stage_instructions` region at all.
    ///
    /// Its prompt went to `task` - the first pinned region, sized for a sentence
    /// from the caller - and on a small window the spawn died with
    /// `stage system prompt does not fit region 'task'`. The workaround was to
    /// floor every task region with a `min_tokens` sized for the largest stage
    /// prompt, coupling an unrelated region to prompt lengths.
    #[test]
    fn a_blueprint_that_declares_no_region_still_gets_one() {
        use leviath_core::layout::{BudgetSpec, ContextLayout, RegionDefinition};
        let window_tokens = 128_000;
        let prompt = big_prompt();

        // Only `task`, at 2% - exactly the reported declaration.
        let mut task =
            RegionDefinition::new("task".to_string(), leviath_core::RegionKind::Pinned, 0);
        task.budget = BudgetSpec::Percent {
            percent: 0.02,
            min: None,
            max: None,
        };
        let only_task = ContextLayout::new(vec![task], window_tokens).resolved(window_tokens);
        let bp = leviath_core::Blueprint::new(
            "t".to_string(),
            "d".to_string(),
            vec![leviath_core::Stage::new(
                "work".to_string(),
                leviath_core::blueprint::ModelConfig::new("p".to_string(), "m".to_string()),
            )],
            only_task,
        );

        let mut window = crate::components::ContextWindow::new(window_tokens);
        crate::context_setup::init_window_seeded(
            &mut window,
            &bp,
            &std::collections::HashMap::new(),
        );
        let prompts = vec![Some(prompt.clone())];
        crate::context_setup::ensure_stage_instructions_region(&mut window, &prompts);

        let setup = crate::pipeline::transition::StageSetup {
            inference_config: crate::components::InferenceConfig {
                temperature: None,
                max_output_tokens: None,
                extra_params: Default::default(),
                batch_tool_hint: false,
                shell_hint: false,
                request_timeout_secs: None,
            },
            routing: None,
            accepts_messages: true,
            context_layout: None,
            context_hide: Vec::new(),
            system_prompt: Some(prompt),
            output: None,
        };
        crate::pipeline::transition::apply_stage_context(&setup, &mut window)
            .expect("the prompt no longer has to fit the caller's task region");

        let task_region = window.get_region("task").expect("task");
        assert!(
            task_region.content.is_empty(),
            "the task region is left for the caller's task"
        );
        let instr = window
            .get_region(leviath_core::layout::STAGE_INSTRUCTIONS_REGION)
            .expect("the runtime made one");
        assert!(instr.content.iter().any(|e| e.content.contains("word")));
    }

    /// Nothing to hold means no region: an empty pinned region is budget taken
    /// from the work for nothing.
    #[test]
    fn no_region_is_made_when_no_stage_has_a_prompt() {
        let mut window = crate::components::ContextWindow::new(1_000);
        crate::context_setup::ensure_stage_instructions_region(&mut window, &[None, None]);
        assert!(
            window
                .get_region(leviath_core::layout::STAGE_INSTRUCTIONS_REGION)
                .is_none()
        );
    }

    /// A declared region is left exactly as the author sized it.
    #[test]
    fn a_declared_region_is_not_resized() {
        let mut window = crate::components::ContextWindow::new(100_000);
        window.add_region(leviath_core::Region::new(
            leviath_core::layout::STAGE_INSTRUCTIONS_REGION.to_string(),
            leviath_core::RegionKind::Pinned,
            4_242,
        ));
        crate::context_setup::ensure_stage_instructions_region(&mut window, &[Some(big_prompt())]);
        assert_eq!(
            window
                .get_region(leviath_core::layout::STAGE_INSTRUCTIONS_REGION)
                .expect("declared")
                .max_tokens,
            4_242
        );
    }

    /// A prompt bigger than the window is still a spawn failure - it was always
    /// going to be. What changes is that the message names the region the prompt
    /// was going to, rather than the caller's task region.
    #[test]
    fn an_impossible_prompt_is_still_refused_and_names_the_right_region() {
        let mut window = crate::components::ContextWindow::new(1_000);
        window.add_region(leviath_core::Region::new(
            "task".to_string(),
            leviath_core::RegionKind::Pinned,
            40,
        ));
        let prompt = "z".repeat(100_000);
        crate::context_setup::ensure_stage_instructions_region(
            &mut window,
            &[Some(prompt.clone())],
        );
        // Capped at a quarter of the window rather than sized to the prompt.
        assert_eq!(
            window
                .get_region(leviath_core::layout::STAGE_INSTRUCTIONS_REGION)
                .expect("made")
                .max_tokens,
            250
        );

        let setup = crate::pipeline::transition::StageSetup {
            inference_config: crate::components::InferenceConfig {
                temperature: None,
                max_output_tokens: None,
                extra_params: Default::default(),
                batch_tool_hint: false,
                shell_hint: false,
                request_timeout_secs: None,
            },
            routing: None,
            accepts_messages: true,
            context_layout: None,
            context_hide: Vec::new(),
            system_prompt: Some(prompt),
            output: None,
        };
        let err = crate::pipeline::transition::apply_stage_context(&setup, &mut window)
            .expect_err("a prompt larger than the window cannot be housed");
        assert!(
            err.contains(leviath_core::layout::STAGE_INSTRUCTIONS_REGION),
            "{err}"
        );
    }

    /// Sized for the largest prompt in the blueprint, not the first stage's:
    /// every stage's instructions pass through the same region.
    #[test]
    fn the_region_is_sized_for_the_widest_prompt() {
        let mut window = crate::components::ContextWindow::new(100_000);
        let small = "word ".repeat(10);
        let large = big_prompt();
        let expected = leviath_core::estimate_tokens(&format!("[Stage instructions: {large}]"));
        crate::context_setup::ensure_stage_instructions_region(
            &mut window,
            &[Some(small), Some(large)],
        );
        assert_eq!(
            window
                .get_region(leviath_core::layout::STAGE_INSTRUCTIONS_REGION)
                .expect("made")
                .max_tokens,
            expected
        );
    }

    /// The reported shape: the stage carries its own `[context.regions]`, which
    /// does not re-declare `stage_instructions`.
    #[test]
    fn a_scoped_stage_layout_still_routes_to_the_declared_region() {
        use leviath_core::layout::{BudgetSpec, ContextLayout, RegionDefinition};
        let window_tokens = 128_000;
        let prompt = big_prompt();

        // The stage narrows what it attends to and says nothing about
        // stage_instructions - the region is the runtime's to fill.
        let mut scoped_task =
            RegionDefinition::new("task".to_string(), leviath_core::RegionKind::Pinned, 0);
        scoped_task.budget = BudgetSpec::Percent {
            percent: 0.02,
            min: None,
            max: None,
        };
        let scoped = ContextLayout::new(vec![scoped_task], window_tokens).resolved(window_tokens);

        let bp = leviath_core::Blueprint::new(
            "t".to_string(),
            "d".to_string(),
            vec![leviath_core::Stage::new(
                "work".to_string(),
                leviath_core::blueprint::ModelConfig::new("p".to_string(), "m".to_string()),
            )],
            layout(window_tokens),
        );

        let mut window = crate::components::ContextWindow::new(window_tokens);
        crate::context_setup::init_window_seeded(
            &mut window,
            &bp,
            &std::collections::HashMap::new(),
        );
        let setup = crate::pipeline::transition::StageSetup {
            inference_config: crate::components::InferenceConfig {
                temperature: None,
                max_output_tokens: None,
                extra_params: Default::default(),
                batch_tool_hint: false,
                shell_hint: false,
                request_timeout_secs: None,
            },
            routing: None,
            accepts_messages: true,
            context_layout: Some(scoped),
            context_hide: Vec::new(),
            system_prompt: Some(prompt),
            output: None,
        };
        crate::pipeline::transition::apply_stage_context(&setup, &mut window)
            .expect("the prompt fits the region declared for it");

        let instr = window
            .get_region(leviath_core::layout::STAGE_INSTRUCTIONS_REGION)
            .expect("carried through the scoped layout");
        assert!(
            instr.content.iter().any(|e| e.content.contains("word")),
            "the prompt landed in stage_instructions, not in the scoped task region"
        );
    }
}
