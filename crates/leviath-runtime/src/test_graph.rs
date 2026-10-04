//! Test helpers: run graphs written in code, the specs a spawn would make of
//! them, and spawning them into a world.
//!
//! A run is resolved from a request and placed with
//! [`insert`](crate::insert::insert). The systems that run it are tested
//! against graphs built here and the inference each stage would resolve to,
//! read as the [`RunSpec`] a resolver would have made of them and placed the
//! same way.

use std::collections::BTreeMap;
use std::sync::Arc;

use leviath_core::JsonDoc;

use crate::insert::RunSpecC;
use crate::pipeline::StageInference;
use crate::spec::graph::{
    Budget, CodeRef, EdgeCarry, EdgeCondition, EdgeDef, Eviction, FALL_THROUGH_EDGE, ModelChoice,
    OutputDef, RegionDef, RegionKind, RegionLayoutDef, RunGraph, StageDef, ToolGroup, ToolSelector,
};
use crate::spec::inputs::InputValues;
use crate::spec::launch::{Delivery, LaunchPolicy, Placement, Unattended};
use crate::spec::names::{
    BlueprintName, BlueprintRef, EdgeName, ModelId, ModelRef, ProviderName, RegionName, RunId,
    StageName, ToolName,
};
use crate::spec::run_spec::{EnvFingerprint, RunSpec, SpecOrigin, StagePlan, ToolDef, ToolSource};

/// The context window every plan of [`spec_of`] and [`spec_with`] claims, so a
/// percentage budget resolves the way a spawn without a registered provider
/// resolves it.
const WINDOW: u32 = crate::pipeline::DEFAULT_CONTEXT_WINDOW_TOKENS as u32;

/// A stage named `name` with every setting at its default and no prompt.
pub(crate) fn stage(name: &str) -> StageDef {
    toml::from_str(&format!("name = \"{name}\"\n")).expect("the stage reads")
}

/// A stage named `name` with the system prompt `p`, every other setting at its
/// default.
pub(crate) fn prompted(name: &str) -> StageDef {
    StageDef {
        system_prompt: Some("p".into()),
        ..stage(name)
    }
}

/// A stage's model choice: `model` on `provider`.
pub(crate) fn model(provider: &str, model: &str) -> ModelChoice {
    ModelChoice {
        models: vec![ModelRef {
            provider: Some(ProviderName::new(provider).expect("a test's provider name")),
            model: ModelId::new(model).expect("a test's model id"),
        }],
        ..ModelChoice::default()
    }
}

/// A tool grant list: each entry a tool by name, or a group by its token.
pub(crate) fn tools(entries: &[&str]) -> Vec<ToolSelector> {
    entries
        .iter()
        .map(|t| match ToolGroup::parse(t) {
            Some(group) => ToolSelector::Group(group),
            None => ToolSelector::Tool(ToolName::new(*t).expect("a test's tool name")),
        })
        .collect()
}

/// Region names, checked.
pub(crate) fn regions(names: &[&str]) -> Vec<RegionName> {
    names.iter().map(|n| region_name(n)).collect()
}

/// A region name, checked.
pub(crate) fn region_name(name: &str) -> RegionName {
    RegionName::new(name).expect("a test's region name")
}

/// The graph's word for a window region kind.
pub(crate) fn kind(kind: leviath_core::RegionKind) -> RegionKind {
    use leviath_core::EvictionStrategy as E;
    use leviath_core::RegionKind as K;
    let n = |v: usize| u32::try_from(v).unwrap_or(u32::MAX);
    match kind {
        K::Pinned => RegionKind::Pinned,
        K::SlidingWindow {
            max_items,
            eviction_strategy,
        } => RegionKind::SlidingWindow {
            max_items: n(max_items),
            eviction: match eviction_strategy {
                E::PerItem => Eviction::PerItem,
                E::Bulk { overflow } => Eviction::Bulk(n(overflow)),
                E::Compact { compact_count } => Eviction::Compact(n(compact_count)),
            },
        },
        K::Temporary => RegionKind::Temporary,
        K::Compacting { threshold_tokens } => RegionKind::Compacting {
            threshold_tokens: (threshold_tokens != usize::MAX).then(|| n(threshold_tokens)),
        },
        K::Clearable => RegionKind::Clearable,
        K::CompactHistory { source_region } => RegionKind::CompactHistory {
            source: (!source_region.is_empty()).then(|| region_name(&source_region)),
        },
        K::HashMap { max_entries } => RegionKind::Keyed {
            max_entries: max_entries.map(n),
        },
        K::Checklist => RegionKind::Checklist,
        K::Custom { script, pinned } => RegionKind::Custom {
            code: CodeRef::File(script),
            pinned,
        },
    }
}

