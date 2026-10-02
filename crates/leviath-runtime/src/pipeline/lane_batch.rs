//! A tool batch on its way to the lane: journaled, each call decided in the
//! world, then sent.
//!
//! [`dispatch_tools`](super::dispatch_tools) resolves what it can inline and
//! hands the rest over as a [`PendingBatch`]. [`dispatch_lane_batches`] then:
//!
//! 1. journals the batch the first time it sees it, as dispatch always has, so
//!    a daemon that stops while the batch waits on a person brings it back
//!    asking again;
//! 2. decides each call in order through
//!    [`ToolService::decide`](super::ToolService::decide), charging the run's
//!    [`WriteLedger`] as it goes. A call a person has to approve stops the
//!    deciding: it is asked on the approval lane and the batch waits
//!    ([`AwaitingApproval`]) until the answer is applied, so a grant made for
//!    it covers the calls after it, as a person answering in order expects;
//! 3. once every call is decided, sends the batch to the lane with what was
//!    decided for each.
//!
//! A stage's seed calls (its refreshing regions) take the same road with
//! nothing journaled: they are the blueprint's, not a turn of the model's, and
//! the stage they fill is held by its own marker until they land.

use std::collections::HashMap;

use super::*;
use crate::pipeline::tool_verdicts::{
    DecideCtx, DecidedCall, Decision, ToolGrants, ToolVerdict, WriteLedger,
};

/// A batch whose lane calls are being decided before they run.
#[derive(Component)]
pub(crate) struct PendingBatch {
    /// The calls for the lane, in the order the model made them.
    lane_calls: Vec<leviath_providers::ToolCall>,
    /// What the dispatcher resolved itself, applied when the batch lands.
    context_results: Vec<(String, String)>,
    /// The execution id minted for each call.
    executions: HashMap<String, String>,
    /// Results carried from before a restart.
    recovered: Vec<crate::tool_bridge::ToolResult>,
    /// Files an accepted submission produced, by execution.
    produced: Vec<(String, Vec<leviath_core::output::Artifact>)>,
    /// Whether the batch record has been journaled, and if so the progress
    /// hook that records each completion and the ack the batch waits on.
    journal: Option<BatchJournal>,
    /// What has been decided, by call id.
    decided: HashMap<String, Decision>,
    /// Whether this is a stage's seed calls rather than a turn's batch.
    seeds: bool,
}

/// The journaling a batch did when it was first seen.
struct BatchJournal {
    progress: ToolProgress,
    ack: Option<(
        tokio::sync::oneshot::Receiver<crate::persistence_bridge::Appended>,
        String,
    )>,
}

/// The batch is held on a person's answer to an approval prompt for one of
/// its calls.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub(crate) struct AwaitingApproval {
    /// The call being asked about.
    pub call_id: String,
    /// What an approval for the stage or the run is remembered under.
    pub keys: Vec<String>,
    /// The bytes an approval charges the run.
    pub charge: u64,
}

/// The run's batch has run and its executors' write total is to be read back
/// into its [`WriteLedger`] once the batch lands.
#[derive(Component, Debug, Clone, Copy)]
pub(crate) struct WritesOut;

impl PendingBatch {
    /// A batch of `lane_calls` with what the dispatcher already resolved.
    pub(crate) fn new(
        lane_calls: Vec<leviath_providers::ToolCall>,
        context_results: Vec<(String, String)>,
        executions: HashMap<String, String>,
        recovered: Vec<crate::tool_bridge::ToolResult>,
        produced: Vec<(String, Vec<leviath_core::output::Artifact>)>,
    ) -> Self {
        Self {
            lane_calls,
            context_results,
            executions,
            recovered,
            produced,
            journal: None,
            decided: HashMap::new(),
            seeds: false,
        }
    }

    /// A stage's seed calls: decided like any batch, journaled not at all.
    pub(crate) fn seeds(calls: Vec<leviath_providers::ToolCall>) -> Self {
        Self {
            journal: Some(BatchJournal {
                progress: noop_progress(),
                ack: None,
            }),
            seeds: true,
            ..Self::new(calls, Vec::new(), HashMap::new(), Vec::new(), Vec::new())
        }
    }

