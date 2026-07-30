//! Unit tests for the lifecycle observer.

use super::*;
use bevy_ecs::schedule::Schedule;
use leviath_core::run_meta::StageRecord;
use leviath_core::telemetry::MemorySink;

fn meta(run_id: &str) -> RunMetadata {
    RunMetadata {
        run_id: run_id.to_string(),
        agent_name: "coder".to_string(),
        agent_path: "/tmp/agents/coder".to_string(),
        task: "do the thing".to_string(),
        model: Some("mock/m".to_string()),
        workdir: "/tmp/w".to_string(),
        num_stages: 2,
        started_at: 1,
        parent_run_id: None,
        metadata: Default::default(),
        callback_url: None,
        callback_secret: None,
        title: None,
    }
}

fn agent(stage: &str, iteration: usize, status: AgentStatus) -> AgentState {
    AgentState {
        agent_id: "a1".to_string(),
        current_stage: stage.to_string(),
        iteration,
        status,
        spawned_children_ids: vec![],
        pending_wait: None,
        accepts_messages: false,
    }
}

fn world_with_sink() -> (World, std::sync::Arc<MemorySink>) {
    let mut world = World::new();
    let sink = std::sync::Arc::new(MemorySink::default());
    world.insert_resource(Telemetry(sink.clone()));
    (world, sink)
}

fn observe(world: &mut World) {
    let mut schedule = Schedule::default();
    schedule.add_systems(observe_lifecycle);
    schedule.run(world);
}

fn ledger_with(index: usize, prompt: usize, completion: usize) -> StageLedger {
    let mut rec = StageRecord::new(format!("stage{index}"), index);
    rec.prompt_tokens = prompt;
    rec.completion_tokens = completion;
    let mut records = Vec::new();
    for i in 0..index {
        records.push(StageRecord::new(format!("stage{i}"), i));
    }
    records.push(rec);
    StageLedger(records)
}

#[test]
fn fresh_spawn_emits_run_started_and_stage_entered_once() {
    let (mut world, sink) = world_with_sink();
    let e = world
        .spawn((meta("r1"), agent("plan", 0, AgentStatus::Active)))
        .id();
    observe(&mut world);
    let events = sink.events();
    assert_eq!(events.len(), 2, "{events:?}");
    assert!(matches!(
        &events[0],
        TelemetryEvent::RunStarted { run_id, recovered: false, .. } if run_id == "r1"
    ));
    assert!(matches!(
        &events[1],
        TelemetryEvent::StageEntered { stage_index: 0, stage_name, .. } if stage_name == "plan"
    ));
    // The observer's memory landed on the entity.
    assert!(world.get::<TelemetryState>(e).is_some());
    assert!(world.get::<StageActivity>(e).is_some());
    // A steady-state pass adds nothing.
    observe(&mut world);
    assert_eq!(sink.events().len(), 2);
}

#[test]
fn cursor_position_beyond_zero_marks_the_run_recovered() {
    let (mut world, sink) = world_with_sink();
    world.spawn((
        meta("r1"),
        agent("build", 0, AgentStatus::Active),
        StageCursor { index: 1 },
    ));
    observe(&mut world);
    let events = sink.events();
    assert!(matches!(
        &events[0],
        TelemetryEvent::RunStarted {
            recovered: true,
            ..
        }
    ));
    assert!(matches!(
        &events[1],
        TelemetryEvent::StageEntered { stage_index: 1, .. }
    ));
}

#[test]
fn prior_iterations_mark_the_run_recovered() {
    let (mut world, sink) = world_with_sink();
    world.spawn((
        meta("r1"),
        agent("plan", 3, AgentStatus::Active),
        StageCursor { index: 0 },
    ));
    observe(&mut world);
    assert!(matches!(
        &sink.events()[0],
        TelemetryEvent::RunStarted {
            recovered: true,
            ..
        }
    ));
}

#[test]
fn stage_transition_emits_exit_with_ledger_tokens_then_enter() {
    let (mut world, sink) = world_with_sink();
    let e = world
        .spawn((
            meta("r1"),
            agent("plan", 0, AgentStatus::Active),
            StageCursor { index: 0 },
            ledger_with(0, 11, 7),
        ))
        .id();
    observe(&mut world);
    world.entity_mut(e).insert(StageJustEntered {
        index: 1,
        name: "build".to_string(),
    });
    observe(&mut world);
    let events = sink.events();
    assert_eq!(events.len(), 4, "{events:?}");
    assert!(matches!(
        &events[2],
        TelemetryEvent::StageExited {
            stage_index: 0,
            prompt_tokens: 11,
            completion_tokens: 7,
            ..
        }
    ));
    assert!(matches!(
        &events[3],
        TelemetryEvent::StageEntered { stage_index: 1, stage_name, .. } if stage_name == "build"
    ));
}

#[test]
fn reentering_the_same_stage_keeps_it_open() {
    let (mut world, sink) = world_with_sink();
    let e = world
        .spawn((meta("r1"), agent("plan", 0, AgentStatus::Active)))
        .id();
    observe(&mut world);
    // A gate sent the stage back for another pass: same index re-entered.
    world.entity_mut(e).insert(StageJustEntered {
        index: 0,
        name: "plan".to_string(),
    });
    observe(&mut world);
    assert_eq!(sink.events().len(), 2, "no exit/enter for a re-entry");
}

#[test]
fn marker_present_at_first_sighting_enters_from_the_marker() {
    let (mut world, sink) = world_with_sink();
    world.spawn((
        meta("r1"),
        agent("build", 0, AgentStatus::Active),
        StageJustEntered {
            index: 1,
            name: "build".to_string(),
        },
    ));
    observe(&mut world);
    let events = sink.events();
    assert_eq!(events.len(), 2, "{events:?}");
    assert!(matches!(
        &events[1],
        TelemetryEvent::StageEntered { stage_index: 1, .. }
    ));
}

