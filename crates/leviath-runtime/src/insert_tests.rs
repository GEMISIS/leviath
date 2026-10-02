use super::*;
use crate::components::{AgentState, AgentStatus, InferenceConfig};
use crate::pipeline::{
    AwaitingTransitionChoice, LastTransition, ReadyToInfer, StageCursor, StageInference,
    StageLedger, StageProgress, VisitCounts, WaitingForChildren,
};
use crate::spec::graph::ToolRescan;
use crate::spec::names::{EdgeName, RunId, StageName};
use crate::state::context::{EntryKind, EntryMeta, EntryState, RegionState, ToolCallState};
use crate::state::{
    Clock, Cursor, FanOutState, FinalOutputState, Flags, MessageState, PendingBatch, PipelinePhase,
    Spend, StageRecord, StageStatus, ToolResultState, Totals, TransitionReason, TransitionRecord,
    VisitRecord, WorkItemState,
};
use leviath_providers::Provider as _;

#[derive(Component, Debug, PartialEq)]
struct Marker(u8);

fn sn(name: &str) -> StageName {
    StageName::new(name).unwrap()
}

/// The two-stage test spec (`plan` then `build`), each stage with a plan.
fn two_stage_spec() -> RunSpec {
    let mut spec = crate::spec::run_spec::tests::spec();
    let mut build = spec.stages[0].clone();
    build.stage = sn("build");
    build.model = crate::spec::names::ModelId::new("builder").unwrap();
    build.notes = vec![];
    spec.stages.push(build);
    spec
}

#[test]
fn a_run_lands_with_its_spec_and_its_bindings() {
    let spec = Arc::new(crate::spec::run_spec::tests::spec());
    let state = initial_state(&spec);
    let mut world = World::new();
    let seen = Arc::new(std::sync::Mutex::new(None));
    let record = seen.clone();
    let bindings = Bindings::new()
        .with(Marker(7))
        .with(RegionScripts::default())
        .after_insert(move |e| *record.lock().unwrap() = Some(e));
    assert_eq!(bindings.len(), 3);
    assert!(!bindings.is_empty());
    assert!(Bindings::new().is_empty());
    assert_eq!(format!("{bindings:?}"), "Bindings(2 bundles, 1 follow-ups)");
    let e = insert(&mut world, spec.clone(), bindings, &state);
    assert_eq!(world.get::<Marker>(e), Some(&Marker(7)));
    assert_eq!(world.get::<RunSpecC>(e).unwrap().0.run_id, spec.run_id);
    assert_eq!(*seen.lock().unwrap(), Some(e));
    assert!(
        world.get::<RegionScripts>(e).is_none(),
        "the scripts move into the window"
    );
    let mut more = Bindings::new();
    more.extend(Bindings::new().with(Marker(1)).after_insert(|_| {}));
    assert_eq!(more.len(), 2);
}

/// A provider that reports a fixed context window.
struct Window(usize);
#[async_trait::async_trait]
impl leviath_providers::Provider for Window {
    async fn infer(
        &self,
        _r: &leviath_providers::InferenceRequest,
    ) -> leviath_providers::Result<leviath_providers::InferenceResponse> {
        Err(leviath_providers::ProviderError::Other("not called".into()))
    }
    async fn count_tokens(&self, _t: &str, _m: &str) -> usize {
        1
    }
    fn max_context_tokens(&self, _m: &str) -> usize {
        self.0
    }
    fn name(&self) -> &str {
        "window"
    }
    fn capabilities(&self, _m: &str) -> leviath_providers::ModelCapabilities {
        leviath_providers::ModelCapabilities::default()
    }
}

#[tokio::test]
async fn the_test_provider_answers_nothing_but_its_window() {
    let p = Window(5);
    let request = leviath_providers::InferenceRequest {
        system: vec![],
        messages: vec![],
        model: "m".into(),
        max_tokens: 1,
        temperature: 0.0,
        tools: vec![],
        extra: serde_json::Value::Null,
        request_timeout_secs: None,
    };
    assert!(p.infer(&request).await.is_err());
    assert_eq!(p.count_tokens("x", "m").await, 1);
    assert_eq!((p.max_context_tokens("m"), p.name()), (5, "window"));
    let _ = p.capabilities("m");
}

fn world_with_window(tokens: usize) -> World {
    let mut world = World::new();
    let mut reg = crate::providers::ProviderRegistry::new();
    reg.register("p".to_string(), Arc::new(Window(tokens)));
    world.insert_resource(crate::pipeline::Providers(reg));
    world
}

/// What a placement is, with the parts minted fresh on every spawn (visit ids
/// and the seconds things happened at) blanked out, so two placements of the
/// same run compare equal.
fn placement(world: &World, e: Entity) -> String {
    let mut state = world.get::<AgentState>(e).unwrap().clone();
    state.current_visit.clear();
    let mut context = world.get::<ContextWindow>(e).unwrap().to_state();
    for region in &mut context.regions {
        for entry in &mut region.entries {
            entry.timestamp = 0;
        }
    }
    let mut ledger = world.get::<StageLedger>(e).unwrap().clone();
    for rec in &mut ledger.0 {
        for visit in &mut rec.visits {
            visit.id.clear();
            visit.entered_at = 0;
        }
    }
    let mut visits: Vec<_> = world
        .get::<VisitCounts>(e)
        .unwrap()
        .0
        .clone()
        .into_iter()
        .collect();
    visits.sort();
    let md = world.get::<crate::persistence::RunMetadata>(e).unwrap();
    format!(
        "{state:?}\n{:?}\n{visits:?}\n{:?}\n{context:?}\n{:?}\n{:?}\n{ledger:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{}|{}|{:?}|{}\n{:?}{:?}{:?}{:?}",
        world.get::<StageCursor>(e).unwrap(),
        world.get::<StageProgress>(e).unwrap(),
        world.get::<StageInference>(e).unwrap(),
        world.get::<InferenceConfig>(e).unwrap(),
        world.get::<crate::pipeline::StageIoBuffer>(e).unwrap(),
        world.get::<crate::persistence::TokenTotals>(e).unwrap(),
        world.get::<crate::persistence::RunClock>(e).unwrap(),
        world.get::<crate::persistence::RunOutcomeFlags>(e).unwrap(),
        md.run_id,
        md.agent_name,
        md.model,
        md.num_stages,
        world.get::<ReadyToInfer>(e),
        world
            .get::<crate::components::ToolResultRoutingComponent>(e)
            .map(|r| r
                .routing
                .tool_regions
                .iter()
                .collect::<std::collections::BTreeMap<_, _>>()),
        world
            .get::<crate::pipeline::CompactionSettings>(e)
            .map(|c| c.0.model.clone()),
        world
            .get::<crate::repetition::RepetitionDetector>(e)
            .is_some(),
    )
}

