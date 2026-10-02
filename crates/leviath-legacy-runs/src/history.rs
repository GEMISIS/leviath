//! The run's history: the old journal's steps, replayed as deltas.
//!
//! The history starts at the state the run was spawned in and applies one
//! delta per journal step that changed something the run file keeps. The
//! context, status, stage and totals come from the journal's snapshots and
//! metadata; model calls, tool calls, answers and messages become events.
//! A last delta then carries the run to the state rebuilt from every file,
//! with the conversion's report in its log, so folding the deltas always ends
//! exactly at the last state.

use leviath_core::run_meta::{ContextSnapshot, RunMeta};
use leviath_runtime::runfile::record::{AttemptOutcome, AttemptRecord};
use leviath_runtime::spec::names::ModelRef;
use leviath_runtime::spec::run_spec::RunSpec;
use leviath_runtime::state::context::ToolCallState;
use leviath_runtime::state::{
    MessageState, RunEvent, RunState, Spend, StateDelta, ToolResultState,
};

use crate::context::Losses;
use crate::journal::{self, JournalRecord};
use crate::legacy::LegacyRun;
use crate::report::Report;
use crate::state::{context_in, entry, from_meta, last, stage_of};

/// A model as the journal names one.
fn model(provider: &str, model: &str) -> Option<ModelRef> {
    ModelRef::parse(&format!("{provider}/{model}")).ok()
}

/// The walk through the journal.
struct Replay<'a> {
    spec: &'a RunSpec,
    meta: RunMeta,
    context: ContextSnapshot,
    state: RunState,
    deltas: Vec<StateDelta>,
    /// The latest provider attempt not yet matched to its usage.
    attempt: Option<AttemptRecord>,
    at: i64,
}

