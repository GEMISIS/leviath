use super::*;
use crate::components::{AwaitingInteraction, MessageInbox, SubAgentChildren};
use crate::dynamic_interaction::InteractionBackend as _;
use crate::pipeline as p;
use leviath_core::interaction::InteractionRequest;
use leviath_core::mime::{BlobRef, Delivery, MimeType};
use leviath_core::region::SerializedToolCall;
use leviath_core::taint::TaintLevel;
use leviath_core::{RegionKind, region::EntryKind as CoreKind};
use serde_json::json;

fn agent(stage: &str, status: AgentStatus) -> AgentState {
    AgentState {
        agent_id: "a".to_string(),
        current_visit: "v1".to_string(),
        current_stage: stage.to_string(),
        iteration: 3,
        status,
        spawned_children_ids: vec!["child-1".into(), "bad id".into()],
        pending_wait: None,
        accepts_messages: true,
    }
}

fn metadata() -> crate::persistence::RunMetadata {
    crate::persistence::RunMetadata {
        run_id: "r1".to_string(),
        agent_name: "a".to_string(),
        agent_path: "/p".to_string(),
        task: "t".to_string(),
        model: None,
        workdir: "/w".to_string(),
        num_stages: 1,
        started_at: 0,
        parent_run_id: None,
        metadata: Default::default(),
        callback_url: None,
        callback_secret: None,
        title: None,
        title_error: None,
        blueprint_digest: None,
        unattended: false,
        yolo_profile: None,
        read_paths: None,
        output_request: None,
        model_override: None,
    }
}

fn spawn(world: &mut World, status: AgentStatus) -> Entity {
    world.spawn(agent("plan", status)).id()
}

fn phase_with(bundle: impl bevy_ecs::bundle::Bundle) -> PipelinePhase {
    let mut world = World::new();
    let e = spawn(&mut world, AgentStatus::Active);
    world.entity_mut(e).insert(bundle);
    inspect(&world, e).unwrap().phase
}

#[test]
fn only_a_run_with_a_named_stage_is_inspected() {
    let mut world = World::new();
    let nothing = world.spawn_empty().id();
    assert!(inspect(&world, nothing).is_none());
    let nameless = world.spawn(agent("", AgentStatus::Idle)).id();
    assert!(inspect(&world, nameless).is_none());
}

#[test]
fn a_bare_run_reads_as_ready_with_every_default() {
    let mut world = World::new();
    let e = spawn(&mut world, AgentStatus::Idle);
    let s = inspect(&world, e).unwrap();
    assert_eq!(s.seq, 0);
    assert_eq!(s.status, RunStatus::Idle);
    assert_eq!(s.cursor.stage.as_str(), "plan");
    assert_eq!(s.cursor.visit, "v1");
    assert_eq!(s.cursor.iteration, 3);
    assert_eq!(s.phase, PipelinePhase::ReadyToInfer);
    assert!(s.accepts_messages);
    assert!(s.visits.is_empty() && s.ledger.is_empty() && s.inbox.is_empty());
    assert_eq!(s.context, ContextState::default());
    assert_eq!(s.pending, None);
    assert_eq!(s.fan_out, None);
    assert!(s.interactions.is_empty());
    assert_eq!(s.totals, Totals::default());
    assert_eq!(s.clock, Clock::default());
    assert_eq!(s.children, vec![RunId::new("child-1").unwrap()]);
    assert_eq!(s.title, None);
    assert_eq!(s.final_output, None);
    assert_eq!(s.wait_reason, None);
    assert_eq!(s.last_transition, None);
}

/// The edge a run last took is read from the component every move sets, so
/// the persisted state records it.
#[test]
fn the_last_move_is_read_from_the_run() {
    let mut world = World::new();
    let e = spawn(&mut world, AgentStatus::Active);
    let moved = TransitionRecord {
        from: StageName::new("plan").unwrap(),
        to: StageName::new("build").unwrap(),
        edge: Some(EdgeName::new("go").unwrap()),
        reason: super::super::TransitionReason::Condition,
        visit: "v2".to_string(),
    };
    world.entity_mut(e).insert(p::LastTransition(moved.clone()));
    assert_eq!(inspect(&world, e).unwrap().last_transition, Some(moved));
}