/// A graph shaped like a coding agent: an entry stage that maps the project
/// with its tool results routed into their own region, two more stages after
/// it (the last one writing files), caller inputs, and a compaction model.
fn coding_graph() -> crate::spec::graph::RunGraph {
    use crate::spec::graph::{CompactionDef, ToolRoutingDef};
    use crate::spec::inputs::{InputDecl, InputSlot, InputType, RegionBinding};
    use crate::spec::names::{InputName, ModelRef, ToolName};
    use crate::test_graph::{layout, region, region_name, tools};
    use leviath_core::region::{EvictionStrategy, RegionKind};
    let stage = |name: &str, prompt: &str| crate::spec::graph::StageDef {
        model: crate::test_graph::model("p", "m"),
        tools: tools(&["read_file"]),
        system_prompt: Some(prompt.to_string()),
        ..crate::test_graph::stage(name)
    };
    let mut discover = stage("discover", "Before any planning, map the project.");
    discover.tool_routing = Some(ToolRoutingDef {
        default_region: region_name("conversation"),
        tool_regions: [(ToolName::new("read_file").unwrap(), region_name("codebase"))].into(),
        keep_results: false,
        max_result_tokens: None,
        tool_max_result_tokens: Default::default(),
    });
    let mut implement = stage("implement", "Make the change.");
    implement.tools.extend(tools(&["write_file", "edit_file"]));
    let regions = vec![
        region("task", RegionKind::Pinned, 2_000),
        region("constraints", RegionKind::Pinned, 2_000),
        region("codebase", RegionKind::Pinned, 20_000),
        region(
            "conversation",
            RegionKind::SlidingWindow {
                max_items: 30,
                eviction_strategy: EvictionStrategy::default(),
            },
            40_000,
        ),
    ];
    let mut graph = crate::test_graph::graph(
        vec![discover, stage("plan", "Plan the change."), implement],
        layout(regions, 64_000),
    );
    graph.title = Some("coder".into());
    graph.description = Some("Plans, writes and checks a change.".into());
    graph.entry = Some(sn("discover"));
    graph.inputs = ["constraints", "task"]
        .iter()
        .map(|name| InputDecl {
            name: InputName::new(*name).unwrap(),
            ty: InputType::Text {
                multiline: true,
                min_len: None,
                max_len: None,
            },
            required: false,
            default: None,
            description: None,
            binds: vec![InputSlot::Region(RegionBinding {
                region: region_name(name),
                template: None,
            })],
        })
        .collect();
    let compaction = leviath_core::lifecycle::CompactionConfig::default();
    graph.compaction = Some(CompactionDef {
        model: ModelRef::parse(&format!("{}/{}", compaction.provider, compaction.model)).unwrap(),
        system_prompt: compaction.system_prompt,
        user_prompt_template: compaction.user_prompt_template,
        max_summary_tokens: u32::try_from(compaction.max_summary_tokens).unwrap(),
        temperature: f64::from(compaction.temperature),
    });
    graph
}

/// A graph spawned through the test bridge places exactly what inserting its
/// spec at its initial state places, and that is the run the graph describes:
/// at its entry stage, visited once, with the stage's instructions, inference
/// and routing in place.
#[test]
fn a_graph_lands_the_same_through_spawn_and_through_insert() {
    let graph = coding_graph();
    let stages = graph
        .stages
        .iter()
        .enumerate()
        .map(|(i, _)| crate::pipeline::ResolvedStage {
            provider_name: "p".to_string(),
            model: "m".to_string(),
            tools: vec![leviath_providers::Tool {
                name: "read_file".to_string(),
                description: "read".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            }],
            fallbacks: vec![crate::spec::names::ModelRef::parse("q/n").unwrap()],
            output: None,
            notes: match i {
                0 => vec!["moved".to_string()],
                _ => vec![],
            },
        })
        .collect();
    let mut spawned = world_with_window(200_000);
    let a = crate::pipeline::place_test_run(
        &mut spawned,
        crate::pipeline::TestRun {
            agent_id: "coder-1".to_string(),
            graph: graph.clone(),
            seeds: [("task".to_string(), "fix the bug".to_string())].into(),
            stages,
            global_hints: crate::test_support::hints(true),
            global_nudge: Default::default(),
            region_scripts: HashMap::new(),
        },
    )
    .expect("coder spawns");
    let spec = spawned.get::<RunSpecC>(a).unwrap().0.clone();
    let mut inserted = World::new();
    let b = insert(
        &mut inserted,
        spec.clone(),
        Bindings::new(),
        &initial_state(&spec),
    );
    assert_eq!(placement(&spawned, a), placement(&inserted, b));

    // And it is the run the graph describes.
    let state = spawned.get::<AgentState>(a).unwrap();
    assert_eq!(state.current_stage, "discover");
    assert_eq!(
        (state.agent_id.as_str(), state.status.clone()),
        ("coder-1", AgentStatus::Active)
    );
    assert!(!state.current_visit.is_empty());
    assert_eq!(spawned.get::<StageCursor>(a).unwrap().index, 0);
    assert_eq!(spawned.get::<VisitCounts>(a).unwrap().0["discover"], 1);
    let ledger = &spawned.get::<StageLedger>(a).unwrap().0;
    assert_eq!(ledger.len(), graph.stages.len());
    assert_eq!(ledger[0].visits[0].id, state.current_visit);
    assert!(ledger.iter().skip(1).all(|r| r.visits.is_empty()));
    let window = spawned.get::<ContextWindow>(a).unwrap();
    let task = window.get_region("task").unwrap();
    assert_eq!(task.content[0].content.as_str(), "fix the bug");
    let prompt = window
        .regions
        .iter()
        .flat_map(|r| r.content.iter())
        .find(|e| e.content.as_str().starts_with("[Stage instructions:"))
        .expect("the entry stage's instructions are in place");
    assert!(prompt.content.as_str().contains("Before any planning"));
    let si = spawned.get::<StageInference>(a).unwrap();
    assert_eq!((si.provider_name.as_str(), si.model.as_str()), ("p", "m"));
    assert_eq!(si.fallbacks[0].provider_or_empty(), "q");
    assert!(
        spawned
            .get::<crate::components::ToolResultRoutingComponent>(a)
            .is_some()
    );
    assert_eq!(
        spawned
            .get::<crate::pipeline::StageIoBuffer>(a)
            .unwrap()
            .logs,
        vec![(0, "moved".to_string())]
    );
    assert!(
        !spawned
            .get::<crate::persistence::RunOutcomeFlags>(a)
            .unwrap()
            .0
            .no_output_tools
    );
    assert!(spawned.get::<ReadyToInfer>(a).is_some());
}