/// A region named `name` of `kind`, holding `tokens` tokens.
pub(crate) fn region(name: &str, kind: leviath_core::RegionKind, tokens: u32) -> RegionDef {
    RegionDef {
        name: region_name(name),
        kind: self::kind(kind),
        budget: Budget::Tokens(tokens),
        compact_at: None,
        description: None,
        describe_in_prompt: false,
        required: false,
        required_message: None,
        summarizable: true,
        admission: Default::default(),
        volatility: Default::default(),
        seed: None,
        accepts: Vec::new(),
    }
}

/// A pinned region named `name` holding `percent` (a fraction) of the window.
pub(crate) fn pct(name: &str, percent: f64) -> RegionDef {
    RegionDef {
        budget: Budget::Percent {
            percent,
            min: None,
            max: None,
        },
        ..region(name, leviath_core::RegionKind::Pinned, 0)
    }
}

/// A layout of `regions` sharing `total` tokens.
pub(crate) fn layout(regions: Vec<RegionDef>, total: u32) -> RegionLayoutDef {
    RegionLayoutDef {
        regions,
        total_budget_tokens: total,
        eviction_order: Vec::new(),
    }
}

/// An `always` edge from `from` to `to`, named after `to`, carrying
/// everything and with no gate.
pub(crate) fn edge(from: &str, to: &str) -> EdgeDef {
    EdgeDef {
        name: EdgeName::new(to).expect("a test's edge name"),
        from: StageName::new(from).expect("a test's stage name"),
        to: StageName::new(to).expect("a test's stage name"),
        when: EdgeCondition::Always,
        hint: None,
        carry: EdgeCarry::Direct,
        gate: None,
        stuck: None,
    }
}

/// [`edge`], taken when `when`.
pub(crate) fn edge_when(from: &str, to: &str, when: EdgeCondition) -> EdgeDef {
    EdgeDef {
        when,
        ..edge(from, to)
    }
}

/// A graph titled `t` of `stages` over `layout`, each stage going on to the
/// one after it along a [`FALL_THROUGH_EDGE`].
pub(crate) fn graph(stages: Vec<StageDef>, layout: RegionLayoutDef) -> RunGraph {
    let edges = stages
        .windows(2)
        .map(|pair| EdgeDef {
            name: EdgeName::new(FALL_THROUGH_EDGE).expect("a valid edge name"),
            ..edge(pair[0].name.as_str(), pair[1].name.as_str())
        })
        .collect();
    let mut graph =
        graph_of("[[stages]]\nname = \"s\"\n[layout]\nregions = []\ntotal_budget_tokens = 0\n");
    graph.title = Some("t".into());
    graph.stages = stages;
    graph.edges = edges;
    graph.layout = layout;
    graph
}

/// The graph `text` writes in TOML, as a blueprint's `[graph]` table holds
/// one.
pub(crate) fn graph_of(text: &str) -> RunGraph {
    toml::from_str(text).expect("the graph reads")
}

/// A spec named `name` over `graph`, with one plan per stage giving each
/// region of the stage's layout its budget against a 128k-token window.
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
                .map(|r| (r.name.clone(), clamp(r.budget.resolve(128_000))))
                .collect(),
            ..plan.clone()
        })
        .collect();
    RunSpec {
        origin: SpecOrigin::Blueprint {
            blueprint: BlueprintRef::parse(name).expect("a valid name"),
            version: "1".into(),
            manifest: String::new(),
        },
        graph,
        stages,
        ..base
    }
}

/// [`spec_named`], as the component a run carries.
pub(crate) fn spec_c(name: &str, graph: RunGraph) -> RunSpecC {
    RunSpecC(Arc::new(spec_named(name, graph)))
}

/// The inference stage `i` of a test graph runs on, unless a test says
/// otherwise: provider `p`, model `m{i}`.
pub(crate) fn plan_inference(i: usize) -> StageInference {
    StageInference {
        provider_name: "p".to_string(),
        model: format!("m{i}"),
        ..Default::default()
    }
}