#[test]
fn every_status_has_its_word_and_the_finished_ones_are_done() {
    let cases = [
        (
            AgentStatus::Idle,
            RunStatus::Idle,
            PipelinePhase::ReadyToInfer,
        ),
        (
            AgentStatus::Active,
            RunStatus::Active,
            PipelinePhase::ReadyToInfer,
        ),
        (
            AgentStatus::Waiting,
            RunStatus::Waiting,
            PipelinePhase::ReadyToInfer,
        ),
        (
            AgentStatus::Paused,
            RunStatus::Paused,
            PipelinePhase::Paused,
        ),
        (
            AgentStatus::Complete,
            RunStatus::Complete,
            PipelinePhase::Done,
        ),
        (
            AgentStatus::Error {
                message: "boom".into(),
            },
            RunStatus::Error("boom".into()),
            PipelinePhase::Done,
        ),
        (
            AgentStatus::Cancelled,
            RunStatus::Cancelled,
            PipelinePhase::Done,
        ),
    ];
    for (status, word, phase) in cases {
        let mut world = World::new();
        let e = spawn(&mut world, status);
        let s = inspect(&world, e).unwrap();
        assert_eq!((s.status, s.phase), (word, phase));
    }
}

#[test]
fn each_pipeline_marker_reads_as_its_phase() {
    use leviath_core::run_meta::SetupBlocker;
    assert_eq!(
        phase_with(p::PausedForSetup {
            blocker: SetupBlocker::ProviderMissing,
            remedy: "add it".into(),
        }),
        PipelinePhase::Paused
    );
    assert_eq!(
        phase_with(p::Wedged { since: 5 }),
        PipelinePhase::Wedged("nothing has driven it since 5".into())
    );
    let stall = |reason| p::DispatchStall {
        since: 1,
        last_seen: 2,
        reason,
    };
    assert_eq!(
        phase_with(stall(p::StallReason::ProviderMissing)),
        PipelinePhase::Wedged("provider-missing".into())
    );
    assert_eq!(
        phase_with(stall(p::StallReason::PoolFull)),
        PipelinePhase::ReadyToInfer
    );
    assert_eq!(
        phase_with(AwaitingInteraction),
        PipelinePhase::AwaitingPerson
    );
    assert_eq!(
        phase_with(crate::gate_prompt::AwaitingGatePrompt(1)),
        PipelinePhase::AwaitingPerson
    );
    assert_eq!(
        phase_with(crate::gate_prompt::AwaitingGatePrompt(0)),
        PipelinePhase::ReadyToInfer
    );
    assert_eq!(
        phase_with(crate::interaction_points::AwaitingInteractionPoint),
        PipelinePhase::AwaitingPerson
    );
    assert_eq!(
        phase_with(p::WaitingForChildren),
        PipelinePhase::WaitingForChildren
    );
    assert_eq!(
        phase_with(p::AwaitingCompaction),
        PipelinePhase::AwaitingCompaction
    );
    assert_eq!(phase_with(p::AwaitingTools), PipelinePhase::AwaitingTools);
    assert_eq!(
        phase_with(p::AwaitingInference),
        PipelinePhase::AwaitingInference
    );
    assert_eq!(phase_with(p::ReadyToInfer), PipelinePhase::ReadyToInfer);
}

#[test]
fn a_choice_names_its_edges_as_the_graph_does() {
    let edge = |name: &str, to: &str| {
        let mut e = crate::spec::graph::tests::edge(name, "plan", to);
        e.when = crate::spec::graph::EdgeCondition::LlmChoice;
        e
    };
    let choice =
        p::AwaitingTransitionChoice(vec![edge("go_build", "build"), edge("ask", "review")]);
    assert_eq!(
        phase_with(choice),
        PipelinePhase::AwaitingChoice(vec![
            EdgeName::new("go_build").unwrap(),
            EdgeName::new("ask").unwrap()
        ])
    );
}

