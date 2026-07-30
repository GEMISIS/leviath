//! Observability as an ECS system: watch the components every other system
//! already writes and narrate them into the installed [`TelemetrySink`].
//!
//! The pipeline's collect systems leave pure-data [`ActivityRecord`]s on the
//! agent (an inference landed, a tool batch ran, a compaction finished);
//! [`observe_lifecycle`] runs once per schedule pass near the end of the tick,
//! turns those plus the agent's own state into [`TelemetryEvent`]s, and emits
//! them. Ordering in the tick chain is load-bearing twice over: the system
//! must run *before* `sync_tool_stages` (which consumes the transient
//! `StageJustEntered` marker) and *before* `dispatch_persistence` (which
//! drains `StageIoBuffer` - this system only reads the buffer, so running
//! first is what makes each log line observed exactly once).

use std::sync::Arc;

use bevy_ecs::prelude::*;
use leviath_core::telemetry::{LogKind, TelemetryEvent, TelemetrySink};

use crate::components::{AgentState, AgentStatus};
use crate::persistence::{RunMetadata, TokenTotals};
use crate::pipeline::{StageCursor, StageIoBuffer, StageJustEntered, StageLedger};

/// The installed telemetry sink. [`crate::world::PipelineWorld::new`] installs
/// [`leviath_core::telemetry::NoopSink`]; a host that wants export replaces
/// the resource (the same way it installs `PolicyGate` or `TitleSettings`).
#[derive(Resource, Clone)]
pub struct Telemetry(pub Arc<dyn TelemetrySink>);

/// One completed piece of stage work, recorded by the collect system that
/// applied it and drained into events by [`observe_lifecycle`]. Carries only
/// what the collect site knows; run/stage identity is added at drain time.
#[derive(Debug, Clone, PartialEq)]
pub enum ActivityRecord {
    /// An inference call finished (either way).
    Inference {
        provider: String,
        model: String,
        latency_ms: u64,
        prompt_tokens: usize,
        completion_tokens: usize,
        cached_tokens: usize,
        success: bool,
    },
    /// One tool call out of a finished batch.
    ToolCall {
        tool_name: String,
        batch_latency_ms: u64,
        success: bool,
    },
    /// A compaction pass finished.
    Compaction { success: bool },
}

/// Buffered [`ActivityRecord`]s awaiting the observer. Inserted alongside
/// [`TelemetryState`] the first time the observer sees an agent, so the
/// collect systems treat it as optional and skip recording until then (an
/// agent's first inference cannot land before the observer has run once).
#[derive(Component, Debug, Default)]
pub struct StageActivity(pub Vec<ActivityRecord>);

/// The observer's per-agent memory: what it has already narrated.
#[derive(Component, Debug, Clone, Default)]
pub struct TelemetryState {
    /// A `RunStarted` was emitted and no `RunCompleted` yet.
    run_open: bool,
    /// The stage the observer last reported as entered.
    last_stage: Option<(usize, String)>,
}

/// The terminal status label for [`TelemetryEvent::RunCompleted`], or `None`
/// while the run is still going.
fn terminal_label(status: &AgentStatus) -> Option<&'static str> {
    match status {
        AgentStatus::Complete => Some("complete"),
        AgentStatus::Error { .. } => Some("error"),
        AgentStatus::Cancelled => Some("cancelled"),
        AgentStatus::Idle | AgentStatus::Active | AgentStatus::Waiting => None,
    }
}

/// The (prompt, completion) token totals a stage accrued, from its ledger
/// record; zeros when the ledger has no record for it.
fn stage_tokens(ledger: Option<&StageLedger>, index: usize) -> (usize, usize) {
    ledger
        .and_then(|l| l.0.get(index))
        .map_or((0, 0), |rec| (rec.prompt_tokens, rec.completion_tokens))
}