/// One region holding one of each kind of entry a context keeps.
fn context() -> crate::state::ContextState {
    let entry = |text: &str, kind: EntryKind| EntryState {
        text: text.to_string(),
        parts: vec![],
        tokens: 2,
        timestamp: 9,
        kind,
        meta: EntryMeta::None,
        key: None,
        reasoning: None,
    };
    let call = ToolCallState {
        id: "c1".into(),
        name: "read_file".into(),
        args: leviath_core::JsonDoc::new(serde_json::json!({"path": "a"})),
        thought_signature: None,
    };
    let mut checklist = entry("one", EntryKind::Text);
    checklist.meta = EntryMeta::ChecklistItem {
        id: 1,
        done: false,
        note: None,
    };
    crate::state::ContextState {
        regions: vec![
            RegionState {
                name: crate::spec::names::RegionName::new("task").unwrap(),
                max_tokens: 500,
                current_tokens: 2,
                needs_message_compaction: false,
                taint: None,
                entries: vec![checklist],
            },
            RegionState {
                name: crate::spec::names::RegionName::new("conversation").unwrap(),
                max_tokens: 9000,
                current_tokens: 6,
                needs_message_compaction: true,
                taint: Some(crate::state::context::TaintState {
                    level: leviath_core::TaintLevel::Public,
                    entries: vec![leviath_core::TaintLevel::Public; 3],
                }),
                entries: vec![
                    entry("go", EntryKind::UserMessage),
                    entry("reading", EntryKind::AssistantTurn(vec![call])),
                    entry(
                        "text",
                        EntryKind::ToolResult {
                            call_id: "c1".into(),
                            tool: "read_file".into(),
                            is_error: false,
                        },
                    ),
                ],
            },
        ],
        hidden: vec![crate::spec::names::RegionName::new("task").unwrap()],
        max_tokens: 12_000,
    }
}

fn spend(tokens: u64) -> Spend {
    Spend {
        prompt_tokens: tokens,
        completion_tokens: 2,
        cached_tokens: 3,
        cache_write_tokens: 4,
        priced_usd: 0.5,
        reported_calls: 1,
        computed_calls: 1,
        unpriced_calls: 0,
    }
}

/// A run in its second visit to `build`, mid tool batch, fanned out, with a
/// message waiting and everything it has spent so far.
fn mid_run() -> RunState {
    let mut state = RunState::initial(sn("plan"), context(), true);
    state.seq = 41;
    state.status = RunStatus::Waiting;
    state.cursor = Cursor {
        stage: sn("build"),
        visit: "v-3".into(),
        iteration: 7,
    };
    state.phase = PipelinePhase::AwaitingTools;
    state.accepts_messages = false;
    state.visits = [(sn("plan"), 1), (sn("build"), 2)].into();
    state.progress.iterations = 3;
    state.progress.total_tool_calls = 5;
    state.progress.edits_by_path = [("a.rs".to_string(), 2)].into();
    state.progress.entry_region_digests = [("task".to_string(), 77)].into();
    state.ledger = vec![
        StageRecord {
            status: StageStatus::Complete,
            entered: true,
            spend: spend(10),
            models: vec![crate::spec::names::ModelRef::parse("p/m").unwrap()],
            visits: vec![VisitRecord {
                id: "v-1".into(),
                entered_at: 1,
                left_at: Some(2),
                spend: spend(10),
                clock: Clock {
                    banked_secs: 5,
                    since: None,
                },
            }],
            region_tokens: [("task".to_string(), 12)].into(),
            first_call_prompt_tokens: Some(10),
            started_at: Some(1),
            ended_at: Some(2),
            ..place::pending_stage(sn("plan"))
        },
        StageRecord {
            status: StageStatus::Active,
            entered: true,
            spend: Spend {
                unpriced_calls: 1,
                ..spend(20)
            },
            clock: Clock {
                banked_secs: 1,
                since: Some(3),
            },
            ..place::pending_stage(sn("build"))
        },
    ];
    state.pending = Some(PendingBatch {
        calls: vec![
            ToolCallState {
                id: "a".into(),
                name: "read_file".into(),
                args: leviath_core::JsonDoc::new(serde_json::json!({"path": "x"})),
                thought_signature: Some("sig".into()),
            },
            ToolCallState {
                id: "b".into(),
                name: "ask_user_text".into(),
                args: leviath_core::JsonDoc::default(),
                thought_signature: None,
            },
            ToolCallState {
                id: "c".into(),
                name: "shell".into(),
                args: leviath_core::JsonDoc::default(),
                thought_signature: None,
            },
        ],
        done: [
            (
                "a".to_string(),
                ToolResultState {
                    text: "contents".into(),
                    is_error: false,
                },
            ),
            (
                "c".to_string(),
                ToolResultState {
                    text: "denied".into(),
                    is_error: true,
                },
            ),
        ]
        .into(),
    });
    state.inbox = vec![MessageState {
        from: "person".into(),
        text: "also this".into(),
        region: Some("conversation".into()),
    }];
    state.totals = Totals {
        spend: spend(30),
        tool_calls: 8,
    };
    state.clock = Clock {
        banked_secs: 60,
        since: Some(100),
    };
    state.flags = Flags {
        modified_files: vec!["a.rs".into()],
        modified_file_count: 1,
        gates_forced: 2,
        broken_scripts: vec!["x.rhai".into()],
        ..Default::default()
    };
    state.children = vec![RunId::new("child-1").unwrap()];
    state.title = Some("Fix it".into());
    state.final_output = Some(FinalOutputState {
        content: "done".into(),
        format: Some("text".into()),
        stage: sn("plan"),
        submitted_at: 4,
        truncated: true,
        // A file of a type this build cannot name is left off the answer.
        artifacts: ["image/png", "not a type"]
            .into_iter()
            .map(|mime_type| crate::state::journal::ArtifactState {
                name: "hero".into(),
                path: "hero.png".into(),
                mime_type: mime_type.into(),
                size: 4,
                sha256: "ab".into(),
            })
            .collect(),
    });
    state.last_transition = Some(TransitionRecord {
        from: sn("plan"),
        to: sn("build"),
        edge: Some(EdgeName::new("next").unwrap()),
        reason: TransitionReason::Condition,
        visit: "v-3".into(),
    });
    state
}