/// The spec a spawn of `graph` runs, each stage on the matching inference of
/// `infs` and an 8k-token window, every region at its own budget against it.
pub(crate) fn spec_with(graph: RunGraph, infs: &[StageInference]) -> RunSpecC {
    let windows = vec![WINDOW as usize; infs.len()];
    let mut spec = spec_from(graph, "t-run", infs, &windows);
    spec.launch.max_depth = 0;
    for plan in &mut spec.stages {
        plan.region_budgets.clear();
    }
    RunSpecC(Arc::new(spec))
}

/// The spec a spawn of `graph` runs, stage `i` on [`plan_inference`].
pub(crate) fn spec_of(graph: RunGraph) -> RunSpecC {
    let infs: Vec<StageInference> = (0..graph.stages.len()).map(plan_inference).collect();
    spec_with(graph, &infs)
}

/// The spec a spawn of `graph` places for the systems that read a run's
/// graph, each stage on a 128k-token model.
pub(crate) fn both(graph: RunGraph) -> RunSpecC {
    let stages: Vec<StageInference> = graph
        .stages
        .iter()
        .map(|_| StageInference {
            provider_name: "script".into(),
            model: "m".into(),
            ..Default::default()
        })
        .collect();
    let windows = vec![128_000; stages.len()];
    RunSpecC(Arc::new(spec_from(graph, "run", &stages, &windows)))
}

/// `graph` as the spec a resolver makes of it: run `agent_id`, stage `i` on
/// `stages[i]` with a window of `windows[i]`, and each region a stage sees
/// budgeted as [`budgets`] sizes it.
fn spec_from(
    graph: RunGraph,
    agent_id: &str,
    stages: &[StageInference],
    windows: &[usize],
) -> RunSpec {
    let plans = graph
        .stages
        .iter()
        .zip(stages.iter().zip(windows))
        .enumerate()
        .map(|(i, (def, (inference, window)))| StagePlan {
            stage: def.name.clone(),
            provider: ProviderName::new(&inference.provider_name).expect("a test's provider"),
            model: ModelId::new(&inference.model).expect("a test's model id"),
            context_window: clamp(*window),
            max_output_tokens: None,
            fallbacks: inference.fallbacks.clone(),
            tools: inference.tools.iter().map(tool_def).collect(),
            output: inference
                .output
                .as_ref()
                .map(|o| OutputDef::from_output_spec(o).expect("a test's output shape reads")),
            region_budgets: budgets(&graph, i, windows),
            notes: Vec::new(),
        })
        .collect();
    let origin = SpecOrigin::Blueprint {
        blueprint: BlueprintRef {
            name: BlueprintName::new(graph.title.as_deref().unwrap_or("t"))
                .expect("a test graph's title"),
            digest: None,
        },
        version: "0.1.0".into(),
        manifest: String::new(),
    };
    RunSpec {
        run_id: RunId::new(agent_id).expect("a test run id"),
        origin,
        launch: LaunchPolicy {
            unattended: Unattended::Off,
            allow: Vec::new(),
            max_depth: graph.max_child_depth.unwrap_or(u8::MAX),
            seed_commands: true,
            capture_model_input: false,
        },
        graph,
        inputs: InputValues::default(),
        stages: plans,
        seeded: BTreeMap::new(),
        code: Vec::new(),
        requested_output: None,
        requested_model: None,
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
        listed: None,
    }
}

/// Each region's budget in stage `index`, in tokens.
///
/// A stage with its own layout sizes its regions against its own window, and
/// takes the graph layout's regions as well, sized as the graph layout sizes
/// them. A region of the graph's layout is sized against the smallest window
/// among the stages that use that layout and see it, so a region budgeted for
/// a wide-window stage is never counted against a narrow one that never reads
/// it. A region no such stage sees takes the first stage's window.
fn budgets(graph: &RunGraph, index: usize, windows: &[usize]) -> BTreeMap<RegionName, u32> {
    let shared = graph.layout.regions.iter().map(|r| {
        let window = graph
            .stages
            .iter()
            .zip(windows)
            .filter(|(s, _)| {
                s.layout.is_none()
                    && crate::pipeline::spec_view::visible_regions(graph, s)
                        .contains(r.name.as_str())
            })
            .map(|(_, w)| *w)
            .min()
            .unwrap_or(windows[0]);
        (r.name.clone(), clamp(r.budget.resolve(window)))
    });
    let own = graph.stages[index]
        .layout
        .iter()
        .flat_map(|l| l.regions.iter())
        .map(|r| (r.name.clone(), clamp(r.budget.resolve(windows[index]))));
    shared.chain(own).collect()
}