#[test]
fn terminal_status_emits_final_exit_and_completed_exactly_once() {
    let (mut world, sink) = world_with_sink();
    let e = world
        .spawn((
            meta("r1"),
            agent("plan", 2, AgentStatus::Active),
            StageCursor { index: 0 },
            TokenTotals {
                prompt_tokens: 100,
                completion_tokens: 40,
                cached_tokens: 0,
                cache_write_tokens: 0,
                tool_calls: 5,
            },
        ))
        .id();
    observe(&mut world);
    world.get_mut::<AgentState>(e).expect("agent state").status = AgentStatus::Complete;
    observe(&mut world);
    let events = sink.events();
    assert!(matches!(
        &events[events.len() - 2],
        TelemetryEvent::StageExited { stage_index: 0, .. }
    ));
    assert!(matches!(
        &events[events.len() - 1],
        TelemetryEvent::RunCompleted {
            status,
            prompt_tokens: 100,
            completion_tokens: 40,
            tool_calls: 5,
            ..
        } if status == "complete"
    ));
    // Later passes stay quiet: the run is closed.
    let count = events.len();
    observe(&mut world);
    assert_eq!(sink.events().len(), count);
}

#[test]
fn a_run_first_seen_terminal_gets_a_complete_story_in_one_pass() {
    let (mut world, sink) = world_with_sink();
    world.spawn((
        meta("r1"),
        agent(
            "plan",
            0,
            AgentStatus::Error {
                message: "boom".to_string(),
            },
        ),
    ));
    observe(&mut world);
    let kinds: Vec<_> = sink.events();
    assert_eq!(kinds.len(), 4, "{kinds:?}");
    assert!(matches!(kinds[0], TelemetryEvent::RunStarted { .. }));
    assert!(matches!(kinds[1], TelemetryEvent::StageEntered { .. }));
    assert!(matches!(kinds[2], TelemetryEvent::StageExited { .. }));
    assert!(matches!(
        &kinds[3],
        TelemetryEvent::RunCompleted { status, prompt_tokens: 0, .. } if status == "error"
    ));
}

#[test]
fn activity_records_drain_into_enriched_events() {
    let (mut world, sink) = world_with_sink();
    let e = world
        .spawn((meta("r1"), agent("plan", 0, AgentStatus::Active)))
        .id();
    observe(&mut world);
    world
        .get_mut::<StageActivity>(e)
        .expect("inserted at first sighting")
        .0
        .extend([
            ActivityRecord::Inference {
                provider: "anthropic".to_string(),
                model: "m1".to_string(),
                latency_ms: 250,
                prompt_tokens: 10,
                completion_tokens: 4,
                cached_tokens: 2,
                success: true,
            },
            ActivityRecord::ToolCall {
                tool_name: "read_file".to_string(),
                batch_latency_ms: 12,
                success: false,
            },
            ActivityRecord::Compaction { success: true },
        ]);
    observe(&mut world);
    let events = sink.events();
    assert!(matches!(
        &events[2],
        TelemetryEvent::InferenceCompleted {
            provider,
            latency_ms: 250,
            stage_name,
            ..
        } if provider == "anthropic" && stage_name == "plan"
    ));
    assert!(matches!(
        &events[3],
        TelemetryEvent::ToolCallCompleted { tool_name, success: false, .. }
            if tool_name == "read_file"
    ));
    assert!(matches!(
        &events[4],
        TelemetryEvent::CompactionCompleted { success: true, .. }
    ));
    // Drained: a further pass does not repeat them.
    observe(&mut world);
    assert_eq!(sink.events().len(), 5);
    assert!(
        world
            .get::<StageActivity>(e)
            .expect("still there")
            .0
            .is_empty()
    );
}

#[test]
fn buffered_log_lines_are_narrated_with_their_kind() {
    let (mut world, sink) = world_with_sink();
    let mut buffer = StageIoBuffer::default();
    buffer.output.push((0, "the plan".to_string()));
    buffer.logs.push((0, "[Tokens: 10 in, 4 out]".to_string()));
    world.spawn((meta("r1"), agent("plan", 0, AgentStatus::Active), buffer));
    observe(&mut world);
    let events = sink.events();
    assert!(matches!(
        &events[2],
        TelemetryEvent::Log { kind: LogKind::Output, line, .. } if line == "the plan"
    ));
    assert!(matches!(
        &events[3],
        TelemetryEvent::Log {
            kind: LogKind::Runtime,
            line,
            ..
        } if line.starts_with("[Tokens")
    ));
}

#[test]
fn terminal_label_maps_every_status() {
    assert_eq!(terminal_label(&AgentStatus::Complete), Some("complete"));
    assert_eq!(
        terminal_label(&AgentStatus::Error {
            message: "x".to_string()
        }),
        Some("error")
    );
    assert_eq!(terminal_label(&AgentStatus::Cancelled), Some("cancelled"));
    assert_eq!(terminal_label(&AgentStatus::Idle), None);
    assert_eq!(terminal_label(&AgentStatus::Active), None);
    assert_eq!(terminal_label(&AgentStatus::Waiting), None);
}

#[test]
fn stage_tokens_reads_the_record_or_zeros() {
    let ledger = ledger_with(1, 9, 3);
    assert_eq!(stage_tokens(Some(&ledger), 1), (9, 3));
    assert_eq!(stage_tokens(Some(&ledger), 7), (0, 0));
    assert_eq!(stage_tokens(None, 0), (0, 0));
}