fn fan_out(world: &mut World, e: Entity) {
    let state = crate::fanout::FanOutState {
        config: serde_json::from_value(json!({"worker": {"stage": "plan"}})).unwrap(),
        max_workers: 2,
        pending: vec![
            crate::fanout::WorkItem {
                id: "i1".into(),
                inputs: [
                    ("topic".to_string(), RawInput::Text("x".into())),
                    ("bad key".to_string(), RawInput::Int(1)),
                ]
                .into(),
            },
            crate::fanout::WorkItem {
                id: "i2".into(),
                inputs: Default::default(),
            },
            crate::fanout::WorkItem {
                id: "i3".into(),
                inputs: [("task".to_string(), RawInput::Text("plain".into()))].into(),
            },
        ],
        active: vec![("i0".into(), "w-1".into()), ("i9".into(), "bad id".into())],
        summaries: vec![("i4".into(), "fine".into())],
        failures: vec![("i5".into(), "bad".into())],
        parts: vec![],
        paused: true,
        origin: Default::default(),
    };
    let resolve = |_: &str| Some(Entity::PLACEHOLDER);
    crate::fanout::restore_fan_out_waiting(world, e, state, &resolve);
}

#[test]
fn a_fan_out_reads_with_its_items_as_typed_inputs() {
    let mut world = World::new();
    let e = spawn(&mut world, AgentStatus::Waiting);
    fan_out(&mut world, e);
    let s = inspect(&world, e).unwrap();
    assert_eq!(s.phase, PipelinePhase::FanOut);
    let f = s.fan_out.unwrap();
    assert_eq!(f.stage.as_str(), "plan");
    assert_eq!(f.max_workers, 2);
    assert!(f.paused);
    assert_eq!(f.queued.len(), 3);
    assert_eq!(
        f.queued[0].inputs.get("topic"),
        Some(&InputValue::Text("x".into()))
    );
    assert!(f.queued[1].inputs.0.is_empty());
    assert_eq!(
        f.queued[2].inputs.get("task"),
        Some(&InputValue::Text("plain".into()))
    );
    assert_eq!(f.active, vec![("i0".into(), RunId::new("w-1").unwrap())]);
    assert_eq!(f.done, vec![("i4".into(), "fine".into())]);
    assert_eq!(f.failed, vec![("i5".into(), "bad".into())]);
    assert_eq!(s.wait_reason, Some(WaitState::FanOutWorkers(5)));
}

#[test]
fn raw_inputs_keep_the_type_they_arrived_with() {
    let raw: RawInput = serde_json::from_value(json!({
        "b": true, "i": -3, "f": 1.5, "s": "t",
        "l": [1, "x"], "r": {"k": 2, "bad key": 1}
    }))
    .unwrap();
    let RawInput::Record(map) = raw else {
        unreachable!()
    };
    let v = inputs_of(map);
    assert_eq!(v.get("b"), Some(&InputValue::Bool(true)));
    assert_eq!(v.get("i"), Some(&InputValue::Int(-3)));
    assert_eq!(v.get("f"), Some(&InputValue::Float(1.5)));
    assert_eq!(v.get("s"), Some(&InputValue::Text("t".into())));
    assert_eq!(
        v.get("l"),
        Some(&InputValue::List(vec![
            InputValue::Int(1),
            InputValue::Text("x".into())
        ]))
    );
    let InputValue::Record(r) = v.get("r").unwrap() else {
        unreachable!()
    };
    assert_eq!(r.len(), 1, "a key that is not an input name is left out");
}

fn entry(content: EntryContent, kind: CoreKind) -> RegionEntry {
    RegionEntry {
        content,
        tokens: 4,
        timestamp: 7,
        metadata: None,
        kind,
        key: None,
        reasoning: None,
    }
}

fn blob(sha: &str) -> BlobRef {
    BlobRef {
        sha256: sha.into(),
        mime_type: MimeType::parse("image/png").unwrap(),
        size: 3,
        width: Some(2),
        height: Some(2),
        duration_ms: None,
        tokens: 85,
        stand_in: "[image]".into(),
    }
}