impl Replay<'_> {
    /// When a step taken at `at` is stamped: never before the step before
    /// it, since a journal's clocks and the metadata's need not agree.
    fn stamp(&self, at: i64) -> i64 {
        self.deltas.last().map_or(at, |d| d.at.max(at))
    }

    /// Apply one journal step's changes and events as a delta.
    fn step(&mut self, context_moved: bool, events: Vec<RunEvent>) {
        let mut next = self.state.clone();
        next.pending = None;
        from_meta(&mut next, &self.meta, &self.spec.graph);
        if context_moved {
            let stage = stage_of(&self.spec.graph, &self.meta).unwrap_or(entry(&self.spec.graph));
            next.context = context_in(
                &self.spec.graph,
                stage,
                &self.context,
                &mut Losses::default(),
            );
        }
        let delta = StateDelta::between(&self.state, &next, self.stamp(self.at), events);
        if !delta.is_empty() {
            delta.apply(&mut self.state);
            self.deltas.push(delta);
        }
    }

    /// One record: the events it carries, and what it changes of the state.
    /// A record is one or the other, so each half passes over the kinds that
    /// belong to the other.
    fn record(&mut self, r: &JournalRecord) {
        let events = self.events(r);
        let moved = match r {
            JournalRecord::Header { meta, .. } => {
                self.meta = (**meta).clone();
                false
            }
            JournalRecord::ContextCheckpoint { snapshot, at } => {
                self.at = *at;
                self.context = snapshot.clone();
                true
            }
            JournalRecord::ContextDiff { delta, at } => {
                self.at = *at;
                journal::apply_delta(&mut self.context, delta);
                true
            }
            JournalRecord::Progress { meta, delta, at } => {
                self.at = *at;
                self.meta = (**meta).clone();
                journal::apply_delta(&mut self.context, delta);
                true
            }
            JournalRecord::Checkpoint { meta, context, at } => {
                self.at = *at;
                self.meta = (**meta).clone();
                self.context = context.clone();
                true
            }
            JournalRecord::StatusChanged { status, at } => {
                self.at = *at;
                self.meta.status = status.clone();
                false
            }
            _ => false,
        };
        self.step(moved, events);
    }

    /// The events a record that is not a state change carries.
    fn events(&mut self, r: &JournalRecord) -> Vec<RunEvent> {
        match r {
            JournalRecord::InferenceAttempt(a) => {
                self.attempt = Some(a.clone());
                match &a.outcome {
                    AttemptOutcome::Succeeded => Vec::new(),
                    other => vec![RunEvent::Log(format!(
                        "attempt {} on {}/{} did not answer: {}",
                        a.id,
                        a.provider,
                        a.model,
                        serde_json::to_string(other).unwrap_or_default()
                    ))],
                }
            }
            JournalRecord::InferenceUsage {
                kind,
                stage,
                iteration,
                provider,
                model: name,
                prompt_tokens,
                completion_tokens,
                cached_tokens,
                cache_write_tokens,
                cost_usd,
                cost_reported_by_provider,
                at,
                ..
            } => {
                self.at = *at;
                let attempt = self
                    .attempt
                    .take_if(|a| a.provider == *provider && a.model == *name);
                let Some(model) = model(provider, name) else {
                    return vec![RunEvent::Log(format!(
                        "a {} call to {provider:?}/{name:?}, which are not valid names",
                        kind.label()
                    ))];
                };
                let spend = Spend {
                    prompt_tokens: *prompt_tokens as u64,
                    completion_tokens: *completion_tokens as u64,
                    cached_tokens: *cached_tokens as u64,
                    cache_write_tokens: *cache_write_tokens as u64,
                    priced_usd: cost_usd.unwrap_or_default(),
                    reported_calls: u32::from(*cost_reported_by_provider == Some(true)),
                    computed_calls: u32::from(*cost_reported_by_provider == Some(false)),
                    unpriced_calls: u32::from(cost_usd.is_none()),
                };
                vec![RunEvent::Inference {
                    attempt: attempt.as_ref().map(|a| a.id.clone()).unwrap_or_default(),
                    model,
                    spend,
                    finish_reason: attempt.map(|a| a.finish_reason),
                    kind: (*kind).into(),
                    stage: leviath_runtime::spec::names::StageName::new(stage).ok(),
                    iteration: u32::try_from(*iteration).unwrap_or(u32::MAX),
                }]
            }
            JournalRecord::InferenceFailover(f) => {
                self.at = f.at;
                match (
                    model(&f.from_provider, &f.from_model),
                    model(&f.to_provider, &f.to_model),
                ) {
                    (Some(from), Some(to)) => vec![RunEvent::Failover {
                        from,
                        to,
                        reason: f.reason.clone(),
                    }],
                    _ => vec![RunEvent::Log(format!(
                        "failed over from {}/{} to {}/{}: {}",
                        f.from_provider, f.from_model, f.to_provider, f.to_model, f.reason
                    ))],
                }
            }
            JournalRecord::ToolBatch { calls, at, .. } => {
                self.at = *at;
                let mut events = Vec::new();
                for c in calls {
                    events.push(RunEvent::ToolStarted(ToolCallState {
                        id: c.id.clone(),
                        name: c.name.clone(),
                        args: leviath_core::JsonDoc::parse(&c.arguments).unwrap_or_else(|_| {
                            leviath_core::JsonDoc::new(serde_json::Value::String(
                                c.arguments.clone(),
                            ))
                        }),
                        thought_signature: c.thought_signature.clone(),
                    }));
                    events.extend(c.result.as_ref().map(|r| RunEvent::ToolFinished {
                        call_id: c.id.clone(),
                        result: ToolResultState {
                            text: r.as_str().to_string(),
                            is_error: false,
                        },
                        millis: 0,
                    }));
                }
                events
            }
            JournalRecord::ToolCallDone {
                call_id,
                result,
                outcome,
                at,
                ..
            } => {
                self.at = *at;
                vec![RunEvent::ToolFinished {
                    call_id: call_id.clone(),
                    result: ToolResultState {
                        text: result.as_str().to_string(),
                        is_error: outcome
                            .is_some_and(|o| o != leviath_core::execution::ToolOutcome::Succeeded),
                    },
                    millis: 0,
                }]
            }
            JournalRecord::Interaction {
                request_id,
                settlement,
                at,
                ..
            } => {
                self.at = *at;
                vec![RunEvent::Answered {
                    id: request_id.clone(),
                    answer: serde_json::to_string(settlement).unwrap_or_default(),
                }]
            }
            JournalRecord::Message { message, at } => {
                self.at = *at;
                vec![RunEvent::Message(MessageState {
                    from: message.role.clone(),
                    text: message.content.clone(),
                    region: None,
                })]
            }
            JournalRecord::OwnershipChanged {
                machine_id,
                world_id,
                at,
            } => {
                self.at = *at;
                vec![RunEvent::Log(format!(
                    "the run moved to machine {machine_id}, world {world_id}"
                ))]
            }
            JournalRecord::ArtifactsProduced {
                execution_id,
                artifacts,
                at,
            } => {
                self.at = *at;
                vec![RunEvent::Log(format!(
                    "execution {execution_id} produced {} files",
                    artifacts.len()
                ))]
            }
            JournalRecord::Inference { stage, at, .. } => {
                self.at = *at;
                vec![RunEvent::Log(format!("a model call in stage {stage}"))]
            }
            // What changed the window, kept as the live lane keeps it, so a
            // converted run reads its context changes the way it always did.
            JournalRecord::ContextTransaction {
                revision_before,
                revision_after,
                cause,
                regions,
                execution_id,
                at,
            } => {
                self.at = *at;
                leviath_runtime::runfile::journal_events(
                    &leviath_runtime::runfile::record::RunRecord::ContextTransaction {
                        revision_before: revision_before.clone(),
                        revision_after: revision_after.clone(),
                        cause: *cause,
                        regions: regions.clone(),
                        execution_id: execution_id.clone(),
                        at: *at,
                    },
                )
            }
            JournalRecord::ContextChange {
                region,
                cause,
                entries_added,
                entries_removed,
                token_delta,
                at,
            } => {
                self.at = *at;
                vec![RunEvent::ContextNoted(
                    leviath_runtime::state::journal::ContextNoteState {
                        region: region.clone(),
                        cause: (*cause).into(),
                        entries_added: crate::context::n32(*entries_added),
                        entries_removed: crate::context::n32(*entries_removed),
                        token_delta: *token_delta,
                    },
                )]
            }
            // A change of state carries no event; `record` reads it.
            _ => Vec::new(),
        }
    }
}

