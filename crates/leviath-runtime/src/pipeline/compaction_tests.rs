//! A graph's compaction model, from the graph to a summary on the wire: the
//! route a run resolves for it and the call each lane makes.

use std::sync::{Arc, Mutex};

use crate::ContextWindow;
use bevy_ecs::prelude::*;
use leviath_core::{Region, RegionKind};
use leviath_providers::{InferenceRequest, InferenceResponse, ModelCapabilities, Provider};
use tokio::runtime::Handle;
use tokio::sync::{Notify, mpsc};

use super::*;
use crate::compaction_bridge::CompactionOutcome;
use crate::inference_pool::{InferencePoolConfig, InferencePools};
use crate::providers::ProviderRegistry;
use crate::spec::graph::RunGraph;
use crate::test_graph::spawning::{HostAnswers, StageAnswers, TestEnv, limits};

/// A provider that serves the models it is given, answers every request with
/// a summary, and records the model each request named.
struct Serving {
    models: Vec<&'static str>,
    asked: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl Provider for Serving {
    async fn infer(&self, r: &InferenceRequest) -> leviath_providers::Result<InferenceResponse> {
        self.asked.lock().unwrap().push(r.model.clone());
        Ok(InferenceResponse {
            parts: Vec::new(),
            content: "summary".to_string(),
            tool_calls: vec![],
            tokens_used: leviath_providers::TokenUsage::new(1, 0, 0, 1),
            finish_reason: leviath_providers::FinishReason::Complete,
            reasoning: None,
            content_is_reasoning: false,
        })
    }
    async fn count_tokens(&self, _t: &str, _m: &str) -> usize {
        1
    }
    fn max_context_tokens(&self, _m: &str) -> usize {
        100_000
    }
    fn name(&self) -> &str {
        "serving"
    }
    fn capabilities(&self, _m: &str) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    fn serves_model(&self, model_key: &str) -> Option<String> {
        self.models
            .contains(&model_key)
            .then(|| model_key.to_string())
    }
}

/// What each provider of a test registry was asked for, by name.
type Asked = Vec<(String, Arc<Mutex<Vec<String>>>)>;

/// A registry with each `(name, models)` registered as a [`Serving`]
/// provider, and what each was asked for, by name.
fn registry(providers: &[(&str, &[&'static str])]) -> (ProviderRegistry, Asked) {
    let mut r = ProviderRegistry::new();
    let mut asked = Vec::new();
    for (name, models) in providers {
        let log = Arc::new(Mutex::new(Vec::new()));
        r.register(
            (*name).to_string(),
            Arc::new(Serving {
                models: models.to_vec(),
                asked: log.clone(),
            }),
        );
        asked.push(((*name).to_string(), log));
    }
    (r, asked)
}

/// A one-stage graph on `stage_model`, its compaction written as `compaction`.
fn graph(stage_model: &str, compaction: &str) -> RunGraph {
    toml::from_str(&format!(
        r#"
entry = "work"
compaction = {compaction}

[layout]
total_budget_tokens = 0
regions = [{{ name = "task", kind = "pinned", budget = "10%" }}, {{ name = "notes", kind = "compacting", budget = "50%" }}]

[[stages]]
name = "work"
model = {{ models = [{{ model = "{stage_model}" }}] }}
"#
    ))
    .expect("a test graph reads")
}

/// The compaction settings a run of `graph` is placed with, resolved over
/// `registry` by `defaults` as a spawn resolves it.
fn placed_settings(
    graph: RunGraph,
    defaults: ModelDefaults,
    registry: &ProviderRegistry,
) -> (CompactionSettings, StageInference) {
    let env = TestEnv::new(
        graph,
        "run",
        StageAnswers::Host(Box::new(HostAnswers {
            defaults,
            registry: registry.clone(),
            catalog: Vec::new(),
        })),
        limits(Default::default()),
    );
    let resolved = env.resolve(&env.request()).expect("the graph resolves");
    let spec = Arc::new(resolved.spec);
    let mut window = crate::insert::seeded_window(&spec, &Default::default());
    crate::insert::enter_first_stage(&spec, &mut window).expect("the first stage fits");
    let state = crate::insert::initial_state_from(&spec, &window);
    let mut world = World::new();
    let e = crate::insert::insert(&mut world, spec, crate::spec::env::Bindings::new(), &state);
    (
        world
            .get::<CompactionSettings>(e)
            .cloned()
            .expect("a graph with compaction places its settings"),
        world
            .get::<StageInference>(e)
            .cloned()
            .expect("the run is in its first stage"),
    )
}

/// A window whose compacting region is past its threshold, with a history to
/// roll the summary into.
fn compacting_window() -> ContextWindow {
    let mut w = ContextWindow::new(100);
    let mut notes = Region::new(
        "notes".to_string(),
        RegionKind::Compacting {
            threshold_tokens: 5,
        },
        100,
    );
    let _ = notes.add_entry("x".repeat(380), 95);
    w.add_region(notes);
    w.current_tokens = w.calculate_tokens();
    w
}

fn agent_state() -> AgentState {
    AgentState {
        agent_id: "a".to_string(),
        current_visit: String::new(),
        current_stage: "work".to_string(),
        iteration: 0,
        status: AgentStatus::Active,
        spawned_children_ids: vec![],
        pending_wait: None,
        accepts_messages: true,
    }
}

/// A world serving `registry` on the compaction lane, and that lane's
/// outcomes.
fn world(registry: ProviderRegistry) -> (World, mpsc::UnboundedReceiver<CompactionOutcome>) {
    let (tx, _rx) = mpsc::unbounded_channel();
    let (ttx, _trx) = mpsc::unbounded_channel();
    let (ctx, crx) = mpsc::unbounded_channel();
    let (cstx, _csrx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(Providers(registry));
    world.insert_resource(InferenceStage {
        pools: Arc::new(InferencePools::new(InferencePoolConfig::new())),
        outcomes: tx,
        transition_outcomes: ttx,
        compaction_outcomes: ctx,
        content_summary_outcomes: cstx,
        wake: Arc::new(Notify::new()),
        runtime: Handle::current(),
        stream_inference: true,
    });
    (world, crx)
}

/// Run memory-pressure compaction once for an agent placed with `settings`
/// in `stage`, and say where its summary went, if anywhere.
async fn summarized_on(
    registry: ProviderRegistry,
    settings: CompactionSettings,
    stage: StageInference,
) -> Option<(String, String)> {
    let (mut world, mut outcomes) = world(registry);
    let e = world
        .spawn((
            compacting_window(),
            settings,
            stage,
            agent_state(),
            ReadyToInfer,
        ))
        .id();
    let mut schedule = Schedule::default();
    schedule.add_systems(dispatch_compaction);
    schedule.run(&mut world);
    world.get::<AwaitingCompaction>(e)?;
    let outcome = outcomes.recv().await.expect("the job reports");
    assert!(outcome.result.is_ok(), "the summary was written");
    Some((outcome.provider_name, outcome.model))
}

/// The issue's case: the compaction model names no provider, and the only
/// provider is the user's default. The summary runs there.
#[tokio::test]
async fn a_bare_compaction_model_runs_on_the_users_provider() {
    let (registry, asked) = registry(&[("cfg", &["m", "small"])]);
    let defaults = ModelDefaults {
        provider: "cfg".to_string(),
        ..ModelDefaults::default()
    };
    let g = graph(
        "m",
        r#"{ model = { model = "small" }, max_summary_tokens = 200, temperature = 0.2 }"#,
    );
    let (settings, stage) = placed_settings(g, defaults, &registry);
    let went = summarized_on(registry, settings, stage).await;
    assert_eq!(went, Some(("cfg".to_string(), "small".to_string())));
    assert_eq!(*asked[0].1.lock().unwrap(), ["small"]);
}

/// A compaction that names no model summarizes on the model the current
/// stage resolved to, on the user's provider.
#[tokio::test]
async fn a_compaction_with_no_model_runs_on_the_stage_model() {
    let (registry, _) = registry(&[("cfg", &["m"])]);
    let defaults = ModelDefaults {
        provider: "cfg".to_string(),
        ..ModelDefaults::default()
    };
    let (settings, stage) = placed_settings(graph("m", "{}"), defaults, &registry);
    assert!(settings.0.routes.is_empty(), "no routes of its own");
    assert_eq!(settings.0.max_summary_tokens, 2000);
    let went = summarized_on(registry, settings, stage).await;
    assert_eq!(went, Some(("cfg".to_string(), "m".to_string())));
}

/// A list of compaction models is resolved as a stage's is: a name nothing
/// here serves is passed over for the next one.
#[tokio::test]
async fn a_listed_compaction_lands_on_a_model_this_machine_serves() {
    let (registry, _) = registry(&[("cfg", &["m", "small"])]);
    let defaults = ModelDefaults {
        provider: "cfg".to_string(),
        ..ModelDefaults::default()
    };
    let g = graph(
        "m",
        r#"{ models = [{ model = "nobody-serves-this" }, { model = "small" }] }"#,
    );
    let (settings, stage) = placed_settings(g, defaults, &registry);
    let routes: Vec<String> = settings.0.routes.iter().map(ToString::to_string).collect();
    assert_eq!(routes, ["cfg/small"]);
    let went = summarized_on(registry, settings, stage).await;
    assert_eq!(went, Some(("cfg".to_string(), "small".to_string())));
}

/// On the stage's model, a stage whose provider is gone is followed along its
/// own fallbacks, in order.
#[tokio::test]
async fn a_stage_model_summary_follows_the_stages_fallbacks() {
    let (registry, _) = registry(&[("cfg", &["m"])]);
    let stage = StageInference {
        provider_name: "gone".to_string(),
        model: "m".to_string(),
        fallbacks: vec![crate::spec::names::ModelRef::parse("cfg/m").unwrap()],
        ..StageInference::default()
    };
    let settings = CompactionSettings(leviath_core::CompactionConfig::default());
    let went = summarized_on(registry, settings, stage).await;
    assert_eq!(went, Some(("cfg".to_string(), "m".to_string())));
}

/// With no route this machine can serve, nothing is sent and the agent
/// carries on to inference.
#[tokio::test]
async fn no_servable_route_leaves_the_agent_ready() {
    let (registry, _) = registry(&[("cfg", &["m"])]);
    let settings = CompactionSettings(leviath_core::CompactionConfig::default());
    let went = summarized_on(registry, settings, StageInference::default()).await;
    assert_eq!(went, None);
}