/// Inserting a run partway through puts every part of its state back: where
/// it is, how often it has been everywhere, its tool batch with the results
/// already in, its context entry for entry, and its spend, flags and output.
#[test]
fn a_mid_run_state_is_placed_exactly() {
    let spec = Arc::new(two_stage_spec());
    let state = mid_run();
    let mut world = World::new();
    let e = insert(&mut world, spec.clone(), Bindings::new(), &state);

    assert_eq!(world.get::<StageCursor>(e).unwrap().index, 1);
    let agent = world.get::<AgentState>(e).unwrap();
    assert_eq!(
        (
            agent.current_stage.as_str(),
            agent.current_visit.as_str(),
            agent.iteration,
            agent.status.clone(),
            agent.accepts_messages,
            agent.spawned_children_ids.clone()
        ),
        (
            "build",
            "v-3",
            7,
            AgentStatus::Waiting,
            false,
            vec!["child-1".to_string()]
        )
    );
    let visits = &world.get::<VisitCounts>(e).unwrap().0;
    assert_eq!((visits["plan"], visits["build"]), (1, 2));
    let progress = world.get::<StageProgress>(e).unwrap();
    assert_eq!((progress.iterations, progress.total_tool_calls), (3, 5));
    assert_eq!(progress.edits_by_path["a.rs"], 2);
    assert_eq!(progress.entry_region_digests["task"], 77);

    // The context, entry for entry.
    let window = world.get::<ContextWindow>(e).unwrap();
    assert_eq!(window.to_state(), state.context);
    assert!(matches!(
        window.get_region("conversation").unwrap().kind,
        leviath_core::RegionKind::SlidingWindow { max_items: 50, .. }
    ));

    // The tool batch goes back out with the results that came in.
    let infer = world.get::<crate::components::InferenceResult>(e).unwrap();
    assert_eq!(
        infer
            .tool_calls
            .iter()
            .map(|c| c.tool_id.as_str())
            .collect::<Vec<_>>(),
        vec!["a", "b", "c"]
    );
    assert_eq!(
        infer.tool_calls[0].thought_signature.as_deref(),
        Some("sig")
    );
    let recovered = &world.get::<crate::pipeline::RecoveredResults>(e).unwrap().0;
    assert_eq!(recovered.len(), 2);
    assert_eq!(recovered[0].1.as_str(), "contents");
    assert_eq!(recovered[1].1.as_str(), "[error] denied");
    assert!(world.get::<crate::pipeline::ReadyForTools>(e).is_some());
    assert!(world.get::<ReadyToInfer>(e).is_none());

    // The current stage's inference, and the run's spend and record.
    assert_eq!(world.get::<StageInference>(e).unwrap().model, "builder");
    let totals = world.get::<crate::persistence::TokenTotals>(e).unwrap();
    assert_eq!((totals.prompt_tokens, totals.tool_calls), (30, 8));
    assert_eq!(totals.cost.computed_calls, 1);
    let clock = world.get::<crate::persistence::RunClock>(e).unwrap().0;
    assert_eq!((clock.banked_secs, clock.since), (60, Some(100)));
    let flags = &world
        .get::<crate::persistence::RunOutcomeFlags>(e)
        .unwrap()
        .0;
    assert_eq!((flags.gates_forced, flags.modified_file_count), (2, 1));
    assert_eq!(flags.broken_scripts, vec!["x.rhai".to_string()]);
    let out = &world.get::<crate::persistence::FinalOutput>(e).unwrap().0;
    assert_eq!(
        (out.content.as_str(), out.stage.as_str(), out.truncated),
        ("done", "plan", true)
    );
    assert_eq!(out.artifacts.len(), 1);
    assert_eq!(out.artifacts[0].mime_type.as_str(), "image/png");
    assert_eq!(
        world.get::<LastTransition>(e).unwrap().0,
        state.last_transition.clone().unwrap()
    );
    let md = world.get::<crate::persistence::RunMetadata>(e).unwrap();
    assert_eq!(md.title.as_deref(), Some("Fix it"));
    let inbox = &world
        .get::<crate::components::MessageInbox>(e)
        .unwrap()
        .messages;
    assert_eq!(
        (
            inbox[0].content.as_str(),
            inbox[0].target_region.as_deref(),
            inbox[0].agent_id.as_str()
        ),
        ("also this", Some("conversation"), "t-1")
    );
    assert!(
        world
            .get::<crate::pipeline::StageIoBuffer>(e)
            .unwrap()
            .logs
            .is_empty(),
        "spawn notes belong to a new run"
    );

    // The ledger, stage by stage.
    let ledger = &world.get::<StageLedger>(e).unwrap().0;
    assert_eq!(
        ledger[0].status,
        leviath_core::run_meta::StageRunStatus::Complete
    );
    assert_eq!(ledger[0].visit_count, 1);
    assert_eq!(ledger[0].visits[0].left_at, Some(2));
    assert_eq!(ledger[0].visits[0].active.unwrap().banked_secs, 5);
    assert_eq!(
        (ledger[0].cost_usd, ledger[0].cost_is_exact),
        (Some(0.5), false)
    );
    assert_eq!(ledger[0].models[0].provider, "p");
    assert_eq!(ledger[0].region_tokens["task"], 12);
    assert_eq!(
        ledger[1].visit_count, 2,
        "the state's visit count, past the recorded visits"
    );
    assert_eq!(
        ledger[1].cost_usd, None,
        "an unpriced call leaves the cost unknown"
    );
    assert_eq!(ledger[1].active.unwrap().since, Some(3));
    assert!(ledger[0].active.is_none(), "a stage clock that never ran");
}

/// A fan-out in progress comes back under the stage that started it, with
/// its queue, its finished and failed items, and the workers already placed.
#[test]
fn a_fan_out_in_progress_is_placed_with_its_workers() {
    let spec = Arc::new(two_stage_spec());
    let config = crate::spec::graph::FanOutDef {
        worker: crate::spec::graph::WorkerSource::Stage(sn("build")),
        merge_stage: Some(sn("build")),
        max_workers: Some(3),
        on_worker_failure: crate::spec::graph::WorkerFailure::FailAll,
        split_prompt: "split".into(),
        results_region: Some(crate::spec::names::RegionName::new("task").unwrap()),
        max_items: Some(9),
        max_attempts: Some(2),
    };
    let mut state = mid_run();
    state.phase = PipelinePhase::FanOut;
    state.pending = None;
    state.fan_out = Some(FanOutState {
        stage: sn("plan"),
        config: config.clone(),
        max_workers: Some(3),
        queued: vec![WorkItemState {
            id: "q1".into(),
            inputs: crate::spec::inputs::InputValues(
                [(
                    crate::spec::names::InputName::new("topic").unwrap(),
                    crate::spec::inputs::InputValue::Text("rust".into()),
                )]
                .into_iter()
                .collect(),
            ),
        }],
        active: vec![
            ("a1".into(), RunId::new("worker-1").unwrap()),
            ("a2".into(), RunId::new("worker-gone").unwrap()),
        ],
        done: vec![("d1".into(), "summary".into())],
        failed: vec![("f1".into(), "broke".into())],
        paused: true,
        origin: crate::fanout::FanOutOrigin::Tool {
            call_id: "call-9".into(),
        },
        parts: vec![crate::state::context::PartState {
            mime_type: "text/plain".into(),
            body: crate::state::context::PartBody::Inline("handed up".into()),
            name: Some("notes.txt".into()),
            deliver: None,
        }],
    });
    let mut world = World::new();
    world.spawn(AgentState {
        agent_id: "worker-1".into(),
        current_stage: "build".into(),
        current_visit: String::new(),
        iteration: 0,
        status: AgentStatus::Active,
        spawned_children_ids: vec![],
        pending_wait: None,
        accepts_messages: true,
    });
    let e = insert(&mut world, spec, Bindings::new(), &state);
    let restored = world
        .get::<crate::fanout::FanOutWaiting>(e)
        .unwrap()
        .to_state();
    assert_eq!(restored.config, config);
    assert_eq!(restored.max_workers, Some(3));
    assert_eq!(restored.pending[0].id, "q1");
    assert_eq!(
        restored.pending[0].inputs["topic"],
        crate::spec::inputs::RawInput::Text("rust".into())
    );
    assert_eq!(
        restored.active,
        vec![("a1".to_string(), "worker-1".to_string())]
    );
    assert_eq!(
        restored.summaries,
        vec![("d1".to_string(), "summary".to_string())]
    );
    assert_eq!(
        restored.failures.len(),
        2,
        "a worker that is not here is counted failed"
    );
    assert!(restored.paused);
    assert!(world.get::<ReadyToInfer>(e).is_none());
    assert!(world.get::<crate::pipeline::ReadyForTools>(e).is_none());
}

