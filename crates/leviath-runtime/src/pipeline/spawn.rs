//! Spawning an agent: the caller-resolved per-stage inputs
//! ([`ResolvedStage`]), the per-stage setup derived from them, and the two
//! spawn entry points.

use super::*;

/// A blueprint stage resolved to a concrete provider, model, and effective tool
/// set - the per-stage input to `spawn_agent`. The caller (CLI / daemon) owns
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
    /// See `crate::pipeline::resolve_stage_candidates`.
    pub fallbacks: Vec<crate::spec::blueprint::ModelEntry>,
    /// The output shape resolved for this stage: the blueprint's default, the
    /// stage's override, and the launching caller's request, combined. Resolved
    /// caller-side (like the model and tool choices beside it) because only the
    /// caller knows what was asked for at launch.
    pub output: Option<leviath_core::output::OutputSpec>,
    /// Operational lines to log for this stage at spawn: today, one per stage
    /// whose head the user's `override_model` or `fallback_model` moved off
    /// the blueprint's own choice. Empty when the blueprint's choice stands.
    pub notes: Vec<String>,
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

/// Spawn a fully-formed agent into `world` from its blueprint, task, and
/// per-stage resolution, and return its entity: the task seeds the `task`
/// region and everything else is as [`spawn_agent_seeded`] does it.
///
/// `stages` must be aligned with `blueprint.stages` (one [`ResolvedStage`] each).
///
/// `global_hints` is the caller's global config toggle for each system-prompt
/// hint; each is resolved per stage against the blueprint's agent-level and
/// per-stage override of the same name.
#[cfg(test)]
pub(crate) fn spawn_agent(
    world: &mut World,
    agent_id: String,
    blueprint: crate::spec::Blueprint,
    task: &str,
    stages: Vec<ResolvedStage>,
    global_hints: leviath_core::config::PromptHints,
) -> Result<Entity, String> {
    let seeds = std::collections::HashMap::from([("task".to_string(), task.to_string())]);
    spawn_agent_seeded(
        world,
        SeededSpawn {
            agent_id,
            blueprint,
            seeds,
            parts: Vec::new(),
            stages,
            global_hints,
            global_nudge: crate::spec::NudgeConfig::default(),
            region_scripts: std::collections::HashMap::new(),
            mime_registry: None,
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
    pub blueprint: crate::spec::Blueprint,
    /// Content for named caller-input regions, keyed by region name.
    pub seeds: std::collections::HashMap<String, String>,
    /// Files the caller attached, written into their regions as stored parts
    /// once the seeds are in.
    pub parts: Vec<leviath_core::mime::InboundPart>,
    /// The blueprint's stages, already resolved against the provider registry.
    pub stages: Vec<ResolvedStage>,
    /// Config-level prompt hints, applied where the blueprint says nothing.
    pub global_hints: leviath_core::config::PromptHints,
    /// The config-level nudge, likewise.
    pub global_nudge: crate::spec::NudgeConfig,
    /// Compiled render hooks, keyed by region name.
    pub region_scripts: std::collections::HashMap<
        String,
        std::sync::Arc<leviath_scripting::region_hook::RegionScript>,
    >,
    /// The run's mime registry, when the host built one (with the
    /// blueprint's checks compiled and attached). Left `None`, one is built
    /// here from the world's registry and the blueprint's own rows, with no
    /// checks.
    pub mime_registry: Option<crate::blob_store::RunMimeRegistry>,
}

/// A run spec for a parsed blueprint and the inference each of its stages
/// resolved to, for spawns that start from a blueprint and a stage list
/// rather than a spawn request.
///
/// The graph is the blueprint read as a [`RunGraph`](crate::spec::graph::RunGraph);
/// each stage's plan is its [`StageInference`] with a zero context window and
/// no region budgets, which the caller fills in when it knows them. The spec
/// launches attended, from no workdir, with nothing seeded.
pub fn run_spec_from_blueprint(
    bp: &crate::spec::Blueprint,
    agent_id: &str,
    stages: &[StageInference],
) -> Result<crate::spec::run_spec::RunSpec, String> {
    use crate::spec::names::{BlueprintName, BlueprintRef};
    use crate::spec::run_spec::{RunSpec, SpecOrigin};
    let graph =
        crate::spec::graph::RunGraph::from_blueprint(bp).map_err(|issues| issues.to_string())?;
    let plans = graph
        .stages
        .iter()
        .zip(stages)
        .map(|(stage, si)| stage_plan(stage.name.clone(), si))
        .collect::<Result<Vec<_>, String>>()?;
    Ok(RunSpec {
        run_id: named(agent_id)?,
        origin: match BlueprintName::new(bp.name.as_str()) {
            Ok(name) => SpecOrigin::Blueprint {
                blueprint: BlueprintRef { name, digest: None },
                version: bp.version.clone(),
            },
            Err(_) => SpecOrigin::Raw,
        },
        graph,
        inputs: Default::default(),
        stages: plans,
        seeded: Default::default(),
        code: Vec::new(),
        requested_output: None,
        requested_model: None,
        launch: crate::spec::launch::LaunchPolicy {
            unattended: crate::spec::launch::Unattended::Off,
            allow: Vec::new(),
            max_depth: 0,
            seed_commands: true,
            capture_model_input: false,
        },
        auto_answers: Default::default(),
        placement: crate::spec::launch::Placement {
            workdir: std::path::PathBuf::new(),
            parent: None,
            depth: 0,
            worker_stage: None,
        },
        delivery: Default::default(),
        env: Default::default(),
        created_at: chrono::Utc::now().timestamp(),
    })
}

/// A checked name, or why the text is not one.
fn named<T: std::str::FromStr<Err = crate::spec::names::NameError>>(
    text: &str,
) -> Result<T, String> {
    text.parse()
        .map_err(|e: crate::spec::names::NameError| e.to_string())
}

/// A stage's plan from the inference it resolved to. A fallback with no
/// provider leaves the provider to the operator's order.
fn stage_plan(
    stage: crate::spec::names::StageName,
    si: &StageInference,
) -> Result<crate::spec::run_spec::StagePlan, String> {
    use crate::spec::names::ModelRef;
    use crate::spec::run_spec::{StagePlan, ToolDef, ToolSource};
    let fallbacks = si
        .fallbacks
        .iter()
        .map(|f| {
            let provider = (!f.provider.is_empty())
                .then(|| named(&f.provider))
                .transpose()?;
            Ok(ModelRef {
                provider,
                model: named(&f.model)?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let tools = si
        .tools
        .iter()
        .map(|t| {
            Ok(ToolDef {
                name: named(&t.name)?,
                description: t.description.clone(),
                schema: leviath_core::JsonDoc::new(t.parameters.clone()),
                source: ToolSource::Builtin,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(StagePlan {
        stage,
        provider: named(&si.provider_name)?,
        model: named(&si.model)?,
        context_window: 0,
        max_output_tokens: None,
        fallbacks,
        tools,
        output: si.output.as_ref().map(output_def).transpose()?,
        region_budgets: Default::default(),
        notes: Vec::new(),
    })
}

/// An output shape as a run graph writes it.
fn output_def(
    spec: &leviath_core::output::OutputSpec,
) -> Result<crate::spec::graph::OutputDef, String> {
    use crate::spec::graph::{ArtifactDef, CodeRef, OutputDef};
    Ok(OutputDef {
        format: spec.format.clone(),
        instructions: spec.instructions.clone(),
        example: spec.example.clone(),
        schema: spec.schema.clone().map(leviath_core::JsonDoc::new),
        validator: spec.validator.clone().map(CodeRef::File),
        on_validator_error: spec.on_validator_error,
        overwrite_artifacts: spec.overwrite_artifacts,
        artifacts: spec
            .artifacts
            .iter()
            .map(|a| {
                Ok(ArtifactDef {
                    name: a.name.clone(),
                    mime_type: named(&a.mime_type)?,

                    required: a.required,
                    description: a.description.clone(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?,
    })
}

/// The config-level defaults a blueprint spawn is resolved against, folded
/// into the graph so the run's spec carries them: each hint and nudge setting
/// the blueprint leaves open takes the operator's.
fn fold_operator_defaults(
    graph: &mut crate::spec::graph::RunGraph,
    hints: leviath_core::config::PromptHints,
    nudge: &crate::spec::NudgeConfig,
) {
    graph.batch_tool_hint = Some(graph.batch_tool_hint.unwrap_or(hints.batch_tool));
    graph.shell_hint = Some(graph.shell_hint.unwrap_or(hints.shell));
    let own = graph.nudge.clone().unwrap_or_default();
    graph.nudge = Some(crate::spec::graph::NudgeDef {
        enabled: own.enabled.or(nudge.enabled),
        max: own
            .max
            .or_else(|| nudge.max.map(|m| u32::try_from(m).unwrap_or(u32::MAX))),
        text: own.text.or_else(|| nudge.text.clone()),
    });
}

/// The region a blueprint spawn's seed lands in: its own name, or for the
/// `task` seed, the pinned region named `task` or else the first pinned region.
fn seed_target(layout: &crate::spec::ContextLayout, key: &str) -> Option<String> {
    let pinned = |r: &&crate::spec::layout::RegionDefinition| {
        matches!(r.kind, leviath_core::RegionKind::Pinned)
    };
    let found = match key {
        "task" => layout
            .regions
            .iter()
            .filter(pinned)
            .find(|r| r.name == "task")
            .or_else(|| layout.regions.iter().find(pinned)),
        _ => layout.regions.iter().find(|r| r.name == key),
    };
    found.map(|r| r.name.clone())
}

/// Spawn an agent from a blueprint, seeds and resolved stages: build the run's
/// spec from them, lay out and seed its window, enter its first stage, and
/// [`insert`](crate::insert::insert) it. Returns `Err` when the blueprint does
/// not read as a run graph, when a layout does not fit its stage's window, or
/// when the first stage's system prompt does not fit its region.
///
/// Every percentage region budget is resolved here against the model windows
/// the providers report, and written into each stage's plan: a region of the
/// graph's layout is sized against the smallest window among the stages that
/// see it, and a stage's own layout against that stage's window.
///
/// `global_hints` and `global_nudge` are the caller's config-level defaults;
/// each setting the blueprint leaves open takes them.
pub fn spawn_agent_seeded(world: &mut World, spawn: SeededSpawn) -> Result<Entity, String> {
    let SeededSpawn {
        agent_id,
        blueprint,
        seeds,
        parts,
        stages,
        global_hints,
        global_nudge,
        region_scripts,
        mime_registry,
    } = spawn;
    // The registry this run types its bytes by: the host's, or the world's
    // rows with the blueprint's `[mime_types]` on top. A world without a
    // registry (one assembled by hand in a test) has no run registry either.
    let run_registry = match mime_registry {
        Some(registry) => Some(registry),
        None => world
            .get_resource::<crate::blob_store::MimeRegistryHandle>()
            .map(|r| {
                crate::blob_store::RunMimeRegistry::new(
                    &r.0,
                    blueprint.mime_types.clone(),
                    std::collections::BTreeMap::new(),
                )
            })
            .transpose()
            .map_err(|e| format!("[mime_types]: {e}"))?,
    };
    // Where attached bytes go, read before the world is borrowed for the
    // spawn. A world without a store (one assembled by hand in a test)
    // refuses a part rather than dropping it on the floor.
    let mime_store = world
        .get_resource::<crate::blob_store::BlobStoreHandle>()
        .map(|s| s.0.clone())
        .zip(run_registry.as_ref().map(|r| r.registry()));
    let limits = world
        .get_resource::<crate::blob_store::MimeLimits>()
        .copied()
        .unwrap_or_default();
    // `parse_manifest` guarantees at least one stage, but this is `pub` and an
    // embedder can hand-build a `Blueprint`; refusing here turns index panics
    // into the `Err` the signature already promises.
    if blueprint.stages.is_empty() {
        return Err("blueprint declares no stages".to_string());
    }
    if stages.len() != blueprint.stages.len() {
        return Err(format!(
            "{} resolved stages for a blueprint with {}",
            stages.len(),
            blueprint.stages.len()
        ));
    }
    let stage_windows: Vec<usize> = stages
        .iter()
        .map(|rs| context_window_tokens(world, &rs.provider_name, &rs.model))
        .collect();
    let resolved = resolved_layouts(&blueprint, &stage_windows)?;

    let notes: Vec<Vec<String>> = stages.iter().map(|rs| rs.notes.clone()).collect();
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
    let mut spec = run_spec_from_blueprint(&blueprint, &agent_id, &stage_infs)?;
    fold_operator_defaults(&mut spec.graph, global_hints, &global_nudge);
    for (i, plan) in spec.stages.iter_mut().enumerate() {
        plan.context_window = u32::try_from(stage_windows[i]).unwrap_or(u32::MAX);
        plan.notes = notes[i].clone();
        plan.region_budgets = resolved.budgets(i);
    }
    for (key, content) in &seeds {
        // Unknown names are rejected upstream; a seed that targets nothing is
        // dropped here to keep this infallible for it.
        let Some(target) = seed_target(&resolved.global, key)
            .and_then(|t| crate::spec::names::RegionName::new(t).ok())
        else {
            continue;
        };
        spec.seeded.insert(
            target,
            crate::spec::run_spec::SeededContent {
                text: content.clone(),
                parts: Vec::new(),
            },
        );
    }
    let spec = std::sync::Arc::new(spec);

    let mut window = crate::insert::seeded_window(&spec, &region_scripts);
    if !parts.is_empty() {
        let Some((store, registry)) = mime_store else {
            return Err("this world has no blob store, so it cannot take an attached part".into());
        };
        crate::context_setup::ingest_parts_into(
            &mut window,
            crate::context_setup::task_region(&spec.graph.layout),
            parts,
            &crate::context_setup::PartSink {
                store: store.as_ref(),
                registry: &registry,
                run_id: &agent_id,
                max_part_bytes: limits.max_part_bytes,
                inline_text_bytes: limits.inline_text_bytes,
            },
        )?;
    }
    crate::insert::enter_first_stage(&spec, &mut window)?;
    let state = crate::insert::initial_state_from(&spec, &window);

    let mut bindings =
        crate::spec::env::Bindings::new().with(crate::insert::RegionScripts(region_scripts));
    if let Some(registry) = run_registry {
        bindings = bindings.with(registry);
    }
    let entity = crate::insert::insert(world, spec.clone(), bindings, &state);
    // The modules outside the pipeline that still read the parsed blueprint
    // and the per-stage lists find them here.
    let mut blueprint = blueprint;
    blueprint.context_layout = resolved.global;
    for (stage, own) in blueprint.stages.iter_mut().zip(resolved.per_stage) {
        stage.context_layout = own;
    }
    let setups = (0..spec.graph.stages.len())
        .map(|i| spec_view::stage_setup(&spec, i))
        .collect();
    world.entity_mut(entity).insert((
        AgentBlueprint(blueprint),
        StageInferences(stage_infs),
        StageSetups(setups),
    ));
    Ok(entity)
}

/// A blueprint's layouts with every percentage budget resolved to tokens.
struct ResolvedLayouts {
    /// The blueprint's own layout.
    global: crate::spec::ContextLayout,
    /// Each stage's own layout, when it has one.
    per_stage: Vec<Option<crate::spec::ContextLayout>>,
}

impl ResolvedLayouts {
    /// Each region's budget in stage `i`: the stage's own layout's, over the
    /// blueprint layout's for every region the stage's layout does not name.
    fn budgets(&self, i: usize) -> std::collections::BTreeMap<crate::spec::names::RegionName, u32> {
        let own = self.per_stage[i].iter().flat_map(|l| l.regions.iter());
        self.global
            .regions
            .iter()
            .chain(own)
            .filter_map(|r| {
                let tokens = u32::try_from(r.max_tokens).unwrap_or(u32::MAX);
                crate::spec::names::RegionName::new(r.name.as_str())
                    .ok()
                    .map(|name| (name, tokens))
            })
            .collect()
    }
}

/// Resolve a blueprint's layouts against its stages' model windows and check
/// each one, as [`spawn_agent_seeded`] describes.
fn resolved_layouts(
    blueprint: &crate::spec::Blueprint,
    stage_windows: &[usize],
) -> Result<ResolvedLayouts, String> {
    // A region's percentage budget is sized against the smallest context window
    // among the stages that actually see it - not the entry stage's window, and
    // not a stage that never reads the region. A per-stage layout's regions are
    // private to that stage, so its own window is the only one that uses them.
    let smallest_window_seeing = |region: &str| -> usize {
        blueprint
            .stages
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                s.context_layout.is_none() && blueprint.regions_visible_to(s).contains(region)
            })
            .map(|(i, _)| stage_windows[i])
            .min()
            // No stage uses the global layout for this region (every stage
            // has its own, or all hide it); the first window is a harmless
            // default for a budget nothing at runtime consults.
            .unwrap_or(stage_windows[0])
    };
    let global = blueprint
        .context_layout
        .resolved_per_region(&smallest_window_seeing);
    let per_stage: Vec<Option<crate::spec::ContextLayout>> = blueprint
        .stages
        .iter()
        .enumerate()
        .map(|(i, s)| {
            s.context_layout
                .as_ref()
                .map(|l| l.resolved(stage_windows[i]))
        })
        .collect();
    // Structural validation (duplicate names, eviction order, custom scripts)
    // once per distinct layout, now that percentages are concrete numbers.
    global.validate().map_err(|e| e.to_string())?;
    for layout in per_stage.iter().flatten() {
        layout.validate().map_err(|e| e.to_string())?;
    }
    // Then the working-room floor, per stage: each stage must keep enough
    // evictable room after its fixed regions, judged against *its* model window
    // over just the regions *it* sees - a region budgeted generously for a
    // wide-window stage must not be counted against a narrow one that never
    // reads it.
    for (i, stage) in blueprint.stages.iter().enumerate() {
        let layout = per_stage[i].as_ref().unwrap_or(&global);
        let visible = blueprint.regions_visible_to(stage);
        layout
            .retaining(|name| visible.contains(name))
            .validate_working_room(stage_windows[i])
            .map_err(|e| e.to_string())?;
    }
    Ok(ResolvedLayouts { global, per_stage })
}

#[cfg(test)]
mod stage_instructions_fit_tests {
    //! A stage prompt bigger than the first pinned region, spawned end to end.

    /// A small `task` region beside a dedicated `stage_instructions` region
    /// with room for a stage prompt.
    fn layout(window: usize) -> crate::spec::layout::ContextLayout {
        use crate::spec::layout::{BudgetSpec, ContextLayout, RegionDefinition};
        let pct = |p: f64| BudgetSpec::Percent {
            percent: p,
            min: None,
            max: None,
        };
        let mut task =
            RegionDefinition::new("task".to_string(), leviath_core::RegionKind::Pinned, 0);
        task.budget = pct(0.02);
        let mut instr = RegionDefinition::new(
            crate::spec::layout::STAGE_INSTRUCTIONS_REGION.to_string(),
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
            .find(|r| r.name == crate::spec::layout::STAGE_INSTRUCTIONS_REGION)
            .expect("stage_instructions")
            .max_tokens;
        let prompt = big_prompt();
        let tokens = leviath_core::estimate_tokens(&format!("[Stage instructions: {prompt}]"));
        assert!(
            tokens > task_max && tokens < instr_max,
            "the fixture must reproduce the reported shape: {tokens} vs task {task_max} / \
             stage_instructions {instr_max}"
        );

        let bp = crate::spec::Blueprint::new(
            "t".to_string(),
            "d".to_string(),
            vec![crate::spec::Stage::new(
                "work".to_string(),
                crate::spec::blueprint::ModelConfig::new("p".to_string(), "m".to_string()),
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
                as_text: Vec::new(),
            },
            routing: None,
            accepts_messages: true,
            context_layout: None,
            context_hide: Vec::new(),
            context_reset: Vec::new(),
            system_prompt: Some(prompt),
        };
        crate::pipeline::transition::apply_stage_context(&setup, &mut window)
            .expect("the prompt fits the region declared for it");

        let instr = window
            .get_region(crate::spec::layout::STAGE_INSTRUCTIONS_REGION)
            .expect("region exists");
        assert!(
            instr.content.iter().any(|e| e.content.contains("word")),
            "the prompt landed in stage_instructions"
        );
    }

    /// A blueprint that declares no `stage_instructions` region at all.
    ///
    /// Without one the prompt goes to `task` - the first pinned region, sized
    /// for a sentence from the caller - and on a small window the spawn dies
    /// with `stage system prompt does not fit region 'task'`. The alternative
    /// is flooring every task region with a `min_tokens` sized for the largest
    /// stage prompt, coupling an unrelated region to prompt lengths.
    #[test]
    fn a_blueprint_that_declares_no_region_still_gets_one() {
        use crate::spec::layout::{BudgetSpec, ContextLayout, RegionDefinition};
        let window_tokens = 128_000;
        let prompt = big_prompt();

        // Only `task`, at 2% - a region sized for a sentence from the caller.
        let mut task =
            RegionDefinition::new("task".to_string(), leviath_core::RegionKind::Pinned, 0);
        task.budget = BudgetSpec::Percent {
            percent: 0.02,
            min: None,
            max: None,
        };
        let only_task = ContextLayout::new(vec![task], window_tokens).resolved(window_tokens);
        let bp = crate::spec::Blueprint::new(
            "t".to_string(),
            "d".to_string(),
            vec![crate::spec::Stage::new(
                "work".to_string(),
                crate::spec::blueprint::ModelConfig::new("p".to_string(), "m".to_string()),
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
                as_text: Vec::new(),
            },
            routing: None,
            accepts_messages: true,
            context_layout: None,
            context_hide: Vec::new(),
            context_reset: Vec::new(),
            system_prompt: Some(prompt),
        };
        crate::pipeline::transition::apply_stage_context(&setup, &mut window)
            .expect("the prompt no longer has to fit the caller's task region");

        let task_region = window.get_region("task").expect("task");
        assert!(
            task_region.content.is_empty(),
            "the task region is left for the caller's task"
        );
        let instr = window
            .get_region(crate::spec::layout::STAGE_INSTRUCTIONS_REGION)
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
                .get_region(crate::spec::layout::STAGE_INSTRUCTIONS_REGION)
                .is_none()
        );
    }

    /// A declared region is left exactly as the author sized it.
    #[test]
    fn a_declared_region_is_not_resized() {
        let mut window = crate::components::ContextWindow::new(100_000);
        window.add_region(leviath_core::Region::new(
            crate::spec::layout::STAGE_INSTRUCTIONS_REGION.to_string(),
            leviath_core::RegionKind::Pinned,
            4_242,
        ));
        crate::context_setup::ensure_stage_instructions_region(&mut window, &[Some(big_prompt())]);
        assert_eq!(
            window
                .get_region(crate::spec::layout::STAGE_INSTRUCTIONS_REGION)
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
                .get_region(crate::spec::layout::STAGE_INSTRUCTIONS_REGION)
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
                as_text: Vec::new(),
            },
            routing: None,
            accepts_messages: true,
            context_layout: None,
            context_hide: Vec::new(),
            context_reset: Vec::new(),
            system_prompt: Some(prompt),
        };
        let err = crate::pipeline::transition::apply_stage_context(&setup, &mut window)
            .expect_err("a prompt larger than the window cannot be housed");
        assert!(
            err.contains(crate::spec::layout::STAGE_INSTRUCTIONS_REGION),
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
                .get_region(crate::spec::layout::STAGE_INSTRUCTIONS_REGION)
                .expect("made")
                .max_tokens,
            expected
        );
    }

    /// The reported shape: the stage carries its own `[context.regions]`, which
    /// does not re-declare `stage_instructions`.
    #[test]
    fn a_scoped_stage_layout_still_routes_to_the_declared_region() {
        use crate::spec::layout::{BudgetSpec, ContextLayout, RegionDefinition};
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

        let bp = crate::spec::Blueprint::new(
            "t".to_string(),
            "d".to_string(),
            vec![crate::spec::Stage::new(
                "work".to_string(),
                crate::spec::blueprint::ModelConfig::new("p".to_string(), "m".to_string()),
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
                as_text: Vec::new(),
            },
            routing: None,
            accepts_messages: true,
            context_layout: Some(scoped),
            context_hide: Vec::new(),
            context_reset: Vec::new(),
            system_prompt: Some(prompt),
        };
        crate::pipeline::transition::apply_stage_context(&setup, &mut window)
            .expect("the prompt fits the region declared for it");

        let instr = window
            .get_region(crate::spec::layout::STAGE_INSTRUCTIONS_REGION)
            .expect("carried through the scoped layout");
        assert!(
            instr.content.iter().any(|e| e.content.contains("word")),
            "the prompt landed in stage_instructions, not in the scoped task region"
        );
    }
}
