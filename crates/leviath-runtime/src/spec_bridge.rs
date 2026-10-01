//! Test helpers: runs built from a parsed blueprint.
//!
//! A run is resolved from a request and placed with
//! [`insert`](crate::insert::insert). Many tests of the systems that run it
//! are written against a hand-built [`Blueprint`] and per-stage choices
//! instead, so these read such a blueprint as the [`RunSpec`] a resolver
//! would have made of it and place it the same way.

use std::collections::BTreeMap;

use leviath_core::JsonDoc;

use crate::pipeline::StageInference;
use crate::spec::Blueprint;
use crate::spec::graph::{OutputDef, RunGraph};
use crate::spec::inputs::InputValues;
use crate::spec::issues::SpawnIssues;
use crate::spec::launch::{Delivery, LaunchPolicy, Placement, Unattended};
use crate::spec::names::{
    BlueprintName, BlueprintRef, ModelId, ModelRef, ProviderName, RegionName, RunId, ToolName,
};
use crate::spec::run_spec::{EnvFingerprint, RunSpec, SpecOrigin, StagePlan, ToolDef, ToolSource};

/// Read a spawned blueprint as a run spec.
///
/// `bp` is the blueprint as the spawn left it, with every region budget
/// resolved to tokens. `stages` and `windows` are the spawn's per-stage
/// resolution and each stage's model window, aligned with `bp.stages`. Fails
/// only when the blueprint cannot be read as a graph at all, with every field
/// that does not fit.
pub(crate) fn run_spec_from_blueprint(
    bp: &Blueprint,
    agent_id: &str,
    stages: &[StageInference],
    windows: &[usize],
) -> Result<RunSpec, SpawnIssues> {
    let graph = RunGraph::from_blueprint(bp)?;
    let plans = bp
        .stages
        .iter()
        .zip(&graph.stages)
        .zip(stages.iter().zip(windows))
        .map(|((stage, def), (inference, window))| StagePlan {
            stage: def.name.clone(),
            provider: provider(&inference.provider_name),
            model: model(&inference.model),
            context_window: clamp(*window),
            max_output_tokens: None,
            fallbacks: inference
                .fallbacks
                .iter()
                .map(|e| ModelRef {
                    provider: ProviderName::new(e.provider.as_str()).ok(),
                    model: model(&e.model),
                })
                .collect(),
            tools: inference.tools.iter().map(tool_def).collect(),
            output: inference
                .output
                .as_ref()
                .map(|o| OutputDef::from_output_spec(o).expect("a test's output shape reads")),
            region_budgets: region_budgets(bp, stage, &graph, def),
            notes: Vec::new(),
        })
        .collect();
    let origin = SpecOrigin::Blueprint {
        blueprint: BlueprintRef {
            name: BlueprintName::new(bp.name.as_str()).expect("a test blueprint's name"),
            digest: None,
        },
        version: bp.version.clone(),
    };
    Ok(RunSpec {
        run_id: RunId::new(agent_id).expect("a test run id"),
        origin,
        graph,
        inputs: InputValues::default(),
        stages: plans,
        seeded: BTreeMap::new(),
        code: Vec::new(),
        requested_output: None,
        requested_model: None,
        launch: LaunchPolicy {
            unattended: Unattended::Off,
            allow: Vec::new(),
            max_depth: bp
                .max_child_depth
                .map_or(u8::MAX, |d| u8::try_from(d).unwrap_or(u8::MAX)),
            seed_commands: true,
            capture_model_input: false,
        },
        auto_answers: Default::default(),
        placement: Placement {
            workdir: std::path::PathBuf::new(),
            parent: None,
            depth: 0,
            worker_stage: None,
        },
        delivery: Delivery::default(),
        env: EnvFingerprint::default(),
        created_at: chrono::Utc::now().timestamp(),
    })
}