fn busy_window() -> ContextWindow {
    let mut w = ContextWindow::new(5000);
    let mut conv = Region::new("conversation".into(), RegionKind::Clearable, 3000);
    let call = SerializedToolCall {
        id: "c1".into(),
        name: "read_file".into(),
        arguments: json!({"path": "a"}),
        thought_signature: Some("sig".into()),
    };
    let sha = Digest::of(b"png");
    conv.content = vec![
        entry(EntryContent::text("hi"), CoreKind::UserMessage),
        entry(
            EntryContent::text("thinking"),
            CoreKind::AssistantTurn { tool_calls: vec![] },
        ),
        entry(
            EntryContent::text("reading"),
            CoreKind::AssistantTurn {
                tool_calls: vec![call],
            },
        ),
        entry(
            EntryContent::from_parts(vec![
                Part::text("see"),
                Part::stored(blob(sha.as_str()))
                    .named("a.png")
                    .delivered(Delivery::Native),
                Part::stored(blob("not a sha")),
            ]),
            CoreKind::ToolResult {
                tool_call_id: "c1".into(),
                tool_name: "read_file".into(),
                is_error: false,
            },
        ),
    ];
    let mut taint = leviath_core::taint::RegionTaint::new();
    taint.add_entry(TaintLevel::Private);
    conv.taint = Some(taint);
    conv.needs_message_compaction = true;
    w.add_region(conv);
    let mut todo = Region::new("todo".into(), RegionKind::Checklist, 500);
    let mut item = entry(EntryContent::text("ship"), CoreKind::Text);
    item.metadata =
        Some(json!({"checklist_id": 2, "checklist_done": true, "checklist_note": "ok"}));
    item.key = Some("k".into());
    item.reasoning = Some("why".into());
    todo.content = vec![item];
    w.add_region(todo);
    w.add_region(Region::new(" bad".into(), RegionKind::Pinned, 10));
    w.hidden.insert("todo".into());
    w.hidden.insert(" bad".into());
    w
}

#[test]
fn the_context_reads_region_by_region_and_entry_by_entry() {
    let c = context_of(&busy_window());
    assert_eq!(c.max_tokens, 5000);
    assert_eq!(c.hidden, vec![RegionName::new("todo").unwrap()]);
    assert_eq!(c.regions.len(), 2);
    let conv = &c.regions[0];
    assert!(conv.needs_message_compaction);
    assert_eq!(
        conv.taint,
        Some(TaintState {
            level: TaintLevel::Private,
            entries: vec![TaintLevel::Private]
        })
    );
    assert_eq!(conv.entries[0].kind, EntryKind::UserMessage);
    assert!(conv.entries[0].parts.is_empty());
    let EntryKind::AssistantTurn(calls) = &conv.entries[2].kind else {
        unreachable!()
    };
    assert_eq!(calls[0].args.value(), &json!({"path": "a"}));
    assert_eq!(calls[0].thought_signature.as_deref(), Some("sig"));
    let result = &conv.entries[3];
    assert!(matches!(result.kind, EntryKind::ToolResult { .. }));
    // The part with no valid digest has nothing to name it by and is left out.
    assert_eq!(result.parts.len(), 2);
    assert_eq!(result.parts[1].name.as_deref(), Some("a.png"));
    assert_eq!(result.parts[1].deliver, Some(Delivery::Native));
    let todo = &c.regions[1].entries[0];
    assert_eq!(
        todo.meta,
        EntryMeta::ChecklistItem {
            id: 2,
            done: true,
            note: Some("ok".into())
        }
    );
    assert_eq!(todo.key.as_deref(), Some("k"));
    assert_eq!(todo.reasoning.as_deref(), Some("why"));
}

#[test]
fn an_entry_s_content_comes_back_from_its_state() {
    let plain = entry(EntryContent::text("hi"), CoreKind::Text);
    assert_eq!(content_of(&entry_of(&plain)), plain.content);
    let parts = EntryContent::from_parts(vec![
        Part::text("see"),
        Part::stored(blob(Digest::of(b"x").as_str())).named("x.png"),
    ]);
    let mixed = entry(parts.clone(), CoreKind::Text);
    assert_eq!(content_of(&entry_of(&mixed)), parts);
    // Plain text with a name is more than plain text.
    let named = entry(
        EntryContent::from_parts(vec![Part::text("n").named("n.txt")]),
        CoreKind::Text,
    );
    assert_eq!(entry_of(&named).parts.len(), 1);
    let delivered = EntryContent::from_parts(vec![Part::text("d").delivered(Delivery::Native)]);
    assert_eq!(entry_of(&entry(delivered, CoreKind::Text)).parts.len(), 1);
    let stored_alone =
        EntryContent::from_parts(vec![Part::stored(blob(Digest::of(b"y").as_str()))]);
    assert_eq!(
        entry_of(&entry(stored_alone, CoreKind::Text)).parts.len(),
        1
    );
    let markdown = EntryContent::from_parts(vec![Part::inline(
        MimeType::parse("text/markdown").unwrap(),
        "# h",
    )]);
    assert_eq!(entry_of(&entry(markdown, CoreKind::Text)).parts.len(), 1);
    // A part whose type no longer parses is left out on the way back.
    let mut state = entry_of(&mixed);
    state.parts[0].mime_type = "nonsense".into();
    assert_eq!(content_of(&state).parts().len(), 1);
}