/// Refuse a graph one of whose stages has too little room to work in: its
/// fixed regions, over the regions it sees, leave less than 8000 tokens of a
/// window of at least 20,000.
fn check_working_room(spec: &RunSpec) -> Result<(), String> {
    for (stage, plan) in spec.graph.stages.iter().zip(&spec.stages) {
        let window = plan.context_window as usize;
        let visible = crate::pipeline::spec_view::visible_regions(&spec.graph, stage);
        let fixed: usize = spec
            .graph
            .layout_for(stage)
            .regions
            .iter()
            .filter(|r| visible.contains(r.name.as_str()))
            .filter(|r| {
                matches!(
                    r.kind,
                    RegionKind::Pinned
                        | RegionKind::Keyed { .. }
                        | RegionKind::CompactHistory { .. }
                        | RegionKind::Custom { pinned: true, .. }
                )
            })
            .map(|r| plan.region_budgets.get(&r.name).copied().unwrap_or(0) as usize)
            .sum();
        let working = window.saturating_sub(fixed);
        if window >= 20_000 && working < 8000 {
            return Err(format!(
                "context layout leaves only {working} working tokens after fixed regions \
                 consume {fixed} of the {window} window"
            ));
        }
    }
    Ok(())
}

/// A number of tokens as the spec stores it.
fn clamp(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
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

/// Spawning runs from a graph, for tests of the systems that run them.
pub(crate) mod spawning {
    use super::{check_working_room, spec_from};
    use crate::pipeline::{Providers, StageInference};
    use crate::spec::graph::{NudgeDef, RunGraph};
    use bevy_ecs::prelude::*;

    /// A model's context window as the registered [`Providers`] report it, or
    /// the pipeline's default window when the provider is not registered.
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

    /// Spawn a run of `graph` into `world` with `task` in its task region and
    /// stage `i` on `stages[i]`, and return its entity. Everything else is as
    /// [`place_test_run`] does it.
    ///
    /// `global_hints` is the operator's toggle for each system-prompt hint; a
    /// hint the graph leaves open takes it.
    pub(crate) fn place_test_task(
        world: &mut World,
        agent_id: String,
        graph: RunGraph,
        task: &str,
        stages: Vec<crate::pipeline::ResolvedStage>,
        global_hints: leviath_core::config::PromptHints,
    ) -> Result<Entity, String> {
        let seeds = std::collections::HashMap::from([("task".to_string(), task.to_string())]);
        place_test_run(
            world,
            TestRun {
                agent_id,
                graph,
                seeds,
                stages,
                global_hints,
                global_nudge: NudgeDef::default(),
                region_scripts: std::collections::HashMap::new(),
            },
        )
    }

    /// Everything a seeded spawn needs besides the world it spawns into.
    pub(crate) struct TestRun {
        /// The run id this run is registered under.
        pub(crate) agent_id: String,
        /// The graph being spawned.
        pub(crate) graph: RunGraph,
        /// Content for named regions, keyed by region name. The `task` key
        /// lands in the pinned region named `task`, else the first pinned one.
        pub(crate) seeds: std::collections::HashMap<String, String>,
        /// The graph's stages, already resolved against the provider registry.
        pub(crate) stages: Vec<crate::pipeline::ResolvedStage>,
        /// Operator prompt hints, applied where the graph says nothing.
        pub(crate) global_hints: leviath_core::config::PromptHints,
        /// The operator's nudge, likewise.
        pub(crate) global_nudge: NudgeDef,
        /// Compiled render hooks, keyed by region name.
        pub(crate) region_scripts: std::collections::HashMap<
            String,
            std::sync::Arc<leviath_scripting::region_hook::RegionScript>,
        >,
    }

    /// The operator's defaults folded into the graph, as a resolver folds them,
    /// so the run's spec carries them: each hint and nudge setting the graph
    /// leaves open takes the operator's.
    fn fold_operator_defaults(
        graph: &mut RunGraph,
        hints: leviath_core::config::PromptHints,
        nudge: &NudgeDef,
    ) {
        graph.batch_tool_hint = Some(graph.batch_tool_hint.unwrap_or(hints.batch_tool));
        graph.shell_hint = Some(graph.shell_hint.unwrap_or(hints.shell));
        let own = graph.nudge.clone().unwrap_or_default();
        graph.nudge = Some(NudgeDef {
            enabled: own.enabled.or(nudge.enabled),
            max: own.max.or(nudge.max),
            text: own.text.or_else(|| nudge.text.clone()),
        });
    }

    /// The region a seed lands in: its own name, or for the `task` seed, the
    /// pinned region named `task` or else the first pinned region.
    fn seed_target(graph: &RunGraph, key: &str) -> Option<crate::spec::names::RegionName> {
        use crate::spec::graph::RegionKind;
        let regions = &graph.layout.regions;
        let pinned = |r: &&crate::spec::graph::RegionDef| matches!(r.kind, RegionKind::Pinned);
        let found = match key {
            "task" => regions
                .iter()
                .filter(pinned)
                .find(|r| r.name.as_str() == "task")
                .or_else(|| regions.iter().find(pinned)),
            _ => regions.iter().find(|r| r.name.as_str() == key),
        };
        found.map(|r| r.name.clone())
    }

    /// Spawn a run of a graph with seeds and resolved stages: build its spec,
    /// lay out and seed its window, enter its first stage, and
    /// [`insert`](crate::insert::insert) it. Returns `Err` when a stage has
    /// too little room to work in, or when the first stage's system prompt
    /// does not fit its region.
    ///
    /// Every percentage region budget is sized here against the model windows
    /// the providers report, and written into each stage's plan.
    pub(crate) fn place_test_run(world: &mut World, spawn: TestRun) -> Result<Entity, String> {
        let TestRun {
            agent_id,
            mut graph,
            seeds,
            stages,
            global_hints,
            global_nudge,
            region_scripts,
        } = spawn;
        // The registry this run types its bytes by: the world's rows with the
        // graph's `mime_types` on top. A world without a registry has no run
        // registry either.
        let run_registry = world
            .get_resource::<crate::blob_store::MimeRegistryHandle>()
            .map(|r| {
                crate::blob_store::RunMimeRegistry::new(
                    &r.0,
                    crate::bind::host::mime_table(&graph.mime_types),
                    std::collections::BTreeMap::new(),
                )
                .expect("a test graph's mime rows read")
            });
        let windows: Vec<usize> = stages
            .iter()
            .map(|rs| context_window_tokens(world, &rs.provider_name, &rs.model))
            .collect();
        let notes: Vec<Vec<String>> = stages.iter().map(|rs| rs.notes.clone()).collect();
        let infs: Vec<StageInference> = stages
            .into_iter()
            .map(|rs| StageInference {
                provider_name: rs.provider_name,
                model: rs.model,
                tools: rs.tools,
                tool_filter: None,
                fallbacks: rs.fallbacks,
                output: rs.output,
            })
            .collect();
        fold_operator_defaults(&mut graph, global_hints, &global_nudge);
        let mut spec = spec_from(graph, &agent_id, &infs, &windows);
        for (plan, notes) in spec.stages.iter_mut().zip(notes) {
            plan.notes = notes;
        }
        check_working_room(&spec)?;
        for (key, content) in &seeds {
            let Some(target) = seed_target(&spec.graph, key) else {
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
}

pub(crate) use spawning::{TestRun, place_test_run, place_test_task};

#[cfg(test)]
mod tests {
    use leviath_core::{EvictionStrategy as E, RegionKind as K};

    use super::*;

    /// Every window region kind reads as the graph's kind of the same name.
    #[test]
    fn every_window_kind_has_a_graph_kind() {
        let sliding = |eviction_strategy| K::SlidingWindow {
            max_items: 3,
            eviction_strategy,
        };
        let cases = [
            (sliding(E::PerItem), "SlidingWindow"),
            (sliding(E::Bulk { overflow: 2 }), "Bulk(2)"),
            (sliding(E::Compact { compact_count: 4 }), "Compact(4)"),
            (K::Temporary, "Temporary"),
            (
                K::Compacting {
                    threshold_tokens: 9,
                },
                "Some(9)",
            ),
            (
                K::Compacting {
                    threshold_tokens: usize::MAX,
                },
                "None",
            ),
            (K::Clearable, "Clearable"),
            (
                K::CompactHistory {
                    source_region: "log".into(),
                },
                "\"log\"",
            ),
            (
                K::CompactHistory {
                    source_region: String::new(),
                },
                "source: None",
            ),
            (
                K::HashMap {
                    max_entries: Some(5),
                },
                "Some(5)",
            ),
            (K::Checklist, "Checklist"),
            (
                K::Custom {
                    script: "r.rhai".into(),
                    pinned: true,
                },
                "r.rhai",
            ),
        ];
        for (core, shown) in cases {
            let read = format!("{:?}", kind(core));
            assert!(read.contains(shown), "{read} lacks {shown}");
        }
    }
}