/// A fan-out a `fan_out` call started keeps the call's own worker, whatever
/// the stage it runs in declares.
#[test]
fn a_tool_started_fan_out_keeps_the_calls_worker() {
    for worker in [
        crate::spec::graph::WorkerSource::Blueprint(
            crate::spec::names::BlueprintRef::parse("helper").unwrap(),
        ),
        crate::spec::graph::WorkerSource::Query("find one".into()),
    ] {
        let mut config = crate::spec::graph::FanOutDef::same_graph(sn("build"));
        config.worker = worker;
        let mut state = mid_run();
        state.fan_out = Some(FanOutState {
            stage: sn("build"),
            config: config.clone(),
            max_workers: Some(5),
            queued: vec![],
            active: vec![],
            done: vec![],
            failed: vec![],
            paused: false,
            origin: Default::default(),
            parts: vec![],
        });
        let mut world = World::new();
        let e = insert(
            &mut world,
            Arc::new(two_stage_spec()),
            Bindings::new(),
            &state,
        );
        let restored = world
            .get::<crate::fanout::FanOutWaiting>(e)
            .unwrap()
            .to_state();
        assert_eq!(restored.max_workers, Some(5));
        assert_eq!(restored.config, config);
    }
}

/// Each phase puts the run in front of the system that drives it next.
#[test]
fn each_phase_places_the_marker_that_drives_it() {
    let spec = Arc::new(two_stage_spec());
    let place_at = |phase: PipelinePhase| {
        let mut state = mid_run();
        state.pending = None;
        state.phase = phase;
        let mut world = World::new();
        let e = insert(&mut world, spec.clone(), Bindings::new(), &state);
        (world, e)
    };
    for phase in [
        PipelinePhase::ReadyToInfer,
        PipelinePhase::AwaitingInference,
        PipelinePhase::AwaitingTools,
        PipelinePhase::AwaitingCompaction,
        PipelinePhase::AwaitingPerson,
        PipelinePhase::Wedged("stuck".into()),
        PipelinePhase::Paused,
    ] {
        let (world, e) = place_at(phase.clone());
        assert!(world.get::<ReadyToInfer>(e).is_some(), "{phase:?}");
    }
    let (world, e) = place_at(PipelinePhase::WaitingForChildren);
    assert!(world.get::<WaitingForChildren>(e).is_some());
    let (world, e) = place_at(PipelinePhase::Done);
    assert!(world.get::<ReadyToInfer>(e).is_none());
    let mut spec2 = two_stage_spec();
    spec2
        .graph
        .edges
        .push(crate::spec::graph::tests::edge("back", "build", "plan"));
    spec2
        .graph
        .edges
        .push(crate::spec::graph::tests::edge("again", "build", "build"));
    let mut state = mid_run();
    state.phase = PipelinePhase::AwaitingChoice(vec![EdgeName::new("back").unwrap()]);
    let mut world = World::new();
    let e = insert(&mut world, Arc::new(spec2), Bindings::new(), &state);
    let choice = &world.get::<AwaitingTransitionChoice>(e).unwrap().0;
    assert_eq!(choice.len(), 1);
    assert_eq!(choice[0].name.as_str(), "back");
}

/// A run that was waiting on a person is placed working, so it asks again;
/// one waiting on anything else is placed as it was, and either way what its
/// file says it was doing is what a summary of it reads.
#[test]
fn a_run_waiting_on_a_person_is_placed_to_ask_again() {
    use crate::state::WaitState;
    let spec = Arc::new(two_stage_spec());
    for (reason, placed) in [
        (WaitState::UserPrompt, AgentStatus::Active),
        (WaitState::ToolApproval, AgentStatus::Active),
        (WaitState::InteractionPoint, AgentStatus::Active),
        (WaitState::TaintGate, AgentStatus::Active),
        (WaitState::FanOutWorkers(2), AgentStatus::Waiting),
        (WaitState::Children(1), AgentStatus::Waiting),
    ] {
        let mut state = mid_run();
        state.wait_reason = Some(reason.clone());
        let mut world = World::new();
        let e = insert(&mut world, spec.clone(), Bindings::new(), &state);
        assert_eq!(
            world.get::<AgentState>(e).unwrap().status,
            placed,
            "{reason:?}"
        );
        assert_eq!(
            place::agent_state(&spec, &state).status,
            AgentStatus::Waiting
        );
    }
}

/// A paused run whose tool batch was in flight keeps it: the batch is placed
/// to be dispatched once the run is resumed, and reading the run back shows
/// it, so the step its placing records does not drop it.
#[test]
fn a_paused_run_keeps_its_batch_in_flight() {
    let spec = Arc::new(two_stage_spec());
    let mut state = mid_run();
    state.status = RunStatus::Paused;
    state.phase = PipelinePhase::Paused;
    let batch = state
        .pending
        .clone()
        .expect("the mid-run state has a batch");
    let mut world = World::new();
    let e = insert(&mut world, spec, Bindings::new(), &state);
    let read = crate::state::inspect::inspect(&world, e).unwrap();
    let pending = read.pending.expect("the batch is still in flight");
    assert_eq!(pending.calls, batch.calls);
    assert_eq!(
        pending.done.keys().collect::<Vec<_>>(),
        batch.done.keys().collect::<Vec<_>>()
    );
    // A failed call reads back marked as one, once.
    assert_eq!(pending.done["c"].text, "[error] denied");
    assert!(pending.done["c"].is_error);
    assert_eq!(read.phase, PipelinePhase::Paused);
}