fn ledger() -> p::StageLedger {
    use leviath_core::run_meta::{StageModelUse, StageRecord as Core, StageRunStatus as S};
    let mut records = Vec::new();
    for (i, status) in [
        S::Pending,
        S::Active,
        S::WaitingInput,
        S::Complete,
        S::Error,
        S::Skipped,
    ]
    .into_iter()
    .enumerate()
    {
        let mut r = Core::new(format!("s{i}"), i);
        r.status = status;
        records.push(r);
    }
    let r = &mut records[1];
    r.entered = true;
    r.prompt_tokens = 10;
    r.cost_priced_usd = 0.5;
    r.cost_is_exact = false;
    r.unpriced_calls = 1;
    r.computed_calls = 1;
    r.reported_calls = 2;
    r.models = vec![
        StageModelUse {
            provider: "mock".into(),
            model: "m".into(),
        },
        StageModelUse {
            provider: "".into(),
            model: "bare".into(),
        },
        StageModelUse {
            provider: "mock".into(),
            model: "".into(),
        },
    ];
    let mut visit = leviath_core::run_meta::StageVisitRecord::opened_at(5, "v1".into());
    visit.active = Some(leviath_core::run_meta::ActiveClock {
        banked_secs: 4,
        since: Some(9),
    });
    r.visits = vec![
        visit,
        leviath_core::run_meta::StageVisitRecord::opened_at(6, "v2".into()),
    ];
    r.region_tokens = [("conversation".to_string(), 12)].into();
    r.first_call_prompt_tokens = Some(10);
    r.active = Some(leviath_core::run_meta::ActiveClock {
        banked_secs: 1,
        since: None,
    });
    records.push(Core::new(" bad".into(), 6));
    p::StageLedger(records)
}