/// A number of tokens as the spec stores it.
fn clamp(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// A provider name a test resolved its stage to.
fn provider(name: &str) -> ProviderName {
    ProviderName::new(name).expect("a test's provider name")
}

/// A model id a test resolved its stage to.
fn model(name: &str) -> ModelId {
    ModelId::new(name).expect("a test's model id")
}

/// The tokens each region the stage sees may take in it.
///
/// `def` is the stage as the graph reads it: its layout holds the same regions
/// in the same order, by their checked names.
fn region_budgets(
    bp: &Blueprint,
    stage: &crate::spec::Stage,
    graph: &RunGraph,
    def: &crate::spec::graph::StageDef,
) -> BTreeMap<RegionName, u32> {
    let layout = stage.context_layout.as_ref().unwrap_or(&bp.context_layout);
    let visible = bp.regions_visible_to(stage);
    layout
        .regions
        .iter()
        .zip(&graph.layout_for(def).regions)
        .filter(|(r, _)| visible.contains(r.name.as_str()))
        .map(|(r, d)| (d.name.clone(), clamp(r.max_tokens)))
        .collect()
}

/// A resolved tool as the spec records it. Where it came from is not known
/// here, so it reads as built in.
fn tool_def(tool: &leviath_providers::Tool) -> ToolDef {
    ToolDef {
        name: ToolName::new(tool.name.as_str()).expect("a test's tool name"),
        description: tool.description.clone(),
        schema: JsonDoc::new(tool.parameters.clone()),
        source: ToolSource::Builtin,
    }
}

/// Spawning runs from a parsed blueprint, for tests written against one.
pub(crate) mod spawning {
    use super::run_spec_from_blueprint;
    use crate::pipeline::{Providers, StageInference};
    use bevy_ecs::prelude::*;

    /// Look up a model's context window (for resolving percentage region budgets)
    /// via the registered [`Providers`]. Falls back to
    /// the pipeline's default window with a warning when the provider isn't
    /// registered - non-fatal, and `min_tokens` floors still protect regions.
    fn context_window_tokens(world: &World, provider_name: &str, model: &str) -> usize {
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
                crate::pipeline::DEFAULT_CONTEXT_WINDOW_TOKENS
            }
        }
    }

    /// Spawn a fully-formed agent into `world` from its blueprint, task, and
    /// per-stage resolution, and return its entity: the task seeds the `task`
    /// region and everything else is as [`spawn_agent_seeded`] does it.
    ///
    /// `stages` must be aligned with `blueprint.stages` (one resolved stage each).
    ///
    /// `global_hints` is the caller's global config toggle for each system-prompt
    /// hint; each is resolved per stage against the blueprint's agent-level and
    /// per-stage override of the same name.
    pub(crate) fn spawn_agent(
        world: &mut World,
        agent_id: String,
        blueprint: crate::spec::Blueprint,
        task: &str,
        stages: Vec<crate::pipeline::ResolvedStage>,
        global_hints: leviath_core::config::PromptHints,
    ) -> Result<Entity, String> {
        let seeds = std::collections::HashMap::from([("task".to_string(), task.to_string())]);
        spawn_agent_seeded(
            world,
            SeededSpawn {
                agent_id,
                blueprint,
                seeds,
                stages,
                global_hints,
                global_nudge: crate::spec::NudgeConfig::default(),
                region_scripts: std::collections::HashMap::new(),
            },
        )
    }

    /// Everything a seeded spawn needs besides the world it spawns into.
    ///
    /// The blueprint and its resolved stages travel with the seeds and the global
    /// defaults because all six are the same decision made at different layers:
    /// what this agent starts with. The caller resolves them; this consumes them.
    pub(crate) struct SeededSpawn {
        /// The run id this agent is registered under.
        pub(crate) agent_id: String,
        /// The blueprint being spawned.
        pub(crate) blueprint: crate::spec::Blueprint,
        /// Content for named caller-input regions, keyed by region name.
        pub(crate) seeds: std::collections::HashMap<String, String>,
        /// The blueprint's stages, already resolved against the provider registry.
        pub(crate) stages: Vec<crate::pipeline::ResolvedStage>,
        /// Config-level prompt hints, applied where the blueprint says nothing.
        pub(crate) global_hints: leviath_core::config::PromptHints,
        /// The config-level nudge, likewise.
        pub(crate) global_nudge: crate::spec::NudgeConfig,
        /// Compiled render hooks, keyed by region name.
        pub(crate) region_scripts: std::collections::HashMap<
            String,
            std::sync::Arc<leviath_scripting::region_hook::RegionScript>,
        >,
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
    pub(crate) fn spawn_agent_seeded(
        world: &mut World,
        spawn: SeededSpawn,
    ) -> Result<Entity, String> {
        let SeededSpawn {
            agent_id,
            blueprint,
            seeds,
            stages,
            global_hints,
            global_nudge,
            region_scripts,
        } = spawn;
        // The registry this run types its bytes by: the host's, or the world's
        // rows with the blueprint's `[mime_types]` on top. A world without a
        // registry (one assembled by hand in a test) has no run registry either.
        let run_registry = world
            .get_resource::<crate::blob_store::MimeRegistryHandle>()
            .map(|r| {
                crate::blob_store::RunMimeRegistry::new(
                    &r.0,
                    blueprint.mime_types.clone(),
                    std::collections::BTreeMap::new(),
                )
                .expect("a test blueprint's [mime_types] read")
            });
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
        let mut spec = run_spec_from_blueprint(&blueprint, &agent_id, &stage_infs, &stage_windows)
            .map_err(|issues| issues.to_string())?;
        fold_operator_defaults(&mut spec.graph, global_hints, &global_nudge);
        for (i, plan) in spec.stages.iter_mut().enumerate() {
            plan.max_output_tokens = None;
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
        crate::insert::enter_first_stage(&spec, &mut window)?;
        let state = crate::insert::initial_state_from(&spec, &window);

        let mut bindings =
            crate::spec::env::Bindings::new().with(crate::insert::RegionScripts(region_scripts));
        if let Some(registry) = run_registry {
            bindings = bindings.with(registry);
        }
        Ok(crate::insert::insert(world, spec, bindings, &state))
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
        fn budgets(
            &self,
            i: usize,
        ) -> std::collections::BTreeMap<crate::spec::names::RegionName, u32> {
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
}

/// Specs and graphs for tests of the systems that read them.
pub(crate) mod test_support {
    pub(crate) use super::spawning::{SeededSpawn, spawn_agent, spawn_agent_seeded};
    use super::*;
    use crate::insert::RunSpecC;
    use crate::spec::graph::StageDef;

    /// The graph a blueprint manifest reads as.
    pub(crate) fn graph_of(manifest: &str) -> RunGraph {
        let bp = crate::spec::manifest::parse_manifest(manifest).expect("the manifest parses");
        RunGraph::from_blueprint(&bp).expect("the manifest reads as a graph")
    }

    /// A stage with every setting at its default.
    pub(crate) fn stage(name: &str) -> StageDef {
        let graph = graph_of(&format!(
            "[agent]\nname = \"t\"\n[stages.\"{name}\"]\nsystem_prompt = \"p\"\n"
        ));
        graph.stages.into_iter().next().expect("one stage")
    }

    /// A spec named `name` over `graph`, with one plan per stage giving each
    /// region of the stage's layout its fixed budget.
    pub(crate) fn spec_named(name: &str, graph: RunGraph) -> RunSpec {
        let base = crate::spec::run_spec::tests::spec();
        let plan = base.stages[0].clone();
        let stages = graph
            .stages
            .iter()
            .map(|s| StagePlan {
                stage: s.name.clone(),
                region_budgets: graph
                    .layout_for(s)
                    .regions
                    .iter()
                    .map(|r| {
                        let tokens = crate::context_setup::budget_tokens(&r.budget, 128_000);
                        (r.name.clone(), clamp(tokens))
                    })
                    .collect(),
                ..plan.clone()
            })
            .collect();
        RunSpec {
            origin: SpecOrigin::Blueprint {
                blueprint: BlueprintRef::parse(name).expect("a valid name"),
                version: "1".into(),
            },
            graph,
            stages,
            ..base
        }
    }

    /// A run spec for a parsed blueprint and the inference each of its stages
    /// resolved to, for tests that start from a blueprint and a stage list
    /// rather than a spawn request.
    ///
    /// The graph is the blueprint read as a [`RunGraph`](crate::spec::graph::RunGraph);
    /// each stage's plan is its [`StageInference`] with a zero context window and
    /// no region budgets, which the caller fills in when it knows them. The spec
    /// launches attended, from no workdir, with nothing seeded.
    pub(crate) fn spec_of_blueprint(
        bp: &crate::spec::Blueprint,
        agent_id: &str,
        stages: &[crate::pipeline::StageInference],
    ) -> crate::spec::run_spec::RunSpec {
        let windows = vec![0; stages.len()];
        let mut spec = super::run_spec_from_blueprint(bp, agent_id, stages, &windows)
            .expect("a test blueprint reads as a spec");
        spec.launch.max_depth = 0;
        for plan in &mut spec.stages {
            plan.region_budgets.clear();
        }
        spec
    }

    /// [`spec_named`], as the component a run carries.
    pub(crate) fn spec_c(name: &str, graph: RunGraph) -> RunSpecC {
        RunSpecC(std::sync::Arc::new(spec_named(name, graph)))
    }

    /// The spec a spawn of `bp` places for the systems that read a run's
    /// graph, each stage on a 128k-token model.
    pub(crate) fn both(bp: Blueprint) -> RunSpecC {
        let stages: Vec<StageInference> = bp
            .stages
            .iter()
            .map(|_| StageInference {
                provider_name: "script".into(),
                model: "m".into(),
                tools: vec![],
                tool_filter: None,
                fallbacks: vec![],
                output: None,
            })
            .collect();
        let windows = vec![128_000; stages.len()];
        let spec = run_spec_from_blueprint(&bp, "run", &stages, &windows)
            .expect("the blueprint reads as a spec");
        RunSpecC(std::sync::Arc::new(spec))
    }
}