    /// The call `call_id` is decided: `decision` it is.
    pub(crate) fn decide(&mut self, call_id: String, decision: Decision) {
        self.decided.insert(call_id, decision);
    }

    /// What was decided for the call `call_id`, once it is.
    #[cfg(test)]
    pub(crate) fn decision(&self, call_id: &str) -> Option<&Decision> {
        self.decided.get(call_id)
    }

    /// The lane call with id `call_id`, when the batch has one.
    pub(crate) fn call(&self, call_id: &str) -> Option<&leviath_providers::ToolCall> {
        self.lane_calls.iter().find(|c| c.id == call_id)
    }
}

/// What `dispatch_lane_batches` selects.
///
/// `&'static` is bevy's `WorldQuery` convention, not a claim about
/// lifetimes: the borrow is bound when the query is fetched.
type LaneBatchQuery = (
    Entity,
    &'static AgentState,
    Option<&'static crate::components::InferenceResult>,
    Option<&'static ContextWindow>,
    Option<&'static RunMetadata>,
    Option<&'static StageCursor>,
    Option<&'static InFlightWork>,
    &'static mut PendingBatch,
    Option<&'static mut ToolGrants>,
    Option<&'static mut WriteLedger>,
);

/// The resources a batch is journaled, asked about and sent through.
#[derive(bevy_ecs::system::SystemParam)]
pub(crate) struct LaneServices<'w> {
    /// The tool service that decides and runs the calls.
    pub service: Res<'w, ToolServiceRes>,
    /// The lane batches are sent on.
    pub stage: Res<'w, ToolStage>,
    /// Where what a run does is recorded, for its run file.
    pub persist: Option<Res<'w, super::JournalSender>>,
    /// Where world events are broadcast.
    pub sink: Option<Res<'w, crate::host::WorldEventSink>>,
    /// The hub an approval is asked through.
    pub hub: Option<Res<'w, InteractionHub>>,
    /// The lane an approval's answer comes back on.
    pub approvals: Option<Res<'w, crate::approval_prompt::ApprovalStage>>,
}

/// Journal, decide and send each held batch (see the module docs).
pub(crate) fn dispatch_lane_batches(
    mut batches: Query<LaneBatchQuery, Without<AwaitingApproval>>,
    lane: LaneServices,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    for (entity, state, result, window, metadata, cursor, in_flight, mut batch, grants, ledger) in
        batches.iter_mut()
    {
        crate::tick_scope::enter(entity);
        if batch.journal.is_none() {
            let result = result.expect("a turn's batch is held beside the turn");
            let journal = journal_batch(&lane, state, result, metadata, cursor, &batch);
            commands
                .entity(entity)
                .insert(crate::components::BatchExecutions {
                    ids: batch.executions.clone(),
                });
            batch.journal = Some(journal);
        }
        let mut no_grants = ToolGrants::default();
        let grants = grants.map_or(&mut no_grants, Mut::into_inner);
        let mut no_ledger = WriteLedger::default();
        let ledger = ledger.map_or(&mut no_ledger, Mut::into_inner);
        let stage_index = cursor.map_or(0, |c| c.index);
        let asked = loop {
            let Some(ask) = decide_in_order(&lane, entity, &mut batch, grants, ledger, stage_index)
            else {
                break false;
            };
            let call = batch
                .call(&ask.call_id)
                .cloned()
                .expect("an asked call is one of the batch's");
            match lane.hub.as_deref().zip(lane.approvals.as_deref()) {
                Some(prompts) => {
                    crate::approval_prompt::ask(
                        prompts,
                        crate::approval_prompt::ApprovalAsk {
                            entity,
                            agent_id: state.agent_id.clone(),
                            call,
                            stage: state.current_stage.clone(),
                            keys: ask.keys.clone(),
                        },
                    );
                    commands.entity(entity).insert(ask);
                    break true;
                }
                // No one to ask: the answer a prompt nobody answered gets.
                None => batch.decide(
                    ask.call_id,
                    Decision::Refuse(crate::approval_prompt::unanswered_approval_result(
                        &call.name, None,
                    )),
                ),
            }
        };
        if !asked {
            send(
                &lane,
                entity,
                window,
                in_flight,
                &mut batch,
                ledger.written,
                &mut commands,
            );
        }
    }
}