#[tokio::test]
async fn a_busy_run_reads_every_field_from_its_components() {
    let hub = crate::interaction_hub::InteractionHub::new();
    let mut world = World::new();
    world.insert_resource(hub.clone());
    let e = spawn(&mut world, AgentStatus::Waiting);
    let progress = p::StageProgress {
        total_tool_calls: 4,
        entry_region_digests: [("notes".to_string(), 99u64)].into(),
        edits_by_path: [("a.rs".to_string(), 2usize)].into(),
        stage_started_at: Some(1),
        ..Default::default()
    };
    let validators = crate::components::OutputValidators::new(Default::default());
    validators.note_broken("shape.rhai");
    let mut flags = crate::persistence::RunOutcomeFlags::default();
    flags.0.record_modification("a.rs");
    let totals = crate::persistence::TokenTotals {
        prompt_tokens: 10,
        completion_tokens: 2,
        cached_tokens: 1,
        cache_write_tokens: 0,
        tool_calls: 3,
        cost: Default::default(),
    };
    let mut inbox = MessageInbox::default();
    inbox.messages.push(crate::components::AgentMessage {
        agent_id: "a".into(),
        from: crate::components::FROM_PERSON.to_string(),
        content: "hurry".into(),
        target_region: Some("conversation".into()),
        parts: vec![],
    });
    world.entity_mut(e).insert((
        (
            p::VisitCounts([("plan".to_string(), 2usize), (" bad".to_string(), 1)].into()),
            progress,
            ledger(),
            busy_window(),
            inbox,
            totals,
            crate::persistence::RunClock(leviath_core::run_meta::ActiveClock {
                banked_secs: 30,
                since: Some(100),
            }),
            flags,
            validators,
        ),
        (
            crate::persistence::FinalOutput(leviath_core::output::FinalOutput {
                artifacts: vec![leviath_core::output::Artifact {
                    name: "hero".into(),
                    path: "hero.png".into(),
                    mime_type: MimeType::parse("image/png").unwrap(),
                    size: 4,
                    sha256: "ab".into(),
                }],
                ..leviath_core::output::FinalOutput::new(
                    "the answer",
                    Some("markdown".into()),
                    "plan".into(),
                    9,
                )
            }),
            crate::persistence::RunMetadata {
                title: Some("Fix it".into()),
                title_error: Some("an earlier try failed".into()),
                read_paths: Some(leviath_core::run_meta::ReadPathGrantCounts {
                    declared: 2,
                    granted: 1,
                }),
                ..metadata()
            },
            SubAgentChildren {
                children: vec![Entity::PLACEHOLDER],
                max_child_depth: 2,
            },
            p::WaitingForChildren,
        ),
    ));
    let other = hub.backend_for("someone-else");
    let mine = hub.backend_for("a");
    let mine2 = hub.backend_for("a");
    let asks = [
        tokio::spawn(async move {
            other
                .ask(InteractionRequest::free_text("z", "?", "s", true))
                .await
        }),
        tokio::spawn(async move {
            mine.ask(InteractionRequest::free_text("q2", "second?", "s", true))
                .await
        }),
        tokio::spawn(async move {
            mine2
                .ask(InteractionRequest::free_text("q1", "first?", "s", true))
                .await
        }),
    ];
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    let s = inspect(&world, e).unwrap();
    for ask in asks {
        ask.abort();
    }
    assert_eq!(s.visits, [(StageName::new("plan").unwrap(), 2)].into());
    assert_eq!(s.progress.total_tool_calls, 4);
    assert_eq!(s.progress.entry_region_digests["notes"], 99);
    assert_eq!(s.progress.edits_by_path["a.rs"], 2);
    assert_eq!(s.ledger.len(), 6);
    let statuses: Vec<StageStatus> = s.ledger.iter().map(|r| r.status).collect();
    assert_eq!(
        statuses,
        vec![
            StageStatus::Pending,
            StageStatus::Active,
            StageStatus::WaitingInput,
            StageStatus::Complete,
            StageStatus::Error,
            StageStatus::Skipped
        ]
    );
    let active = &s.ledger[1];
    assert_eq!(active.spend.computed_calls, 1);
    assert_eq!(active.spend.reported_calls, 2);
    assert_eq!(active.spend.unpriced_calls, 1);
    assert_eq!(active.models.len(), 2);
    assert_eq!(active.models[1].provider, None);
    assert_eq!(active.visits[0].clock.banked_secs, 4);
    assert_eq!(active.visits[1].clock, Clock::default());
    assert_eq!(active.region_tokens["conversation"], 12);
    assert_eq!(active.clock.banked_secs, 1);
    assert_eq!(s.ledger[0].spend.computed_calls, 0);
    assert_eq!(s.context.regions.len(), 2);
    assert_eq!(
        s.inbox,
        vec![MessageState {
            from: "user".into(),
            text: "hurry".into(),
            region: Some("conversation".into())
        }]
    );
    let asked: Vec<&str> = s.interactions.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(asked, vec!["q1", "q2"]);
    assert_eq!(s.totals.spend.prompt_tokens, 10);
    assert_eq!(s.totals.tool_calls, 3);
    assert_eq!(s.clock.banked_secs, 30);
    assert_eq!(s.flags.modified_files, vec!["a.rs".to_string()]);
    assert_eq!(s.flags.broken_scripts, vec!["shape.rhai".to_string()]);
    assert!(s.flags.produced_output);
    assert!(!s.flags.empty_output);
    assert_eq!(s.title.as_deref(), Some("Fix it"));
    assert_eq!(s.title_error.as_deref(), Some("an earlier try failed"));
    assert_eq!(
        s.read_paths,
        Some(super::super::ReadPathCounts {
            declared: 2,
            granted: 1
        })
    );
    let out = s.final_output.unwrap();
    assert_eq!(
        (out.content.as_str(), out.stage.as_str()),
        ("the answer", "plan")
    );
    assert_eq!(out.artifacts.len(), 1);
    assert_eq!(
        (
            out.artifacts[0].path.as_str(),
            out.artifacts[0].mime_type.as_str()
        ),
        ("hero.png", "image/png")
    );
    assert_eq!(s.phase, PipelinePhase::WaitingForChildren);
    assert_eq!(s.wait_reason, Some(WaitState::Children(1)));
}

#[test]
fn an_answer_from_a_stage_with_no_valid_name_is_left_out() {
    let mut world = World::new();
    let e = spawn(&mut world, AgentStatus::Complete);
    world.entity_mut(e).insert(crate::persistence::FinalOutput(
        leviath_core::output::FinalOutput::new("x", None, String::new(), 1),
    ));
    let s = inspect(&world, e).unwrap();
    assert_eq!(s.final_output, None);
    assert!(s.flags.produced_output);
}

