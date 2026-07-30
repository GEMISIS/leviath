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
                // Same invariant as the drain above: an open run always has an
                // entered stage to close.
                let (prev_index, prev_name) = st.last_stage.take().expect("stage set at sighting");
                let (prompt, completion) = stage_tokens(ledger, prev_index);
                sink.emit(TelemetryEvent::StageExited {
                    run_id: md.run_id.clone(),
                    stage_index: prev_index,
                    stage_name: prev_name,
                    prompt_tokens: prompt,
                    completion_tokens: completion,
                    at_ms: now_ms,
                });
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
mod tests;