/// A run paused until the machine is fixed keeps that reason when it is
/// placed again: the marker the reason is read from goes on with it, so
/// reading it back, as the step its placing records does, says the same.
#[test]
fn a_run_paused_until_the_machine_is_fixed_keeps_its_reason() {
    use crate::state::WaitState;
    let spec = Arc::new(two_stage_spec());
    let mut state = mid_run();
    state.status = RunStatus::Paused;
    state.phase = PipelinePhase::Paused;
    let reason = WaitState::NeedsSetup {
        blocker: leviath_core::run_meta::SetupBlocker::ProviderFailed,
        remedy: "`lev resume` this run".to_string(),
    };
    state.wait_reason = Some(reason.clone());
    let mut world = World::new();
    let e = insert(&mut world, spec, Bindings::new(), &state);
    let marker = world
        .get::<crate::pipeline::PausedForSetup>(e)
        .expect("the marker is placed");
    assert_eq!(
        marker.blocker,
        leviath_core::run_meta::SetupBlocker::ProviderFailed
    );
    let read = crate::state::inspect::inspect(&world, e).unwrap();
    assert_eq!(read.wait_reason, Some(reason));
}

/// A run stopped at a checkpoint is placed to ask it again over the document
/// it showed, at the checkpoint and round it had reached.
#[test]
fn a_checkpoint_put_to_a_person_is_placed_to_be_asked_again() {
    use crate::interaction_points::{
        InteractionPointCursor, InteractionPointRounds, ReadyForInteractionPoint,
    };
    let spec = Arc::new(two_stage_spec());
    let mut state = mid_run();
    state.pending = None;
    state.phase = PipelinePhase::AwaitingPerson;
    state.point = crate::state::PointProgress {
        cursor: 1,
        round: 2,
        asking: Some("## Plan".into()),
    };
    let mut world = World::new();
    let e = insert(&mut world, spec.clone(), Bindings::new(), &state);
    assert!(world.get::<ReadyForInteractionPoint>(e).is_some());
    assert!(world.get::<ReadyToInfer>(e).is_none());
    assert_eq!(
        world
            .get::<crate::components::InferenceResult>(e)
            .unwrap()
            .response,
        "## Plan"
    );
    assert_eq!(world.get::<InteractionPointCursor>(e).unwrap().0, 1);
    assert_eq!(world.get::<InteractionPointRounds>(e).unwrap().0, 2);
    let read = crate::state::inspect::inspect(&world, e).unwrap().point;
    assert_eq!((read.cursor, read.round), (1, 2));

    // At the first checkpoint with no revisions, and asking nothing, nothing
    // about checkpoints is placed.
    state.point = Default::default();
    let mut world = World::new();
    let e = insert(&mut world, spec, Bindings::new(), &state);
    assert!(world.get::<InteractionPointCursor>(e).is_none());
    assert!(world.get::<InteractionPointRounds>(e).is_none());
    assert!(world.get::<ReadyToInfer>(e).is_some());
}

#[test]
fn every_run_status_reads_as_the_agent_status_it_names() {
    for (status, label) in [
        (RunStatus::Idle, "idle"),
        (RunStatus::Active, "active"),
        (RunStatus::Waiting, "waiting"),
        (RunStatus::Paused, "paused"),
        (RunStatus::Complete, "complete"),
        (RunStatus::Error("x".into()), "error"),
        (RunStatus::Cancelled, "cancelled"),
    ] {
        assert_eq!(place::agent_status(&status).label(), label);
    }
    assert_eq!(
        place::agent_status(&RunStatus::Error("boom".into())),
        AgentStatus::Error {
            message: "boom".into()
        }
    );
}

/// What the spec alone decides lands with the run: compaction, loop detection,
/// input capture, re-scan markers, and the run's record of who and where it is.
#[test]
fn the_spec_decides_compaction_loops_capture_rescans_and_the_record() {
    use crate::spec::launch::{Callback, Unattended};
    let mut spec = two_stage_spec();
    spec.graph.compaction = Some(crate::spec::graph::CompactionDef {
        model: crate::spec::names::ModelRef::parse("p/summarizer").unwrap(),
        system_prompt: Some("sum".into()),
        user_prompt_template: None,
        max_summary_tokens: 300,
        temperature: 0.1,
    });
    spec.graph.repetition = Some(crate::spec::graph::RepetitionDef {
        enabled: Some(true),
        max_repeat_calls: Some(2),
        max_readonly_streak: Some(4),
    });
    spec.graph.tool_rescan = ToolRescan::BeforeDispatch;
    spec.launch.capture_model_input = true;
    spec.launch.unattended =
        Unattended::Profile(crate::spec::names::ProfileName::new("ci").unwrap());
    spec.placement.parent = Some(RunId::new("parent-1").unwrap());
    spec.requested_output = Some(Default::default());
    spec.seeded.insert(
        crate::spec::names::RegionName::new("task").unwrap(),
        crate::spec::run_spec::SeededContent {
            text: "the task".into(),
            parts: vec![],
        },
    );
    let spec = Arc::new(spec);
    let mut world = World::new();
    let e = insert(
        &mut world,
        spec.clone(),
        Bindings::new(),
        &initial_state(&spec),
    );
    let c = &world
        .get::<crate::pipeline::CompactionSettings>(e)
        .unwrap()
        .0;
    assert_eq!(
        (c.provider.as_str(), c.model.as_str(), c.max_summary_tokens),
        ("p", "summarizer", 300)
    );
    assert!(
        world
            .get::<crate::repetition::RepetitionDetector>(e)
            .is_some()
    );
    assert!(world.get::<crate::pipeline::CaptureModelInput>(e).is_some());
    assert!(world.get::<crate::pipeline::DynamicTools>(e).is_some());
    assert!(
        world
            .get::<crate::pipeline::RescanBeforeDispatch>(e)
            .is_some()
    );
    let md = world.get::<crate::persistence::RunMetadata>(e).unwrap();
    assert_eq!(md.agent_name, "coder");
    assert_eq!(
        md.blueprint_digest.as_deref(),
        Some(crate::spec::names::Digest::of(b"code").as_str())
    );
    assert_eq!(md.task, "the task");
    assert_eq!(md.workdir, "/tmp/w");
    assert_eq!(md.parent_run_id.as_deref(), Some("parent-1"));
    assert_eq!(md.callback_url.as_deref(), Some("https://x.dev/h"));
    assert_eq!(md.callback_secret.as_deref(), Some("s"));
    assert!(md.unattended);
    assert_eq!(md.yolo_profile.as_deref(), Some("ci"));
    assert!(md.output_request.is_some());
    assert_eq!(md.model_override.as_deref(), Some("mock/gpt-mock"));
    assert_eq!(md.metadata["team"], "a");

    // A raw graph, run attended, with no compaction or loop detection, takes
    // the graph's title and places none of the spec's optional components.
    let mut raw = two_stage_spec();
    raw.origin = crate::spec::run_spec::SpecOrigin::Raw;
    raw.launch.unattended = Unattended::Off;
    raw.delivery.callback = Some(Callback {
        url: crate::spec::names::HttpUrl::new("https://y.dev").unwrap(),
        secret: None,
    });
    raw.graph.tool_rescan = ToolRescan::AfterWrites;
    let raw = Arc::new(raw);
    let mut world = World::new();
    let e = insert(
        &mut world,
        raw.clone(),
        Bindings::new(),
        &initial_state(&raw),
    );
    let md = world.get::<crate::persistence::RunMetadata>(e).unwrap();
    assert_eq!(
        (md.agent_name.as_str(), md.blueprint_digest.clone()),
        ("t", None)
    );
    assert!(!md.unattended && md.yolo_profile.is_none());
    assert!(md.callback_secret.is_none());
    assert!(world.get::<crate::pipeline::DynamicTools>(e).is_some());
    assert!(
        world
            .get::<crate::pipeline::RescanBeforeDispatch>(e)
            .is_none()
    );
    assert!(
        world
            .get::<crate::pipeline::CompactionSettings>(e)
            .is_none()
    );
    let mut untitled = (*raw).clone();
    untitled.graph.title = None;
    untitled.launch.unattended = Unattended::All;
    let untitled = Arc::new(untitled);
    let mut world = World::new();
    let e = insert(
        &mut world,
        untitled.clone(),
        Bindings::new(),
        &initial_state(&untitled),
    );
    let md = world.get::<crate::persistence::RunMetadata>(e).unwrap();
    assert_eq!(md.agent_name, "");
    assert!(md.unattended && md.yolo_profile.is_none());
}