/// Decide every call not yet decided, in order. Stops at the first that needs
/// a person's approval, and returns what to ask.
fn decide_in_order(
    lane: &LaneServices,
    entity: Entity,
    batch: &mut PendingBatch,
    grants: &ToolGrants,
    ledger: &mut WriteLedger,
    stage_index: usize,
) -> Option<AwaitingApproval> {
    for call in batch.lane_calls.clone() {
        if batch.decided.contains_key(&call.id) {
            continue;
        }
        let ctx = DecideCtx {
            grants,
            written: ledger.written,
            stage_index,
        };
        let decision = match lane.service.0.decide(entity, &call, &ctx) {
            ToolVerdict::Run { charge } => {
                ledger.written = ledger.written.saturating_add(charge);
                Decision::Run
            }
            ToolVerdict::Interact { attended } => Decision::Interact { attended },
            ToolVerdict::Refuse(text) => Decision::Refuse(text),
            ToolVerdict::Ask { keys, charge } => {
                return Some(AwaitingApproval {
                    call_id: call.id,
                    keys,
                    charge,
                });
            }
        };
        batch.decided.insert(call.id, decision);
    }
    None
}

/// Journal the batch: the record, its artifacts, and the start of every lane
/// call, as dispatch has always done before a batch can run. Returns the
/// progress hook and the ack the batch's exec will wait on.
fn journal_batch(
    lane: &LaneServices,
    state: &AgentState,
    result: &crate::components::InferenceResult,
    metadata: Option<&RunMetadata>,
    cursor: Option<&StageCursor>,
    batch: &PendingBatch,
) -> BatchJournal {
    let dispatch = super::tools::BatchDispatch {
        calls: &result.tool_calls,
        executions: &batch.executions,
        inline: &batch.context_results,
        recovered: &batch.recovered,
        stage_index: cursor.map_or(0, |c| c.index),
        iteration: state.iteration,
        visit_id: &state.current_visit,
        requested_by: &result.attempt_id,
        response: &result.response,
    };
    // A batch record with the dispatcher's inline results pre-filled and every
    // lane call pending, plus a per-call progress hook that records each
    // completion. Worlds without a persistence lane or run metadata (tests,
    // unpersisted agents) dispatch unjournaled with a no-op progress.
    let journal = match (lane.persist.as_ref(), metadata) {
        (Some(persist), Some(md)) => {
            let ack_rx = persist.record_acked(&md.run_id, dispatch.record());
            super::tools::journal_artifacts(persist, &md.run_id, &batch.produced);
            // The calls finish off the tick, so their records wake the world:
            // one that lands while the rest of the batch runs is in the run's
            // file before the batch ends.
            let sender = persist.waking();
            let run_id = md.run_id.clone();
            let iteration = state.iteration;
            let minted = batch.executions.clone();
            let progress: ToolProgress = Arc::new(move |call_id: &str, result| {
                sender.record(
                    &run_id,
                    crate::runfile::record::RunRecord::ToolCallDone {
                        iteration,
                        call_id: call_id.to_string(),
                        // The attempt this completes, so a completion cannot
                        // be attached to a different attempt that shared the
                        // provider's id.
                        execution_id: minted.get(call_id).cloned().unwrap_or_default(),
                        result: result.clone(),
                        // The completion says only that the call finished:
                        // its verdict is not known here.
                        outcome: None,
                        at: chrono::Utc::now().timestamp(),
                    },
                );
            });
            // The run id travels with the ack: an ack only exists when the
            // batch was journaled for a known run, so pairing them here leaves
            // the waiter no impossible case to handle.
            BatchJournal {
                progress,
                ack: Some((ack_rx, md.run_id.clone())),
            }
        }
        _ => BatchJournal {
            progress: noop_progress(),
            ack: None,
        },
    };
    // Announce each lane-bound call. Inline results (context tools, refusals,
    // blocks) never reach the lane and are deliberately not announced.
    if let (Some(sink), Some(md)) = (lane.sink.as_ref(), metadata) {
        for call in &batch.lane_calls {
            let _ = sink.0.send(crate::host::WorldEvent::ToolCallStarted {
                run_id: md.run_id.clone(),
                agent_id: state.agent_id.clone(),
                call_id: call.id.clone(),
                execution_id: batch.executions.get(&call.id).cloned().unwrap_or_default(),
                tool: call.name.clone(),
            });
        }
    }
    journal
}