/// Emit lifecycle, activity, and log events for every agent run.
///
/// Stage boundaries come from the `StageJustEntered` marker (with the
/// agent's first sighting standing in for the marker-less initial stage);
/// a re-entry into the same stage index keeps the stage open rather than
/// closing and reopening it, matching how the stage ledger accrues.
#[allow(clippy::type_complexity)]
pub fn observe_lifecycle(
    telemetry: Res<Telemetry>,
    mut agents: Query<(
        Entity,
        &RunMetadata,
        &AgentState,
        Option<&StageCursor>,
        Option<&TokenTotals>,
        Option<&StageLedger>,
        Option<&StageJustEntered>,
        Option<&mut TelemetryState>,
        Option<&mut StageActivity>,
        Option<&StageIoBuffer>,
    )>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    for (entity, md, state, cursor, totals, ledger, entered, ts, activity, buffer) in
        agents.iter_mut()
    {
        crate::tick_scope::enter(entity);
        let now_ms = chrono::Utc::now().timestamp_millis();
        let sink = telemetry.0.as_ref();
        let mut ts = ts;
        let (mut st, is_new) = match ts.as_deref() {
            Some(existing) => (existing.clone(), false),
            None => (TelemetryState::default(), true),
        };

        if is_new {
            // First sighting. A run restored from disk is already mid-flight:
            // its earlier spans (if any) belong to a previous daemon process,
            // so the trace it gets here starts now and says so.
            let recovered = state.iteration > 0 || cursor.is_some_and(|c| c.index > 0);
            sink.emit(TelemetryEvent::RunStarted {
                run_id: md.run_id.clone(),
                agent_name: md.agent_name.clone(),
                model: md.model.clone(),
                parent_run_id: md.parent_run_id.clone(),
                recovered,
                at_ms: now_ms,
            });
            st.run_open = true;
        }

        if st.run_open {
            // Stage boundary: the transition marker, or - for the marker-less
            // first sighting - the agent's current stage.
            let boundary = match entered {
                Some(marker) => Some((marker.index, marker.name.clone())),
                None if st.last_stage.is_none() => {
                    Some((cursor.map_or(0, |c| c.index), state.current_stage.clone()))
                }
                None => None,
            };
            if let Some((index, name)) = boundary {
                let same_stage = st.last_stage.as_ref().is_some_and(|(i, _)| *i == index);
                if !same_stage {
                    if let Some((prev_index, prev_name)) = st.last_stage.take() {
                        let (prompt, completion) = stage_tokens(ledger, prev_index);
                        sink.emit(TelemetryEvent::StageExited {
                            run_id: md.run_id.clone(),
                            stage_index: prev_index,
                            stage_name: prev_name,
                            prompt_tokens: prompt,
                            completion_tokens: completion,
                            at_ms: now_ms,
                        });
                    }
                    sink.emit(TelemetryEvent::StageEntered {
                        run_id: md.run_id.clone(),
                        stage_index: index,
                        stage_name: name.clone(),
                        at_ms: now_ms,
                    });
                    st.last_stage = Some((index, name));
                }
            }

            // Completed work the collect systems recorded since the last pass.
            if let Some(mut activity) = activity {
                // An open run always has an entered stage: the first sighting
                // above set one before this point.
                let (_, ref stage_name) = *st.last_stage.as_ref().expect("stage set at sighting");
                let stage_name = stage_name.clone();
                for record in activity.0.drain(..) {
                    sink.emit(match record {
                        ActivityRecord::Inference {
                            provider,
                            model,
                            latency_ms,
                            prompt_tokens,
                            completion_tokens,
                            cached_tokens,
                            success,
                        } => TelemetryEvent::InferenceCompleted {
                            run_id: md.run_id.clone(),
                            stage_name: stage_name.clone(),
                            provider,
                            model,
                            latency_ms,
                            prompt_tokens,
                            completion_tokens,
                            cached_tokens,
                            success,
                        },
                        ActivityRecord::ToolCall {
                            tool_name,
                            batch_latency_ms,
                            success,
                        } => TelemetryEvent::ToolCallCompleted {
                            run_id: md.run_id.clone(),
                            stage_name: stage_name.clone(),
                            tool_name,
                            batch_latency_ms,
                            success,
                        },
                        ActivityRecord::Compaction { success } => {
                            TelemetryEvent::CompactionCompleted {
                                run_id: md.run_id.clone(),
                                stage_name: stage_name.clone(),
                                success,
                            }
                        }
                    });
                }
            }

            // Log lines: read, never drain - `dispatch_persistence` (which
            // runs after this system in the same pass) owns the drain, so
            // each line passes through here exactly once.
            if let Some(buffer) = buffer {
                for ((idx, line), kind) in buffer
                    .output
                    .iter()
                    .map(|l| (l, LogKind::Output))
                    .chain(buffer.logs.iter().map(|l| (l, LogKind::Runtime)))
                {
                    sink.emit(TelemetryEvent::Log {
                        run_id: md.run_id.clone(),
                        stage_index: *idx,
                        kind,
                        line: line.clone(),
                    });
                }
            }

            if let Some(status) = terminal_label(&state.status) {
                if let Some((prev_index, prev_name)) = st.last_stage.take() {
                    let (prompt, completion) = stage_tokens(ledger, prev_index);
                    sink.emit(TelemetryEvent::StageExited {
                        run_id: md.run_id.clone(),
                        stage_index: prev_index,
                        stage_name: prev_name,
                        prompt_tokens: prompt,
                        completion_tokens: completion,
                        at_ms: now_ms,
                    });
                }
                let totals = totals.copied().unwrap_or_default();
                sink.emit(TelemetryEvent::RunCompleted {
                    run_id: md.run_id.clone(),
                    status: status.to_string(),
                    prompt_tokens: totals.prompt_tokens,
                    completion_tokens: totals.completion_tokens,
                    tool_calls: totals.tool_calls,
                    at_ms: now_ms,
                });
                st.run_open = false;
            }
        }

        if is_new {
            commands
                .entity(entity)
                .insert((st, StageActivity::default()));
        } else {
            *ts.as_deref_mut().expect("state exists when not new") = st;
        }
    }
}

#[cfg(test)]
mod tests {
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
}