/// A new run starts at its graph's entry stage, with its seeds in, its parts
/// stored, taint tracking on when the graph asks, and the stage's instructions
/// left out (rather than the run refused) when they cannot fit.
#[test]
fn a_new_run_starts_seeded_at_its_entry_stage() {
    let mut spec = two_stage_spec();
    spec.graph.entry = Some(sn("build"));
    spec.graph.taint_tracking = Some(true);
    spec.graph.stages[1].system_prompt = Some("w".repeat(100_000));
    let part = crate::state::context::PartState {
        mime_type: "image/png".into(),
        body: crate::state::context::PartBody::Inline("x".into()),
        name: Some("a.png".into()),
        deliver: None,
    };
    spec.seeded.insert(
        crate::spec::names::RegionName::new("system").unwrap(),
        crate::spec::run_spec::SeededContent {
            text: String::new(),
            parts: vec![part],
        },
    );
    spec.seeded.insert(
        crate::spec::names::RegionName::new("task").unwrap(),
        crate::spec::run_spec::SeededContent {
            text: "t".repeat(10_000),
            parts: vec![],
        },
    );
    let state = initial_state(&spec);
    assert_eq!(state.cursor.stage, sn("build"));
    assert_eq!(state.visits[&sn("build")], 1);
    assert_eq!(state.ledger[1].visits.len(), 1);
    assert!(state.ledger[0].visits.is_empty());
    assert_eq!(state.status, RunStatus::Active);
    let system = state.context.region("system").unwrap();
    assert_eq!(system.entries.len(), 1, "a part with no text is one entry");
    assert!(system.taint.is_some());
    let task = state.context.region("task").unwrap();
    assert!(
        task.entries[0]
            .text
            .ends_with("seed exceeded this region's budget]")
    );
    assert!(
        state
            .context
            .regions
            .iter()
            .flat_map(|r| r.entries.iter())
            .all(|e| !e.text.starts_with("[Stage instructions:")),
        "a prompt that cannot fit is left out"
    );
}

#[test]
fn a_seed_is_trimmed_to_its_region_or_dropped_when_nothing_fits() {
    use crate::context_setup::fit_seed_to_budget;
    assert_eq!(fit_seed_to_budget("short", 100), "short");
    let trimmed = fit_seed_to_budget(&"x".repeat(1000), 50);
    assert!(trimmed.ends_with("seed exceeded this region's budget]"));
    assert!(trimmed.len() <= 49 * 4);
    assert_eq!(fit_seed_to_budget(&"x".repeat(1000), 3), "");
}

#[test]
fn a_cursor_naming_no_stage_reads_as_the_first() {
    let spec = two_stage_spec();
    let mut state = mid_run();
    state.cursor.stage = sn("elsewhere");
    assert_eq!(place::stage_index(&spec, &state), 0);
}

#[test]
fn every_stage_status_reads_as_the_ledger_status_it_names() {
    use leviath_core::run_meta::StageRunStatus as S;
    let mut state = mid_run();
    let all = [
        (StageStatus::Pending, S::Pending),
        (StageStatus::Active, S::Active),
        (StageStatus::WaitingInput, S::WaitingInput),
        (StageStatus::Complete, S::Complete),
        (StageStatus::Error, S::Error),
        (StageStatus::Skipped, S::Skipped),
    ];
    state.ledger = all
        .iter()
        .map(|(status, _)| StageRecord {
            status: *status,
            ..place::pending_stage(sn("plan"))
        })
        .collect();
    let ledger = place::stage_ledger(&state).0;
    for (rec, (_, want)) in ledger.iter().zip(all) {
        assert_eq!(rec.status, want);
    }
}

/// What the run answers for itself lands as the markers the pipeline reads:
/// checkpoints that approve themselves, and gate prompts that do on a run
/// under the taint gate.
#[test]
fn the_runs_own_answers_land_as_markers() {
    use crate::components::{GateAutoApprove, InteractionAutoApprove};
    use crate::spec::run_spec::AutoAnswers;
    let placed = |answers: AutoAnswers, taint: Option<bool>| {
        let mut spec = two_stage_spec();
        spec.auto_answers = answers;
        spec.graph.taint_tracking = taint;
        let spec = Arc::new(spec);
        let mut world = World::new();
        let e = insert(
            &mut world,
            spec.clone(),
            Bindings::new(),
            &initial_state(&spec),
        );
        (
            world.get::<InteractionAutoApprove>(e).is_some(),
            world.get::<GateAutoApprove>(e).is_some(),
        )
    };
    assert_eq!(placed(AutoAnswers::all(), Some(true)), (true, true));
    assert_eq!(placed(AutoAnswers::all(), None), (true, false), "no gate");
    assert_eq!(placed(AutoAnswers::default(), Some(true)), (false, false));
    let gate_only = AutoAnswers {
        gate: true,
        ..AutoAnswers::default()
    };
    assert_eq!(placed(gate_only, Some(true)), (false, true));
}