/// The state the run started in, its deltas, and the state it was last in.
pub(crate) fn build(
    old: &LegacyRun,
    spec: &RunSpec,
    report: &mut Report,
) -> (RunState, Vec<StateDelta>, RunState) {
    let graph = &spec.graph;
    let first = entry(graph);
    let empty = ContextSnapshot {
        stage_name: String::new(),
        total_tokens: 0,
        max_tokens: 0,
        regions: Vec::new(),
    };
    let context = old.first_context().cloned().unwrap_or(empty);
    let start_ctx = context_in(graph, first, &context, &mut Losses::default());
    let start = RunState::initial(first.name.clone(), start_ctx, first.accepts_messages);
    let mut replay = Replay {
        spec,
        meta: old.header.clone(),
        context,
        state: start.clone(),
        deltas: Vec::new(),
        attempt: None,
        at: old.header.started_at,
    };
    for r in old.records.iter().skip(1) {
        replay.record(r);
    }
    let mut last = last(old, spec, report);
    let events = report.log_lines().into_iter().map(RunEvent::Log).collect();
    let at = replay.stamp(old.meta().updated_at);
    let end = StateDelta::between(&replay.state, &last, at, events);
    end.apply(&mut replay.state);
    last.seq = end.seq;
    replay.deltas.push(end);
    report.note(format!(
        "{} journal records became {} deltas",
        old.records.len(),
        replay.deltas.len()
    ));
    (start, replay.deltas, last)
}