#[test]
fn a_parked_run_says_why() {
    use leviath_core::run_meta::SetupBlocker;
    let mut world = World::new();
    let e = spawn(&mut world, AgentStatus::Waiting);
    world.entity_mut(e).insert(p::WaitingForChildren);
    assert_eq!(
        inspect(&world, e).unwrap().wait_reason,
        Some(WaitState::Children(0))
    );
    let paused = spawn(&mut world, AgentStatus::Paused);
    world.entity_mut(paused).insert(p::PausedForSetup {
        blocker: SetupBlocker::CreditsExhausted,
        remedy: "top up".into(),
    });
    assert_eq!(
        inspect(&world, paused).unwrap().wait_reason,
        Some(WaitState::NeedsSetup {
            blocker: SetupBlocker::CreditsExhausted,
            remedy: "top up".into(),
        })
    );
}

/// Every reason a run can be parked for reads into the run file's form and
/// back unchanged.
#[test]
fn every_wait_reason_survives_the_run_file() {
    use leviath_core::run_meta::{SetupBlocker, WaitReason};
    for reason in [
        WaitReason::ToolApproval,
        WaitReason::UserPrompt,
        WaitReason::TaintGate,
        WaitReason::InteractionPoint,
        WaitReason::FanOutWorkers { outstanding: 3 },
        WaitReason::Children { outstanding: 2 },
        WaitReason::NeedsSetup {
            blocker: SetupBlocker::AuthFailed,
            remedy: "a new key".into(),
        },
    ] {
        assert_eq!(WaitReason::from(&WaitState::from(&reason)), reason);
    }
}

/// The reply whose calls are being run: three calls, `c1` to `c3`.
fn tool_reply() -> crate::components::InferenceResult {
    let call = |id: &str| crate::components::ToolCall {
        tool_id: id.into(),
        name: "do".into(),
        arguments: json!({"n": id}),
        thought_signature: None,
    };
    crate::components::InferenceResult {
        attempt_id: String::new(),
        response: String::new(),
        tool_calls: vec![call("c1"), call("c2"), call("c3")],
        tokens_used: 0,
        cut_off_at: None,
        reasoning: None,
        parts: Vec::new(),
    }
}

#[test]
fn a_tool_batch_in_flight_reads_with_the_results_already_in() {
    let mut world = World::new();
    let e = spawn(&mut world, AgentStatus::Active);
    world.entity_mut(e).insert((
        p::AwaitingTools,
        tool_reply(),
        busy_window_with_old_calls(),
        p::ContextToolResults(vec![("c1".into(), "[error] nope".into())]),
        p::RecoveredResults(vec![("c2".into(), "fine".into())]),
    ));
    let pending = inspect(&world, e).unwrap().pending.unwrap();
    let ids: Vec<&str> = pending.calls.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, vec!["c1", "c2", "c3"]);
    assert_eq!(pending.calls[0].args.value(), &json!({"n": "c1"}));
    assert!(pending.done["c1"].is_error);
    assert!(!pending.done["c2"].is_error);
    assert_eq!(pending.done.len(), 2);
    // Without the reply there is no batch, whatever the window holds.
    let bare = spawn(&mut world, AgentStatus::Active);
    world
        .entity_mut(bare)
        .insert((p::AwaitingTools, busy_window_with_old_calls()));
    assert_eq!(inspect(&world, bare).unwrap().pending, None);
    let no_results = spawn(&mut world, AgentStatus::Active);
    world
        .entity_mut(no_results)
        .insert((p::AwaitingTools, tool_reply()));
    let batch = inspect(&world, no_results).unwrap().pending.unwrap();
    assert!(batch.done.is_empty());
}

/// A window whose last turn made a call that has already settled.
fn busy_window_with_old_calls() -> ContextWindow {
    let mut w = ContextWindow::new(1000);
    let mut conv = Region::new("conversation".into(), RegionKind::Clearable, 1000);
    conv.content = vec![
        entry(EntryContent::text("hi"), CoreKind::UserMessage),
        entry(
            EntryContent::text("one call"),
            CoreKind::AssistantTurn {
                tool_calls: vec![SerializedToolCall {
                    id: "old".into(),
                    name: "do".into(),
                    arguments: json!({}),
                    thought_signature: None,
                }],
            },
        ),
    ];
    w.add_region(conv);
    w
}
