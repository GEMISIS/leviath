//! The run's history: the old journal's steps, replayed as deltas.
//!
//! The history starts at the state the run was spawned in and applies one
//! delta per journal step that changed something the run file keeps. The
//! context, status, stage and totals come from the journal's snapshots and
//! metadata; model calls, tool calls, answers and messages become events.
//! A record of a kind the running world still sends (a model call's attempt
//! and its bill, a move to another model, a tool batch and each call's end,
//! a settled question, files an execution produced, a committed change to the
//! window) becomes the events the world makes of it in a new run's file, so
//! a converted run's calls and executions read as the release that wrote
//! them showed them. A last delta then carries the run to the state rebuilt
//! from every file, with the conversion's report in its log, so folding the
//! deltas always ends exactly at the last state.
//!
//! Every state names the stored parts its window has held so far, as a
//! running world names them at each step: a part stays named once its window
//! lets it go, with the region it first appeared in.

use leviath_core::run_meta::{ContextSnapshot, RunMeta};
use leviath_runtime::runfile::record::{InferenceKind, RunRecord};
use leviath_runtime::runfile::{Answered, journal_events_with};
use leviath_runtime::spec::run_spec::RunSpec;
use leviath_runtime::state::files::note_blobs;
use leviath_runtime::state::{Change, ContextDiff, MessageState, RunEvent, RunState, StateDelta};

use crate::context::Losses;
use crate::journal::{self, JournalRecord};
use crate::legacy::LegacyRun;
use crate::report::Report;
use crate::state::{context_in, entry, from_meta, last, stage_of};

/// The walk through the journal.
struct Replay<'a> {
    spec: &'a RunSpec,
    meta: RunMeta,
    context: ContextSnapshot,
    state: RunState,
    deltas: Vec<StateDelta>,
    /// The attempts that answered, waiting for the usage record that bills
    /// them.
    answered: Answered,
    at: i64,
    /// Whether the checkpoint the start state was seeded from has been read.
    seeded: bool,
    /// What reading each step's window left out.
    losses: Losses,
}