/// Send a fully decided batch to the lane, and move the agent to
/// `AwaitingTools`.
fn send(
    lane: &LaneServices,
    entity: Entity,
    window: Option<&ContextWindow>,
    in_flight: Option<&InFlightWork>,
    batch: &mut PendingBatch,
    written: u64,
    commands: &mut Commands,
) {
    let BatchJournal { progress, ack } = batch
        .journal
        .take()
        .expect("a batch is journaled before it is sent");
    let calls: Vec<DecidedCall> = std::mem::take(&mut batch.lane_calls)
        .into_iter()
        .map(|call| DecidedCall {
            decision: batch
                .decided
                .remove(&call.id)
                .expect("every call is decided before the batch is sent"),
            call,
        })
        .collect();
    // What a tool may read by name: every stored part the window holds right
    // now, offered whole so a stale offer never outlives the entry it came
    // from.
    let offered: Vec<leviath_core::mime::Part> = window
        .iter()
        .flat_map(|w| w.regions.iter())
        .flat_map(|r| r.content.iter())
        .flat_map(|e| e.content.stored().cloned())
        .collect();
    lane.service.0.offer_parts(entity, offered);
    let landed = LandedResults::default();
    let exec = lane
        .service
        .0
        .exec_decided(entity, calls, written, landed.keeping(progress));
    let exec = match ack {
        Some((ack, run_id)) => {
            super::tools::barrier_then(exec, ack, super::tools::BATCH_JOURNAL_ACK_TIMEOUT, run_id)
        }
        None => exec,
    };
    let cancel = crate::cancel::CancelToken::new();
    // The lane is alive for the world's lifetime; a failed send would only
    // happen during shutdown, where dropping the job is fine.
    lane.stage.stats.enqueued();
    let _ = lane.stage.jobs.send(ToolJob {
        entity,
        exec,
        cancel: cancel.clone(),
    });
    let mut agent = commands.entity(entity);
    agent.remove::<PendingBatch>().insert(WritesOut);
    // Seed calls land on the stage's own hold, which `collect_tools` releases;
    // a turn's batch waits as `AwaitingTools`, cancellable with the agent.
    if !batch.seeds {
        agent
            .insert(AwaitingTools)
            .insert(landed)
            .insert(ContextToolResults(std::mem::take(
                &mut batch.context_results,
            )));
        track_in_flight(commands, entity, in_flight, cancel);
    }
}

/// The runs `settle_write_ledgers` reads back: a batch that ran and has landed.
type LandedWrites = (
    With<WritesOut>,
    Without<AwaitingTools>,
    Without<crate::stage_seeds::PendingStageSeeds>,
);

/// Read a landed batch's write total back from the tool service into the
/// run's [`WriteLedger`]: what its executors measured once the calls ran
/// (a shell redirect's file, a script's writes) is only known there.
pub(crate) fn settle_write_ledgers(
    mut agents: Query<(Entity, Option<&mut WriteLedger>), LandedWrites>,
    service: Res<ToolServiceRes>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    for (entity, ledger) in agents.iter_mut() {
        crate::tick_scope::enter(entity);
        if let Some(mut ledger) = ledger {
            ledger.written = service.0.written(entity).unwrap_or(ledger.written);
        }
        commands.entity(entity).remove::<WritesOut>();
    }
}

#[cfg(test)]
#[path = "lane_batch_tests.rs"]
mod tests;
