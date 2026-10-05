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
use crate::spec::launch::{DeliveryPlan, LaunchPolicy, Placement, Unattended};
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
    let mut spec = spec_from(graph, "t-run", infs, WINDOW);
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
    RunSpecC(Arc::new(spec_from(graph, "run", &stages, 128_000)))
}

/// `graph` as a spec: run `agent_id`, stage `i` on `stages[i]` with a window
/// of `window`, and each region budgeted as the resolver sizes it against
/// that window.
fn spec_from(graph: RunGraph, agent_id: &str, stages: &[StageInference], window: u32) -> RunSpec {
    let windows = vec![window; stages.len()];
    let plans = graph
        .stages
        .iter()
        .zip(stages)
        .enumerate()
        .map(|(i, (def, inference))| StagePlan {
            stage: def.name.clone(),
            provider: ProviderName::new(&inference.provider_name).expect("a test's provider"),
            model: ModelId::new(&inference.model).expect("a test's model id"),
            context_window: window,
            max_output_tokens: None,
            fallbacks: inference.fallbacks.clone(),
            tools: inference.tools.iter().map(tool_def).collect(),
            output: inference
                .output
                .as_ref()
                .map(|o| OutputDef::from_output_spec(o).expect("a test's output shape reads")),
            region_budgets: crate::resolve::budgets(&graph, i, &windows),
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
            work_item: None,
        },
        delivery: DeliveryPlan::default(),
        env: EnvFingerprint::default(),
        created_at: chrono::Utc::now().timestamp(),
        listed: None,
    }
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
///
/// A test names each stage's model and tools itself, or has a host choose
/// them over a registry and a tool catalog; everything else (the operator's
/// defaults folded in, the region budgets, the working-room check, where an
/// input lands) is decided by the real [`resolve`](crate::resolve::resolve),
/// and the result is placed with [`insert`](crate::insert::insert) as every
/// spawn is.
pub(crate) mod spawning {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::{Path, PathBuf};

    use async_trait::async_trait;
    use bevy_ecs::prelude::*;
    use leviath_core::mime::MimeRegistry;

    use crate::bind::host;
    use crate::pipeline::{ModelDefaults, Providers};
    use crate::providers::ProviderRegistry;
    use crate::resolve::{ResolveMode, Resolved, resolve};
    use crate::spec::env::{
        Caller, CodeFiles, CodeUse, LoadedBlueprint, ModelPlan, OperatorDefaults, ResolveEnv,
        SeedCx, SpawnLimits, StageTools,
    };
    use crate::spec::graph::{
        CodeRef, DependencyDef, MimeRows, NudgeDef, RunGraph, Seed, StageDef,
    };
    use crate::spec::inputs::{InputDecl, InputSlot, InputType, PathKind, RawInput, RegionBinding};
    use crate::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
    use crate::spec::names::{
        BlueprintName, BlueprintRef, Digest, InputName, McpServerName, MimePattern, ModelId,
        ModelRef, ProviderName, RegionName, RunId, WorkdirPath,
    };
    use crate::spec::request::{SpawnRequest, SpawnSource};
    use crate::spec::run_spec::{SeededContent, ToolDef};

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
        /// Content for named regions, keyed by region name: each is the input
        /// of that name, declared as a text input bound to the region of its
        /// name when the graph does not declare it. A key that names no region
        /// is dropped.
        pub(crate) seeds: std::collections::HashMap<String, String>,
        /// The model and tools of each of the graph's stages, in order.
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

    /// How a [`TestEnv`] answers for each stage's model and tools.
    pub(crate) enum StageAnswers {
        /// Each stage's model and tools as the test names them, by stage.
        Given(BTreeMap<String, (ModelPlan, Vec<ToolDef>)>),
        /// As a host chooses them.
        Host(Box<HostAnswers>),
    }

    /// What a host chooses a stage's model and tools from: the model by
    /// [`choose_model`](host::choose_model) over `registry` and `defaults`,
    /// the tools by [`select_tools`](host::select_tools) from `catalog`.
    pub(crate) struct HostAnswers {
        pub(crate) defaults: ModelDefaults,
        pub(crate) registry: ProviderRegistry,
        pub(crate) catalog: Vec<ToolDef>,
    }

    /// A machine that answers what each test says: the graph is the one
    /// installed blueprint, and each stage's model and tools are answered as
    /// [`StageAnswers`] says.
    pub(crate) struct TestEnv {
        blueprint: LoadedBlueprint,
        run_id: String,
        stages: StageAnswers,
        limits: SpawnLimits,
    }

    impl TestEnv {
        /// A machine whose one blueprint is `graph`, named after its title,
        /// that mints `run_id` for the run and answers for its stages as
        /// `stages` says, under `limits`.
        pub(crate) fn new(
            graph: RunGraph,
            run_id: &str,
            stages: StageAnswers,
            limits: SpawnLimits,
        ) -> Self {
            let name = BlueprintName::new(graph.title.as_deref().unwrap_or("t"))
                .expect("a test graph's title");
            Self {
                blueprint: LoadedBlueprint {
                    graph,
                    reference: BlueprintRef { name, digest: None },
                    version: "0.1.0".into(),
                    base_dir: PathBuf::new(),
                },
                run_id: run_id.to_string(),
                stages,
                limits,
            }
        }

        /// A request for this machine's blueprint, with every other field at
        /// its default.
        pub(crate) fn request(&self) -> SpawnRequest {
            SpawnRequest::new(SpawnSource::Blueprint(self.blueprint.reference.clone()))
        }

        /// Resolve `request` as a top-level spawn. The resolver is async; a
        /// thread of its own lets a test call this inside a runtime or
        /// outside one.
        pub(crate) fn resolve(&self, request: &SpawnRequest) -> Result<Resolved, SpawnIssues> {
            std::thread::scope(|s| {
                s.spawn(|| {
                    tokio::runtime::Builder::new_current_thread()
                        .build()
                        .expect("a runtime for the resolver")
                        .block_on(resolve(
                            request,
                            &Caller::TopLevel,
                            self,
                            ResolveMode::Spawn,
                        ))
                })
                .join()
                .expect("the resolver ran")
            })
        }
    }

    /// The limits a test spawn answers to: no depth limit, every seed allowed,
    /// no attachment limit, and `defaults` for what a graph leaves open.
    pub(crate) fn limits(defaults: OperatorDefaults) -> SpawnLimits {
        SpawnLimits {
            default_max_depth: u8::MAX,
            seed_commands_allowed: true,
            max_attachment_bytes: u64::MAX,
            default_max_iterations: None,
            defaults,
        }
    }

    /// A stage's model plan: `stage`'s provider and model, with the window and
    /// reply size the world's provider reports, or the host's defaults when
    /// the provider is not registered.
    fn model_plan(world: &World, stage: &crate::pipeline::ResolvedStage) -> ModelPlan {
        let registered = world
            .get_resource::<Providers>()
            .and_then(|p| p.0.get(&stage.provider_name));
        let window = registered
            .as_ref()
            .map_or(crate::pipeline::DEFAULT_CONTEXT_WINDOW_TOKENS, |p| {
                p.max_context_tokens(&stage.model)
            });
        let most = registered.map_or(
            leviath_providers::ModelCapabilities::default().max_output_tokens,
            |p| p.capabilities(&stage.model).max_output_tokens,
        );
        ModelPlan {
            provider: ProviderName::new(&stage.provider_name).expect("a test's provider"),
            model: ModelId::new(&stage.model).expect("a test's model id"),
            context_window: super::clamp(window),
            max_output_tokens: super::clamp(most),
            fallbacks: stage.fallbacks.clone(),
            notes: stage.notes.clone(),
        }
    }

    #[async_trait]
    impl ResolveEnv for TestEnv {
        async fn blueprint(&self, reference: &BlueprintRef) -> Result<LoadedBlueprint, SpawnIssue> {
            (reference.name == self.blueprint.reference.name)
                .then(|| self.blueprint.clone())
                .ok_or_else(|| {
                    SpawnIssue::new(SpecPath::root(), IssueCode::Unknown, "no such blueprint")
                })
        }

        fn limits(&self) -> SpawnLimits {
            self.limits.clone()
        }

        fn new_run_id(&self, _title: &str) -> RunId {
            RunId::new(&self.run_id).expect("a test run id")
        }

        fn workdir(&self, _requested: Option<&Path>) -> Result<PathBuf, String> {
            Ok(PathBuf::new())
        }

        fn path_exists(&self, _workdir: &Path, _path: &WorkdirPath, _kind: PathKind) -> bool {
            false
        }

        async fn model(
            &self,
            stage: &StageDef,
            requested: Option<&ModelRef>,
        ) -> Result<ModelPlan, SpawnIssue> {
            match &self.stages {
                StageAnswers::Given(given) => Ok(given[stage.name.as_str()].0.clone()),
                StageAnswers::Host(h) => {
                    host::choose_model(stage, requested, &h.defaults, &h.registry).map_err(|i| *i)
                }
            }
        }

        fn compaction_model(&self, _model: &ModelRef) -> Result<(), String> {
            Ok(())
        }

        async fn tools(
            &self,
            _graph: &RunGraph,
            stage: &StageDef,
            _code: &CodeFiles,
            _base: Option<&Path>,
            _workdir: Option<&Path>,
        ) -> Result<StageTools, SpawnIssues> {
            match &self.stages {
                StageAnswers::Given(given) => {
                    Ok(StageTools::from(given[stage.name.as_str()].1.clone()))
                }
                StageAnswers::Host(h) => {
                    host::select_tools(&h.catalog, stage).map(StageTools::from)
                }
            }
        }

        /// Inline code as written; a file's code is its name, since a test's
        /// render hooks arrive compiled.
        async fn code(&self, code: &CodeRef, _base: Option<&Path>) -> Result<Vec<u8>, String> {
            Ok(match code {
                CodeRef::Inline(source) => source.as_bytes().to_vec(),
                CodeRef::File(file) => file.as_bytes().to_vec(),
            })
        }

        fn check_code(&self, _code: &[u8], _used_as: CodeUse) -> Result<(), String> {
            Ok(())
        }

        /// A seed's content is the seed written out.
        async fn seed(&self, seed: &Seed, _cx: SeedCx<'_>) -> Result<SeededContent, String> {
            Ok(SeededContent {
                text: format!("{seed:?}"),
                parts: Vec::new(),
            })
        }

        fn mime_registry(&self, rows: &MimeRows) -> Result<MimeRegistry, String> {
            host::run_registry(&MimeRegistry::builtin(), rows)
        }

        fn sniff(
            &self,
            registry: &MimeRegistry,
            name: &str,
            bytes: &[u8],
            declared: Option<&MimePattern>,
        ) -> Result<String, String> {
            host::sniff(registry, name, bytes, declared)
        }

        async fn dependency(
            &self,
            _dependency: &DependencyDef,
            _code: Option<&[u8]>,
        ) -> Result<(), String> {
            Ok(())
        }

        fn provider_fingerprint(&self, _provider: &ProviderName) -> Option<Digest> {
            None
        }

        fn mcp_fingerprint(&self, _server: &McpServerName) -> Option<Digest> {
            None
        }
    }

    /// A text input named `name`, bound to the region of the same name.
    fn region_input(name: &str) -> InputDecl {
        InputDecl {
            name: InputName::new(name).expect("a test's input name"),
            ty: InputType::Text {
                multiline: true,
                min_len: None,
                max_len: None,
            },
            required: false,
            default: None,
            description: None,
            binds: vec![InputSlot::Region(RegionBinding {
                region: RegionName::new(name).expect("a test's region name"),
                template: None,
            })],
        }
    }

    /// Spawn a run of a graph with seeds and its stages' models: resolve it,
    /// lay out and seed its window, enter its first stage, and
    /// [`insert`](crate::insert::insert) it. Returns `Err` with the
    /// resolver's issues when it refuses the spawn, or when the first stage's
    /// system prompt does not fit its region.
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
        let regions: BTreeSet<String> = std::iter::once(&graph.layout)
            .chain(graph.stages.iter().filter_map(|s| s.layout.as_ref()))
            .flat_map(|l| &l.regions)
            .map(|r| r.name.to_string())
            .collect();
        let mut inputs = Vec::new();
        for (key, text) in seeds.into_iter().filter(|(k, _)| regions.contains(k)) {
            let declared = graph.inputs.iter().any(|d| d.name.as_str() == key);
            if !declared {
                graph.inputs.push(region_input(&key));
            }
            inputs.push((key, RawInput::Text(text)));
        }
        let given = graph
            .stages
            .iter()
            .zip(&stages)
            .map(|(def, stage)| {
                let tools = stage.tools.iter().map(super::tool_def).collect();
                (def.name.to_string(), (model_plan(world, stage), tools))
            })
            .collect();
        let defaults = OperatorDefaults {
            batch_tool_hint: global_hints.batch_tool,
            shell_hint: global_hints.shell,
            nudge: global_nudge,
            ..OperatorDefaults::default()
        };
        let env = TestEnv::new(
            graph,
            &agent_id,
            StageAnswers::Given(given),
            limits(defaults),
        );
        let request = inputs
            .into_iter()
            .fold(env.request(), |request, (key, value)| {
                request.input(key, value)
            });
        let resolved = env.resolve(&request).map_err(|issues| issues.to_string())?;
        let spec = std::sync::Arc::new(resolved.spec);

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

    /// The test machine answers the questions no test spawn happens to ask:
    /// another blueprint is not there, no path exists, a seed is written
    /// out, attached bytes are typed by the host, and every dependency is met.
    #[tokio::test]
    async fn the_test_machine_answers_every_question() {
        use crate::spec::env::{CodeFiles, ResolveEnv, SeedCx};
        use crate::spec::graph::{DependencyDef, Needs, Seed};
        use spawning::{StageAnswers, TestEnv, limits};
        use std::path::Path;

        let g = graph(vec![stage("s")], layout(Vec::new(), 0));
        let env = TestEnv::new(
            g.clone(),
            "r",
            StageAnswers::Given(BTreeMap::new()),
            limits(Default::default()),
        );
        let other = BlueprintRef::parse("other").expect("a name");
        assert!(env.blueprint(&other).await.is_err());
        let path = crate::spec::names::WorkdirPath::new("a.txt").expect("a path");
        assert!(!env.path_exists(Path::new(""), &path, crate::spec::inputs::PathKind::Any));
        let run_id = RunId::new("r").expect("an id");
        let launch = LaunchPolicy::top_level(&Default::default(), 0, true);
        let code = CodeFiles::new();
        let cx = SeedCx {
            run_id: &run_id,
            agent: "a",
            graph: &g,
            launch: &launch,
            workdir: Path::new(""),
            blueprint_dir: None,
            commands_allowed: true,
            code: &code,
            code_refs: &[],
            inputs: &InputValues::default(),
        };
        let seed = Seed::Literal("hi".into());
        let seeded = env.seed(&seed, cx).await.expect("a seed");
        assert_eq!(seeded.text, format!("{seed:?}"));
        let registry = leviath_core::mime::MimeRegistry::builtin();
        assert_eq!(
            env.sniff(&registry, "a.txt", b"hello", None).as_deref(),
            Ok("text/plain")
        );
        let dependency = DependencyDef {
            name: "d".into(),
            needs: Needs::Env("NOWHERE".into()),
            required: true,
            remedy: None,
            description: None,
            install: None,
        };
        assert!(env.dependency(&dependency, None).await.is_ok());
    }
}