impl Replay<'_> {
    /// When a step taken at `at` is stamped: never before the step before
    /// it, since a journal's clocks and the metadata's need not agree.
    fn stamp(&self, at: i64) -> i64 {
        self.deltas.last().map_or(at, |d| d.at.max(at))
    }

    /// Apply one journal step's changes and events as a delta. A step that
    /// held the window is a point in the run's history, as every release
    /// that wrote the journal showed it, so its delta always carries the
    /// window, even when the window is as it was.
    fn step(&mut self, context_moved: bool, events: Vec<RunEvent>) {
        let mut next = self.state.clone();
        next.pending = None;
        // Each move into a stage is a visit to it, as the world counts one, so
        // every point of the history names the stage it was in.
        if let Some(stage) = stage_of(&self.spec.graph, &self.meta)
            && (stage.name != next.cursor.stage || !next.visits.contains_key(&stage.name))
        {
            *next.visits.entry(stage.name.clone()).or_default() += 1;
        }
        from_meta(&mut next, &self.meta, &self.spec.graph);
        if context_moved {
            let stage = stage_of(&self.spec.graph, &self.meta).unwrap_or(entry(&self.spec.graph));
            next.context = context_in(&self.spec.graph, stage, &self.context, &mut self.losses);
            note_blobs(&mut next.blobs, &next.context);
        }
        let mut delta = StateDelta::between(&self.state, &next, self.stamp(self.at), events);
        if context_moved
            && !delta
                .changes
                .iter()
                .any(|c| matches!(c, Change::Context(_)))
        {
            delta.changes.push(Change::Context(ContextDiff {
                regions: Vec::new(),
                removed: Vec::new(),
                order: None,
                hidden: None,
                max_tokens: Some(next.context.max_tokens),
            }));
        }
        if !delta.is_empty() {
            delta.apply(&mut self.state);
            self.deltas.push(delta);
        }
    }

    /// One record: the events it carries, and what it changes of the state.
    /// A record is one or the other, so each half passes over the kinds that
    /// belong to the other.
    fn record(&mut self, r: &JournalRecord) {
        self.enter_named_stage(r);
        let events = self.events(r);
        let moved = match r {
            JournalRecord::Header { meta, .. } => {
                self.meta = (**meta).clone();
                false
            }
            JournalRecord::ContextCheckpoint { snapshot, at } => {
                self.at = *at;
                self.context = snapshot.clone();
                self.held_after_seed()
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
                self.held_after_seed()
            }
            JournalRecord::StatusChanged { status, at } => {
                self.at = *at;
                self.meta.status = status.clone();
                false
            }
            // A stage's model call is made in the iteration the record
            // names, which the metadata written after it only catches up
            // with by a later step.
            JournalRecord::InferenceUsage {
                kind: InferenceKind::Stage,
                iteration,
                ..
            } if *iteration > 0 => {
                self.meta.iteration = *iteration;
                false
            }
            // So is the batch its answer asked for. A journal that billed no
            // call first moves to that iteration in a step of its own, so
            // the batch is read in the iteration it was dispatched in, as a
            // step is read against the cursor before it.
            JournalRecord::ToolBatch { iteration, .. }
                if *iteration > 0 && self.meta.iteration != *iteration =>
            {
                self.meta.iteration = *iteration;
                self.step(false, Vec::new());
                false
            }
            _ => false,
        };
        self.step(moved, events);
    }

    /// Move into the stage a model call's record names, in a step of its
    /// own, when the run is not in it yet. A journal wrote a call's records
    /// as the call was made, and the step that moved the run into the stage
    /// only at its next save, after them; read against the cursor before it,
    /// as a step is, the call then reads as made in the stage it was made
    /// in. A record that names no stage of the graph (the title call names
    /// none) moves nothing.
    fn enter_named_stage(&mut self, r: &JournalRecord) {
        let named = match r {
            JournalRecord::InferenceAttempt(a) => &a.stage,
            JournalRecord::InferenceFailover(f) => &f.stage,
            JournalRecord::InferenceUsage { stage, .. } => stage,
            _ => return,
        };
        if let Some(stage) = self.spec.graph.stage(named)
            && stage.name.as_str() != self.meta.current_stage
        {
            self.meta.current_stage = stage.name.to_string();
            self.step(false, Vec::new());
        }
    }

    /// Whether a checkpoint is a point of its own: every one but the first,
    /// which is the state the run started in.
    fn held_after_seed(&mut self) -> bool {
        std::mem::replace(&mut self.seeded, true)
    }

    /// The events a record that is not a state change carries.
    fn events(&mut self, r: &JournalRecord) -> Vec<RunEvent> {
        if let Some((record, at)) = shared(r) {
            self.at = at;
            let events = journal_events_with(&record, &mut self.answered);
            return match (events.is_empty(), dropped(r)) {
                (true, Some(line)) => vec![RunEvent::Log(line)],
                _ => events,
            };
        }
        match r {
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
            JournalRecord::Inference { stage, at, .. } => {
                self.at = *at;
                vec![RunEvent::Log(format!("a model call in stage {stage}"))]
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

/// A record of a kind the running world still sends, as the world's own
/// record of it, and when it was written.
fn shared(r: &JournalRecord) -> Option<(RunRecord, i64)> {
    Some(match r.clone() {
        JournalRecord::InferenceAttempt(a) => {
            let at = a.at;
            (RunRecord::InferenceAttempt(Box::new(a)), at)
        }
        JournalRecord::InferenceFailover(f) => {
            let at = f.at;
            (RunRecord::InferenceFailover(f), at)
        }
        JournalRecord::InferenceUsage {
            kind,
            stage,
            iteration,
            provider,
            model,
            prompt_tokens,
            completion_tokens,
            cached_tokens,
            cache_write_tokens,
            cost_usd,
            cost_reported_by_provider,
            at,
        } => (
            RunRecord::InferenceUsage {
                kind,
                stage,
                iteration,
                provider,
                model,
                prompt_tokens,
                completion_tokens,
                cached_tokens,
                cache_write_tokens,
                cost_usd,
                cost_reported_by_provider,
                at,
            },
            at,
        ),
        JournalRecord::ToolBatch {
            calls,
            at,
            stage_index,
            iteration,
            visit_id,
            requested_by,
            response,
        } => (
            RunRecord::ToolBatch {
                calls,
                at,
                stage_index,
                iteration,
                visit_id,
                requested_by,
                response,
            },
            at,
        ),
        JournalRecord::ToolCallDone {
            iteration,
            call_id,
            execution_id,
            result,
            outcome,
            at,
        } => (
            RunRecord::ToolCallDone {
                iteration,
                call_id,
                execution_id,
                result,
                outcome,
                at,
            },
            at,
        ),
        JournalRecord::ArtifactsProduced {
            execution_id,
            artifacts,
            at,
        } => (
            RunRecord::ArtifactsProduced {
                execution_id,
                artifacts,
                at,
            },
            at,
        ),
        JournalRecord::Interaction {
            request_id,
            kind,
            tool,
            prompt,
            stage,
            settlement,
            asked_at,
            at,
        } => (
            RunRecord::Interaction {
                request_id,
                kind,
                tool,
                prompt,
                stage,
                settlement,
                asked_at,
                at,
            },
            at,
        ),
        JournalRecord::ContextTransaction {
            revision_before,
            revision_after,
            cause,
            regions,
            execution_id,
            at,
        } => (
            RunRecord::ContextTransaction {
                revision_before,
                revision_after,
                cause,
                regions,
                execution_id,
                at,
            },
            at,
        ),
        _ => return None,
    })
}

/// What the log says of a model call's bill, or a move between models, that
/// names a model no run file can: the world keeps no event for either.
fn dropped(r: &JournalRecord) -> Option<String> {
    match r {
        JournalRecord::InferenceUsage {
            kind,
            provider,
            model,
            ..
        } => Some(format!(
            "a {} call to {provider:?}/{model:?}, which are not valid names",
            kind.label()
        )),
        JournalRecord::InferenceFailover(f) => Some(format!(
            "failed over from {}/{} to {}/{}: {}",
            f.from_provider, f.from_model, f.to_provider, f.to_model, f.reason
        )),
        _ => None,
    }
}

/// Name in the report what reading the old journal left out: records this
/// build does not read, and a record a crash cut short at its end.
fn left_out(old: &LegacyRun, report: &mut Report) {
    if old.skipped_records > 0 {
        report.note(format!(
            "{} records of the old journal were left out: they are not records this build reads",
            old.skipped_records
        ));
    }
    if old.torn_bytes > 0 {
        report.note(format!(
            "the old journal ends in {} bytes of a record cut short, which were left out",
            old.torn_bytes
        ));
    }
}

/// The state the run started in, its deltas, and the state it was last in.
pub(crate) fn build(
    old: &LegacyRun,
    spec: &RunSpec,
    report: &mut Report,
) -> (RunState, Vec<StateDelta>, RunState) {
    let graph = &spec.graph;
    // The run starts in the stage its first record names, visited once, as a
    // run the world spawns does; one whose record names none entered none.
    let named = stage_of(graph, &old.header);
    let first = named.unwrap_or_else(|| entry(graph));
    let empty = ContextSnapshot {
        stage_name: String::new(),
        total_tokens: 0,
        max_tokens: 0,
        regions: Vec::new(),
    };
    let context = old.first_context().cloned().unwrap_or(empty);
    let mut losses = Losses::default();
    let start_ctx = context_in(graph, first, &context, &mut losses);
    let mut start = RunState::initial(first.name.clone(), start_ctx, first.accepts_messages);
    start.visits.extend(named.map(|s| (s.name.clone(), 1)));
    note_blobs(&mut start.blobs, &start.context);
    let mut replay = Replay {
        spec,
        meta: old.header.clone(),
        context,
        state: start.clone(),
        deltas: Vec::new(),
        answered: Answered::default(),
        at: old.header.started_at,
        seeded: false,
        losses,
    };
    for r in old.records.iter().skip(1) {
        replay.record(r);
    }
    let named = replay.state.blobs.clone();
    let mut last = last(old, spec, &mut replay.losses, named, report);
    replay.losses.report(report);
    left_out(old, report);
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