/// A fresh top-level run with a task asks for a title when the host bound the
/// chain a title call walks; nothing else does.
#[test]
fn a_new_named_run_asks_for_a_title_only_when_the_host_wants_one() {
    use crate::title::{PendingTitle, TitleCandidates};
    let tasked = || {
        let mut spec = two_stage_spec();
        spec.placement.parent = None;
        spec.seeded.insert(
            crate::spec::names::RegionName::new("task").unwrap(),
            crate::spec::run_spec::SeededContent {
                text: "do it".into(),
                parts: vec![],
            },
        );
        spec
    };
    let chain = || Bindings::new().with(TitleCandidates(vec![("p".into(), "m".into())]));
    let asks = |spec: RunSpec, bindings: Bindings, state: Option<RunState>| {
        let spec = Arc::new(spec);
        let state = state.unwrap_or_else(|| initial_state(&spec));
        let mut world = World::new();
        let e = insert(&mut world, spec, bindings, &state);
        world.get::<PendingTitle>(e).is_some()
    };
    assert!(asks(tasked(), chain(), None));
    assert!(!asks(tasked(), Bindings::new(), None), "titles are off");
    let mut child = tasked();
    child.placement.parent = Some(RunId::new("p-1").unwrap());
    assert!(!asks(child, chain(), None), "a child is not titled");
    let mut blank = tasked();
    blank.seeded.clear();
    assert!(!asks(blank, chain(), None), "nothing to name it after");
    let spec = tasked();
    let mut resumed = initial_state(&spec);
    resumed.seq = 4;
    assert!(!asks(spec.clone(), chain(), Some(resumed)), "had its turn");
    let mut named = initial_state(&spec);
    named.title = Some("Named".into());
    assert!(!asks(spec, chain(), Some(named)));
}

/// A run is named after the task it was given, whatever region its `task`
/// input fills: the researcher puts it in `query`, the data analyst in
/// `subject`. A task that is a file has no text to be named after.
#[test]
fn a_run_is_titled_from_its_task_input_whatever_region_it_fills() {
    use crate::spec::inputs::InputValue;
    use crate::title::{PendingTitle, TitleCandidates};
    let given = |value: InputValue| {
        let mut spec = two_stage_spec();
        spec.placement.parent = None;
        spec.seeded.clear();
        spec.seeded.insert(
            crate::spec::names::RegionName::new("query").unwrap(),
            crate::spec::run_spec::SeededContent {
                text: "how do tides work".into(),
                parts: vec![],
            },
        );
        spec.inputs
            .0
            .insert(crate::spec::names::InputName::new("task").unwrap(), value);
        let spec = Arc::new(spec);
        let mut world = World::new();
        let chain = Bindings::new().with(TitleCandidates(vec![("p".into(), "m".into())]));
        let e = insert(&mut world, spec.clone(), chain, &initial_state(&spec));
        let task = world
            .get::<crate::persistence::RunMetadata>(e)
            .unwrap()
            .task
            .clone();
        (world.get::<PendingTitle>(e).is_some(), task)
    };
    assert_eq!(
        given(InputValue::Text("how do tides work".into())),
        (true, "how do tides work".to_string())
    );
    assert_eq!(
        given(InputValue::File("tides.png".into())),
        (false, String::new()),
        "a media-first run stays untitled"
    );
}

/// A binding can fill a field of a component insertion placed, and does
/// nothing to an entity without that component.
#[test]
fn a_binding_edits_what_insertion_placed() {
    let spec = Arc::new(two_stage_spec());
    let mut world = World::new();
    let bindings = Bindings::new()
        .edit(|meta: &mut crate::persistence::RunMetadata| {
            meta.agent_path = "/agents/coder/agent.toml".into();
        })
        .edit(|m: &mut Marker| m.0 = 9);
    let e = insert(&mut world, spec.clone(), bindings, &initial_state(&spec));
    let md = world.get::<crate::persistence::RunMetadata>(e).unwrap();
    assert_eq!(md.agent_path, "/agents/coder/agent.toml");
    assert!(world.get::<Marker>(e).is_none());

    let bindings = Bindings::new()
        .with(Marker(1))
        .edit(|m: &mut Marker| m.0 = 9);
    let e = insert(&mut world, spec.clone(), bindings, &initial_state(&spec));
    assert_eq!(world.get::<Marker>(e), Some(&Marker(9)));
}

/// A fan-out worker starts in the stage it was started to run, not at the
/// graph's entry.
#[test]
fn a_worker_starts_in_its_worker_stage() {
    let graph = crate::test_graph::graph_of(
        "entry = \"a\"\n\
         edges = [{ name = \"next\", from = \"a\", to = \"b\" }]\n\
         layout = { total_budget_tokens = 1000, regions = [\
           { name = \"conversation\", kind = { kind = \"sliding_window\", max_items = 10 }, budget = 1000 },\
         ] }\n\
         [[stages]]\nname = \"a\"\nsystem_prompt = \"first\"\n\
         [[stages]]\nname = \"b\"\nsystem_prompt = \"second\"\nallow_as_worker = true\n",
    );
    let mut spec = crate::test_graph::spec_named("t", graph);
    assert_eq!(initial_state(&spec).cursor.stage.as_str(), "a");
    spec.placement.worker_stage = Some(crate::spec::names::StageName::new("b").unwrap());
    let state = initial_state(&spec);
    assert_eq!(state.cursor.stage.as_str(), "b");
    assert_eq!(state.visits.get("b"), Some(&1));
    // A stage the graph does not have leaves the run at its entry.
    spec.placement.worker_stage = Some(crate::spec::names::StageName::new("z").unwrap());
    assert_eq!(initial_state(&spec).cursor.stage.as_str(), "a");
}

/// A new run whose entry stage has an `on_stage_enter` hook waits for it
/// before its first request; a resumed one entered that stage long ago and
/// goes straight back to asking.
#[test]
fn only_a_new_run_waits_for_its_entry_stage_hook() {
    use crate::pipeline::EnteringEntryStage;
    let mut spec = crate::spec::run_spec::tests::spec();
    spec.graph.stages[0].hooks.on_stage_enter = Some(crate::spec::graph::CodeRef::Inline(
        "fn on_stage_enter(ctx) { () }".into(),
    ));
    let spec = Arc::new(spec);
    let fresh = initial_state(&spec);
    let mut resumed = fresh.clone();
    resumed.seq = 3;
    let mut world = World::new();
    let new_run = insert(&mut world, spec.clone(), Bindings::new(), &fresh);
    assert!(world.get::<EnteringEntryStage>(new_run).is_some());
    assert!(world.get::<ReadyToInfer>(new_run).is_none());
    let old_run = insert(&mut world, spec, Bindings::new(), &resumed);
    assert!(world.get::<EnteringEntryStage>(old_run).is_none());
    assert!(world.get::<ReadyToInfer>(old_run).is_some());
}
