//! Stage transitions: cursors, gates, stuck detection, spawning, and transition choices.

use super::*;
use crate::insert::RunSpecC;
use crate::spec::graph::{EdgeCarry, EdgeCondition, EdgeDef, GateDef, RunGraph, StageDef};
use crate::spec::names::{EdgeName, StageName};
use crate::state::TransitionReason;
use spec_view::StageToolOverrides;

// ─── Stage transition ────────────────────────────────────────────────────────

/// The parsed blueprint a run was spawned from, kept on runs that came through
/// [`spawn_agent_seeded`](super::spawn_agent_seeded) for the modules outside
/// the pipeline that still read it. The pipeline itself reads the run's spec.
#[derive(Component, Debug, Clone)]
pub struct AgentBlueprint(pub crate::spec::Blueprint);

/// The index of the agent's current stage within its graph.
#[derive(Component, Debug, Clone, Copy)]
pub struct StageCursor {
    /// Current stage index.
    pub index: usize,
}

/// Every stage's [`StageInference`], by position, as runs spawned through
/// [`spawn_agent_seeded`](super::spawn_agent_seeded) carry it for the modules
/// outside the pipeline that still read it. The pipeline works each stage's
/// inference out from the run's spec.
#[derive(Component, Debug, Clone)]
pub(crate) struct StageInferences(pub Vec<StageInference>);

/// How many times the agent has entered each stage (for `max_revisits`).
#[derive(Component, Debug, Clone, Default)]
pub(crate) struct VisitCounts(pub std::collections::HashMap<String, usize>);

/// What entering a stage sets up: inference parameters, tool-result routing,
/// whether the stage accepts live user input, an optional stage-specific
/// context layout, and an optional system prompt. Worked out from the run's
/// spec by [`stage_setup`](super::spec_view::stage_setup) as the stage is
/// entered.
#[derive(Clone, Default)]
pub(crate) struct StageSetup {
    /// Per-stage inference config (temperature / max output tokens).
    pub inference_config: InferenceConfig,
    /// Optional per-stage tool-result routing.
    pub routing: Option<crate::spec::ToolResultRouting>,
    /// Whether the stage delivers live user messages to the agent.
    pub accepts_messages: bool,
    /// Optional stage-specific context layout to swap to on entry.
    pub context_layout: Option<crate::spec::ContextLayout>,
    /// Regions this stage leaves out of its prompt.
    pub context_hide: Vec<String>,
    /// Regions this stage empties on entry.
    pub context_reset: Vec<String>,
    /// Optional stage instructions injected as pinned context on entry.
    pub system_prompt: Option<String>,
}

/// Every stage's [`StageSetup`], by position, kept beside [`StageInferences`]
/// for the same readers.
#[derive(Component, Clone)]
pub(crate) struct StageSetups(pub Vec<StageSetup>);

/// The stage completed with multiple candidate edges (or a single edge the stage
/// may decline); an LLM must choose. Holds the choosable edges for the async
/// transition-choice system.
#[derive(Component, Debug, Clone)]
pub(crate) struct AwaitingTransitionChoice(pub Vec<EdgeDef>);

/// The last edge the run took, and why: the stage it left, the stage it
/// entered, the edge's name when a declared edge was taken, and the visit it
/// started. Written by every system that moves a run between stages, and read
/// when the run's state is taken.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct LastTransition(pub crate::state::TransitionRecord);

/// Where a transition goes and how it gets there.
#[derive(Debug, Clone)]
pub(crate) struct NextStage {
    /// The stage entered, by position.
    pub idx: usize,
    /// What happens to the context on the way.
    pub carry: EdgeCarry,
    /// What must be true before the run may go.
    pub gate: Option<GateDef>,
    /// The edge taken, when a declared one was.
    pub edge: Option<EdgeName>,
    /// Why this edge.
    pub reason: TransitionReason,
}

impl NextStage {
    /// Taking `edge` to the stage at `idx`, because its condition held.
    fn along(idx: usize, edge: &EdgeDef) -> Self {
        Self {
            idx,
            carry: edge.carry.clone(),
            gate: edge.gate.clone(),
            edge: Some(edge.name.clone()),
            reason: TransitionReason::Condition,
        }
    }

    /// The same move with no gate: an escape from a stage that already failed
    /// is never held back.
    fn ungated(mut self) -> Self {
        self.gate = None;
        self
    }
}

/// The outcome of synchronously resolving a completed stage's transition.
pub(crate) enum StageResolution {
    /// No valid outgoing transition - the agent is done.
    Terminal,
    /// The stage errored and has no `error` edge - terminate the run as errored,
    /// preserving the error status the collect system already set.
    TerminalError,
    /// The stage DECLARES normal outgoing transitions, but every one of them is
    /// revisit-exhausted (or targets an unknown stage): the graph dead-ended in
    /// the middle. Distinct from [`Self::Terminal`] because reporting this as
    /// `Complete` is how a run silently ended at stage 2 of 5 with no output -
    /// the resolver routes it down the stage's `error` edge, or fails the run.
    DeadEnd,
    /// Advance along an edge, once its gate (if any) is satisfied. Boxed so
    /// the variants that hold nothing stay small.
    Next(Box<NextStage>),
    /// Multiple candidate edges - an LLM must choose among them.
    Choose(Vec<EdgeDef>),
    /// Not a transition after all - put the agent back to work in its current
    /// stage. Only a stuck interrupt produces this: it fires mid-stage, so when
    /// its escape edge is no longer available the stage must simply continue
    /// (falling through would end a stage the agent never said it had finished).
    Resume,
}

/// Whether the stage an edge enters exists and has revisits left.
fn target_available(
    graph: &RunGraph,
    edge: &EdgeDef,
    visits: &std::collections::HashMap<String, usize>,
) -> Option<usize> {
    let idx = spec_view::stage_index(graph, edge.to.as_str())?;
    let within_budget = match graph.stages[idx].max_revisits {
        Some(max) => visits.get(edge.to.as_str()).copied().unwrap_or(0) <= max as usize,
        None => true,
    };
    within_budget.then_some(idx)
}

/// Find the first available edge leaving `stage` with the given `condition`
/// (e.g. `Error` or `MaxIterations`) whose target exists and hasn't exhausted
/// its revisit budget.
pub(crate) fn find_conditioned_edge_ref<'a>(
    graph: &'a RunGraph,
    stage: &'a StageDef,
    visits: &std::collections::HashMap<String, usize>,
    condition: EdgeCondition,
) -> Option<(usize, &'a EdgeDef)> {
    graph
        .edges_from(stage.name.as_str())
        .filter(|edge| edge.when == condition)
        .find_map(|edge| target_available(graph, edge, visits).map(|idx| (idx, edge)))
}

/// As [`find_conditioned_edge_ref`], as the move the transition systems make.
/// Never gated: these edges are escapes from a stage that cannot finish.
pub(crate) fn find_conditioned_edge(
    graph: &RunGraph,
    stage: &StageDef,
    visits: &std::collections::HashMap<String, usize>,
    condition: EdgeCondition,
) -> Option<NextStage> {
    find_conditioned_edge_ref(graph, stage, visits, condition)
        .map(|(idx, edge)| NextStage::along(idx, edge).ungated())
}

/// Resolve the next stage for a normally-completed stage without any LLM call.
/// The `Error`/`MaxIterations` edges don't apply to a normal completion, and
/// the LLM-choice case is returned as [`StageResolution::Choose`]. A stage no
/// edge leaves goes on to the next stage in the graph, or ends the run when it
/// is the last.
pub(crate) fn resolve_transition_sync(
    graph: &RunGraph,
    stage: &StageDef,
    stage_idx: usize,
    visits: &std::collections::HashMap<String, usize>,
) -> StageResolution {
    let edges = spec_view::edges_from(graph, stage);
    if edges.is_empty() {
        return match stage_idx + 1 < graph.stages.len() {
            // A fall-through carries context as-is and has no edge to hang a
            // gate on.
            true => StageResolution::Next(Box::new(NextStage {
                idx: stage_idx + 1,
                carry: EdgeCarry::Direct,
                gate: None,
                edge: None,
                reason: TransitionReason::Condition,
            })),
            false => StageResolution::Terminal,
        };
    }
    let normal = |e: &EdgeDef| matches!(e.when, EdgeCondition::Always | EdgeCondition::LlmChoice);
    // Only Always/LlmChoice edges are auto/LLM-followable on completion, and
    // only while their target has revisits left.
    let choosable: Vec<(usize, &EdgeDef)> = edges
        .iter()
        .filter(|e| normal(e))
        .filter_map(|e| target_available(graph, e, visits).map(|idx| (idx, *e)))
        .collect();
    match choosable.as_slice() {
        // No followable edge left. If the stage never declared a normal edge,
        // this is a legitimate terminal whose conditioned edges are alternates.
        // If it DID - and they were all spent - the graph dead-ended mid-run,
        // which must not read as success.
        [] => match edges.iter().any(|e| normal(e)) {
            true => StageResolution::DeadEnd,
            false => StageResolution::Terminal,
        },
        [(idx, edge)] if !stage.allow_complete => {
            StageResolution::Next(Box::new(NextStage::along(*idx, edge)))
        }
        _ => StageResolution::Choose(choosable.into_iter().map(|(_, e)| e.clone()).collect()),
    }
}

/// Marks a parent agent held at a `requires_children` stage boundary until all
/// its spawned sub-agents are terminal. Distinct from `FanOutWaiting` (which is
/// the fan-out split/merge wait).
#[derive(Component, Debug, Clone, Copy)]
pub(crate) struct WaitingForChildren;

/// Whether an agent status is terminal (the run/child has finished).
///
/// Every collect system consults this before applying an outcome: a run that
/// reached a terminal state while its work was in flight must stay there, not be
/// walked back to `Active`/`Complete` by the result landing afterwards.
pub fn is_terminal_status(status: &AgentStatus) -> bool {
    matches!(
        status,
        AgentStatus::Complete | AgentStatus::Error { .. } | AgentStatus::Cancelled
    )
}

/// Hold an agent in its current stage after a gate refused the transition: inject
/// the nudge, count the re-entry, and put it back in front of the model. The
/// stage is *not* re-entered - `StageProgress` is deliberately preserved so the
/// stage's `max_iterations` still bounds the loop.
pub(crate) fn hold_for_gate(
    entity: Entity,
    nudge: &str,
    progress: &mut StageProgress,
    window: &mut ContextWindow,
    commands: &mut Commands,
) {
    crate::pipeline::response::inject_system_nudge(window, nudge);
    progress.gate_reentries += 1;
    commands
        .entity(entity)
        .remove::<ResolveTransition>()
        .remove::<AwaitingTransitionResponse>()
        .remove::<StageOutcome>()
        .insert(ReadyToInfer);
}

/// End a stage in failure the way its blueprint asked for.
///
/// Two things have to happen together and this is the only place that promises
/// both: the status carries the message for the terminal case, and
/// [`StageOutcome::Errored`] plus [`ResolveTransition`] hand the agent to
/// [`resolve_transition`], which follows the stage's `error`-conditioned edge
/// when one has revisits left.
///
/// Writing the status on its own silently discards the recovery the author
/// declared: setting `AgentStatus::Error` directly ends the run with the
/// stage's `error_recovery` target sitting unused in the graph and every stage
/// behind it still pending, because nothing ever consults the edge. The
/// distinction is invisible at the call site - both spellings read as "fail the
/// run" - so it lives in one helper rather than in a rule to remember.
///
/// Not for conditions that are terminal by nature (a cancel, a completed run).
/// This is for "this stage could not go on", which is exactly what an
/// `error` edge exists to answer.
///
/// The one exception, which writes the status directly and says so where it does
/// it: a run whose journal cannot be written (see
/// [`fail_runs_with_unwritable_journals`](super::fail_runs_with_unwritable_journals)).
/// A recovery stage is more work done on the same unwritable journal, and the
/// recovery's own history would go unrecorded too, so that run stops rather than
/// being routed.
pub(crate) fn fail_stage(
    commands: &mut Commands,
    entity: Entity,
    state: &mut AgentState,
    message: String,
) {
    state.status = AgentStatus::Error {
        message: message.clone(),
    };
    commands
        .entity(entity)
        .insert(StageOutcome::Errored(message))
        .insert(ResolveTransition);
}

/// [`fail_stage`] for an exclusive system, which has a `&mut World` and no
/// `Commands`.
///
/// A no-op for an entity that has already despawned, matching the rest of the
/// exclusive-system helpers: a run that went away mid-tick has nothing left to
/// route.
pub(crate) fn fail_stage_world(world: &mut World, entity: Entity, message: String) {
    let Ok(mut entity_mut) = world.get_entity_mut(entity) else {
        return;
    };
    if let Some(mut state) = entity_mut.get_mut::<AgentState>() {
        state.status = AgentStatus::Error {
            message: message.clone(),
        };
    }
    entity_mut
        .insert(StageOutcome::Errored(message))
        .insert(ResolveTransition);
}

/// What `resolve_transition` selects.
///
/// `&'static` is bevy's `WorldQuery` convention, not a claim about
/// lifetimes: the borrow is bound when the query is fetched.
type ResolveTransitionQuery = (
    Entity,
    &'static RunSpecC,
    &'static mut StageCursor,
    &'static mut AgentState,
    &'static mut StageProgress,
    Option<&'static StageToolOverrides>,
    &'static mut VisitCounts,
    &'static mut ContextWindow,
    Option<&'static StageOutcome>,
    Option<&'static mut crate::persistence::RunOutcomeFlags>,
    Option<&'static crate::persistence::RunMetadata>,
    Option<&'static crate::persistence::FinalOutput>,
    Option<&'static mut StageLedger>,
);

/// How a stage that ended in failure leaves: down its first escape edge of
/// `first` then `second`, with the failure noted where the next stage reads
/// it; or, with no escape, as a failed run.
fn escape(
    graph: &RunGraph,
    stage: &StageDef,
    visits: &VisitCounts,
    [first, second]: [EdgeCondition; 2],
    window: &mut ContextWindow,
    state: &mut AgentState,
    message: &str,
) -> StageResolution {
    let found = find_conditioned_edge(graph, stage, &visits.0, first)
        .or_else(|| find_conditioned_edge(graph, stage, &visits.0, second));
    match found {
        Some(next) => {
            // Put the error where the recovery stage will read it; without an
            // escape the run terminates and the status carries the message.
            note_error(window, stage.name.as_str(), message);
            StageResolution::Next(Box::new(next))
        }
        None => {
            state.status = AgentStatus::Error {
                message: message.to_string(),
            };
            StageResolution::TerminalError
        }
    }
}

/// Transition-resolution system: for each `ResolveTransition` agent, resolve the
/// next stage. Terminal ⇒ mark the agent `Complete`. A single/linear target ⇒
/// enter the new stage (swap its `StageInference`, reset stage progress, bump the
/// visit count) and loop to `ReadyToInfer`. Multiple candidate edges ⇒ hand off
/// to the async transition-choice system via `AwaitingTransitionChoice`.
pub(crate) fn resolve_transition(
    mut agents: Query<ResolveTransitionQuery, With<ResolveTransition>>,
    sink: Option<Res<crate::host::WorldEventSink>>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    for (
        entity,
        spec,
        mut cursor,
        mut state,
        mut progress,
        overrides,
        mut visits,
        mut window,
        outcome,
        mut flags,
        metadata,
        submitted,
        mut ledger,
    ) in agents.iter_mut()
    {
        crate::tick_scope::enter(entity);
        // A pause that lands while a transition is pending must hold: entering
        // the next stage flips the agent back to Active. The marker stays put,
        // so the transition resolves on the first tick after resume.
        if state.status == AgentStatus::Paused {
            continue;
        }
        let graph = &spec.0.graph;
        let stage = &graph.stages[cursor.index];
        // How the stage ended governs the transition: an error/max-iterations
        // outcome follows its conditioned edge (e.g. → error_recovery) if present.
        let resolution = match outcome {
            // `error` first, then `dead_end`. Both declare that this stage may
            // not be able to go on, and the dead-end arm below falls back the
            // other way; a run whose author wrote only the `dead_end` escape
            // should not die because what went wrong was spelled "error".
            Some(StageOutcome::Errored(message)) => escape(
                graph,
                stage,
                &visits,
                [EdgeCondition::Error, EdgeCondition::DeadEnd],
                &mut window,
                &mut state,
                message,
            ),
            Some(StageOutcome::MaxIterations) => {
                // Whatever runs next - a max_iterations edge target, the normal
                // successor, or the transition-choice model - should know the
                // stage was cut off, not finished.
                note_max_iterations(
                    &mut window,
                    stage.name.as_str(),
                    stage.max_iterations.unwrap_or(0) as usize,
                );
                find_conditioned_edge(graph, stage, &visits.0, EdgeCondition::MaxIterations)
                    .map(|next| StageResolution::Next(Box::new(next)))
                    .unwrap_or_else(|| {
                        resolve_transition_sync(graph, stage, cursor.index, &visits.0)
                    })
            }
            Some(StageOutcome::Stuck(_)) => {
                // A stuck interrupt is mid-stage, not a stage end. If the escape
                // hatch went away between detection and here (its target spent
                // its last revisit), resume - falling through to
                // `resolve_transition_sync` would end a stage the agent never
                // said it had finished.
                find_conditioned_edge(graph, stage, &visits.0, EdgeCondition::Stuck)
                    .map(|next| StageResolution::Next(Box::new(next)))
                    .unwrap_or(StageResolution::Resume)
            }
            None => resolve_transition_sync(graph, stage, cursor.index, &visits.0),
        };
        // A dead end resolves like a stage error: down the `dead_end` edge, then
        // the `error` edge, and otherwise the run FAILS. Resolving it as
        // `Terminal` would report `complete` from the middle of a graph, with the
        // output stage still pending and nothing produced.
        let resolution = match resolution {
            StageResolution::DeadEnd => escape(
                graph,
                stage,
                &visits,
                [EdgeCondition::DeadEnd, EdgeCondition::Error],
                &mut window,
                &mut state,
                &format!(
                    "stage '{}' dead-ended: every declared transition's target has spent \
                     its max_revisits budget before an output or terminal stage was reached",
                    stage.name
                ),
            ),
            other => other,
        };
        match resolution {
            StageResolution::Terminal => {
                // A run that owed a final output and never produced one is not
                // a success: `lev result` exits non-zero there, and anything
                // polling `status` must not read it as success either.
                let owed_output = graph.stages.iter().any(|s| s.require_output);
                state.status = match owed_output && submitted.is_none() {
                    true => AgentStatus::Error {
                        message: "the run finished without the final output it \
                                  requires; the stage that owes one never called \
                                  submit_output"
                            .to_string(),
                    },
                    false => AgentStatus::Complete,
                };
                commands
                    .entity(entity)
                    .remove::<ResolveTransition>()
                    .remove::<StageOutcome>();
            }
            // `DeadEnd` is in the pattern only for exhaustiveness: the
            // conversion above always turns it into `Next` or `TerminalError`.
            StageResolution::TerminalError | StageResolution::DeadEnd => {
                // Status was set to Error by the collect system (or by the
                // dead-end conversion above); just stop.
                commands
                    .entity(entity)
                    .remove::<ResolveTransition>()
                    .remove::<StageOutcome>();
            }
            StageResolution::Next(next) => {
                // Check the edge's gate BEFORE the transform runs: the transform
                // compacts/clears regions, and a held stage must keep its context.
                let gate = outcome.is_none().then_some(next.gate.as_ref()).flatten();
                let mut reason = next.reason;
                match gate_blocks(gate, stage, &progress, &window) {
                    GateDecision::Block(nudge) => {
                        hold_for_gate(entity, &nudge, &mut progress, &mut window, &mut commands);
                        continue;
                    }
                    GateDecision::Forced => {
                        reason = TransitionReason::Gate;
                        if let Some(flags) = flags.as_mut() {
                            flags.0.gates_forced += 1;
                        }
                    }
                    GateDecision::Pass => {}
                }
                // Reshape the outgoing context per the edge transform before the
                // new stage's layout/prompt setup.
                let to_compact = apply_edge_transform(&mut window, &next.carry);
                let idx = next.idx;
                let setup = spec_view::stage_setup(&spec.0, idx);
                let from = state.current_stage.clone();
                match enter_stage(
                    idx,
                    graph,
                    &setup,
                    StageEntry {
                        cursor: &mut cursor,
                        state: &mut state,
                        progress: &mut progress,
                        visits: &mut visits,
                        window: &mut window,
                        ledger: ledger.as_deref_mut(),
                    },
                ) {
                    Ok(visit) => {
                        // Entering a stage is active work; clears a prior error
                        // status when recovering down an `error` edge.
                        state.status = AgentStatus::Active;
                        let name = graph.stages[idx].name.to_string();
                        let taken = transition_record(&from, &state, next.edge.clone(), reason);
                        emit_stage_transition(&sink, metadata, &state.agent_id, from, &name, visit);
                        let mut ec = commands.entity(entity);
                        ec.remove::<ResolveTransition>().remove::<StageOutcome>();
                        taken.into_iter().for_each(|t| {
                            ec.insert(t);
                        });
                        let inference = spec_view::stage_inference(&spec.0, idx, overrides);
                        attach_stage_components(ec, inference, &setup, idx, name);
                        if !to_compact.is_empty() {
                            commands
                                .entity(entity)
                                .insert(PendingEdgeCompact(to_compact));
                        }
                    }
                    Err(message) => {
                        state.status = AgentStatus::Error { message };
                        commands
                            .entity(entity)
                            .remove::<ResolveTransition>()
                            .remove::<StageOutcome>();
                    }
                }
            }
            StageResolution::Choose(edges) => {
                commands
                    .entity(entity)
                    .remove::<ResolveTransition>()
                    .remove::<StageOutcome>()
                    .insert(AwaitingTransitionChoice(edges));
            }
            StageResolution::Resume => {
                // `StageProgress::stuck_fired` is already set, so this cannot
                // ping-pong with `detect_stuck_stage`; the stage now simply runs
                // out to its ordinary `max_iterations`.
                commands
                    .entity(entity)
                    .remove::<ResolveTransition>()
                    .remove::<StageOutcome>()
                    .insert(ReadyToInfer);
            }
        }
    }
}

/// The record of a move a run just made, from the stage named `from` to the
/// one `state` is now in. `None` when either name is not a valid stage name,
/// which a run built from a checked graph never has.
pub(crate) fn transition_record(
    from: &str,
    state: &AgentState,
    edge: Option<EdgeName>,
    reason: TransitionReason,
) -> Option<LastTransition> {
    Some(LastTransition(crate::state::TransitionRecord {
        from: StageName::new(from).ok()?,
        to: StageName::new(state.current_stage.as_str()).ok()?,
        edge,
        reason,
        visit: state.current_visit.clone(),
    }))
}

/// Enter the stage at `idx`: update the cursor + current-stage name, reset
/// per-stage progress, bump the visit count, set `accepts_messages`, and apply the
/// stage's context setup - swap to its layout (if any) and (re)inject its system
/// prompt as pinned `[Stage instructions: …]` context, replacing the previous
/// stage's. (Ported from the imperative loop's per-stage setup.)
///
/// Returns `Err` only when the system prompt doesn't fit its region - the same
/// hard failure the imperative loop raises; the caller marks the agent `Error`.
/// `Ok` carries the stage's updated visit count (this entry included), which the
/// transition systems stamp into the [`StageTransition`](crate::host::WorldEvent)
/// event.
/// The per-agent components entering a stage rewrites.
///
/// Borrowed together because entering a stage is one atomic edit across all
/// five: the cursor moves, per-stage progress resets, the visit count bumps,
/// `accepts_messages` is set from the new stage's mode, and the window is
/// re-laid-out. Doing them through five separate queries over the same entity
/// would cost five passes to say one thing.
pub(crate) struct StageEntry<'a> {
    /// Where in the blueprint the agent is.
    pub cursor: &'a mut StageCursor,
    /// The agent's live state.
    pub state: &'a mut AgentState,
    /// Per-stage counters, reset on entry.
    pub progress: &'a mut StageProgress,
    /// How many times each stage has been entered.
    pub visits: &'a mut VisitCounts,
    /// The context window, re-laid-out for the new stage.
    pub window: &'a mut ContextWindow,
    /// The durable per-stage ledger, whose visit list is cut here.
    ///
    /// This is the only moment the boundary between two visits is exact.
    /// Reconciling it on the persist tick instead would merge a stage entered
    /// and left between two ticks into whichever visit happened to be open, and
    /// attribute that stay's calls to it - which is the misattribution the
    /// per-visit split exists to remove.
    ///
    /// Optional because a bare agent driven by a test has no ledger.
    pub ledger: Option<&'a mut StageLedger>,
}

pub(crate) fn enter_stage(
    idx: usize,
    graph: &RunGraph,
    setup: &StageSetup,
    entry: StageEntry<'_>,
) -> Result<usize, String> {
    let StageEntry {
        cursor,
        state,
        progress,
        visits,
        window,
        ledger,
    } = entry;
    // Before the cursor moves, while `cursor.index` still names the stage being
    // left. A self-transition closes and reopens: it is an entry like any other,
    // and the visit number the transition event carries counts it as one.
    if let Some(ledger) = ledger {
        let at = chrono::Utc::now().timestamp();
        if let Some(rec) = ledger.0.get_mut(cursor.index) {
            rec.close_visit(at);
        }
        if let Some(rec) = ledger.0.get_mut(idx) {
            // Minted here, where the stay begins, and carried on the run so that
            // everything dispatched during it records the visit it belongs to.
            // A visit past the ledger's cap keeps no record of its own, and the
            // id still names it: what it costs is in the stage's own totals.
            let visit = leviath_core::execution::mint_visit_id();
            state.current_visit = visit.clone();
            rec.begin_visit(at, visit);
        }
    }
    cursor.index = idx;
    let name = graph.stages[idx].name.to_string();
    state.current_stage = name.clone();
    state.accepts_messages = setup.accepts_messages;
    *progress = StageProgress::default();
    let visit = visits.0.entry(name).or_insert(0);
    *visit += 1;
    let visit = *visit;

    let result = apply_stage_context(setup, window).map(|()| visit);
    // After the layout swap, so the digest is of the region this stage will
    // actually work on rather than the one the previous stage left behind.
    progress.entry_region_digests = watched_region_digests(graph, &graph.stages[idx], window);
    result
}

/// Content digests of the regions this stage's outgoing gates watch.
///
/// Keyed by region name and taken at stage entry, so [`gate_blocks`] can ask
/// whether *this pass* changed anything rather than whether the region merely
/// has content. A region a gate names but the window does not hold is absent
/// here, and an absent digest reads as "no baseline", which the gate treats as
/// changed - a gate cannot demand an update to something that does not exist.
pub(crate) fn watched_region_digests(
    graph: &RunGraph,
    stage: &StageDef,
    window: &ContextWindow,
) -> std::collections::HashMap<String, u64> {
    graph
        .edges_from(stage.name.as_str())
        .filter_map(|edge| edge.gate.as_ref()?.require_region_updated.as_ref())
        .filter_map(|name| {
            let region = window.get_region(name.as_str())?;
            Some((name.to_string(), region_digest(region)))
        })
        .collect()
}

/// A hash of everything a region currently holds.
///
/// Content only: token counts and timestamps would make an unchanged region
/// look changed, which is the failure this gate exists to prevent.
pub(crate) fn region_digest(region: &leviath_core::Region) -> u64 {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for entry in &region.content {
        entry.content.hash(&mut hasher);
    }
    hasher.finish()
}

/// Push a [`StageTransition`](crate::host::WorldEvent::StageTransition) event
/// into the world's event stream. A no-op in worlds that don't stream (no
/// [`WorldEventSink`](crate::host::WorldEventSink) resource) and for bare
/// agents without run metadata.
pub(crate) fn emit_stage_transition(
    sink: &Option<Res<crate::host::WorldEventSink>>,
    metadata: Option<&crate::persistence::RunMetadata>,
    agent_id: &str,
    from: String,
    to: &str,
    iteration: usize,
) {
    if let (Some(sink), Some(md)) = (sink.as_ref(), metadata) {
        let _ = sink.0.send(crate::host::WorldEvent::StageTransition {
            run_id: md.run_id.clone(),
            agent_id: agent_id.to_string(),
            from,
            to: to.to_string(),
            iteration,
        });
    }
}

/// Which region a stage's instructions are written into.
///
/// A declared [`STAGE_INSTRUCTIONS_REGION`] when there is one, and it is moved
/// to the end of the region list so it renders after every other pinned block.
/// That ordering is the point: pinned regions carry `CacheHint::Always` and are
/// assembled in list order, so instructions sitting anywhere but last put a
/// per-stage string *in front of* the shared prefix - and changing stage then
/// rewrites the head of the prefix and invalidates everything behind it. Last
/// means the bytes in front stay identical across a transition.
///
/// Otherwise the fallback target: the first pinned region, or `conversation`
/// when a layout declares no pinned region at all.
///
/// [`STAGE_INSTRUCTIONS_REGION`]: crate::spec::layout::STAGE_INSTRUCTIONS_REGION
fn stage_instructions_target(window: &mut ContextWindow) -> String {
    let declared = crate::spec::layout::STAGE_INSTRUCTIONS_REGION;
    if let Some(at) = window.regions.iter().position(|r| r.name == declared) {
        if at + 1 < window.regions.len() {
            let region = window.regions.remove(at);
            window.regions.push(region);
        }
        return declared.to_string();
    }
    window
        .regions
        .iter()
        .find(|r| matches!(r.kind, leviath_core::RegionKind::Pinned))
        .map(|r| r.name.clone())
        .unwrap_or_else(|| "conversation".to_string())
}

/// Apply a stage's context setup to a window: swap to the stage's layout (if any)
/// and (re)inject its system prompt as pinned `[Stage instructions: …]` context,
/// clearing any previous stage's first. Returns `Err` only when the prompt
/// doesn't fit its region. Shared by [`enter_stage`] (transitions) and
/// [`build_agent`] (the first stage, at spawn).
pub(crate) fn apply_stage_context(
    setup: &StageSetup,
    window: &mut ContextWindow,
) -> Result<(), String> {
    // The hidden set describes the stage being entered and nothing else. A
    // stage with a layout of its own gets exactly what that layout leaves
    // out; a stage without one carries everything (inheriting the previous
    // stage's hidden set would cost a stage following a narrowed one regions
    // it never asked to lose); and `hide` then removes what this stage's own
    // instructions never read.
    match &setup.context_layout {
        Some(layout) => crate::context_setup::apply_layout(window, layout),
        None => window.hidden.clear(),
    }
    for name in &setup.context_hide {
        if !crate::spec::blueprint::ALWAYS_VISIBLE_REGIONS.contains(&name.as_str()) {
            window.hidden.insert(name.clone());
        }
    }
    // `reset` empties a region as the stage is entered, so it starts on a clean
    // slate - a describe stage reading its image from a region of its own with
    // none of the drawing stage's conversation carried in. Emptied, not
    // hidden: a later stage sees the fresh region, not the old turns.
    for name in &setup.context_reset {
        if let Some(region) = window.regions.iter_mut().find(|r| &r.name == name) {
            region.clear();
        }
    }

    let target = stage_instructions_target(window);
    if let Some(region) = window.regions.iter_mut().find(|r| r.name == target) {
        if target == crate::spec::layout::STAGE_INSTRUCTIONS_REGION {
            // The whole region is ours, so the previous stage's prompt goes by
            // emptying it. The fallback below cannot do that - it shares a
            // region with the author's own content - and has to identify its
            // own entries by their prefix, which silently removes any author
            // content that happens to start with the same words.
            region.clear();
        } else {
            region.remove_entries_by_prefix("[Stage instructions:");
        }
    }
    if let Some(sp) = &setup.system_prompt {
        let content = format!("[Stage instructions: {sp}]");
        let tokens = leviath_core::estimate_tokens(&content);
        window
            .add_to_region_caused(
                leviath_core::ContextCause::Transform,
                &target,
                content,
                tokens,
            )
            .map_err(|e| {
                format!(
                    "stage system prompt (~{tokens} tokens) does not fit context region \
                 '{target}': {e}. Increase that region's max_tokens (or shorten the prompt)."
                )
            })?;
    }
    Ok(())
}

/// Finish a successful stage entry: attach the new stage's inference config,
/// tool-result routing (present ⇒ insert, absent ⇒ clear the stale one), and its
/// pre-resolved [`StageInference`], then mark the agent `ReadyToInfer`. Shared by
/// both the synchronous and LLM-choice transition paths.
pub(crate) fn attach_stage_components(
    mut entity: bevy_ecs::system::EntityCommands,
    stage_inf: StageInference,
    setup: &StageSetup,
    stage_index: usize,
    stage_name: String,
) {
    entity
        .insert(stage_inf)
        .insert(setup.inference_config.clone())
        .insert(StageJustEntered {
            index: stage_index,
            name: stage_name,
        })
        // A fresh stage re-arms its interaction points and its required-region
        // and required-output gates: each stage owes its own, and gets its own
        // budget of attempts to produce it.
        .remove::<crate::interaction_points::InteractionPointCursor>()
        .remove::<crate::interaction_points::InteractionPointRounds>()
        .remove::<RequiredReentries>()
        .remove::<OutputReentries>()
        // A fan-out stage owes its own workers on every entry. Only the
        // "already did it" marker is cleared: `PreviousWorkItems` deliberately
        // survives, because it is what tells the second round what the first
        // one already covered.
        .remove::<FanOutReentries>()
        .remove::<crate::fanout::FannedOut>()
        .insert(ReadyToInfer);
    match &setup.routing {
        Some(routing) => {
            entity.insert(crate::components::ToolResultRoutingComponent {
                routing: routing.clone(),
            });
        }
        None => {
            entity.remove::<crate::components::ToolResultRoutingComponent>();
        }
    }
}

/// Force an agent into the stage at `target_idx` via direct world access - the
/// same effect as `resolve_transition`'s linear-`Next` arm, but callable from
/// an exclusive system (e.g. the fan-out collector jumping to its `merge_stage`)
/// or the daemon (spawning a fan-out worker directly at its worker stage) where no
/// [`Commands`] queue is available. On a system-prompt overflow the agent is
/// marked `Error`, mirroring the transition systems.
pub fn force_transition(world: &mut World, agent: crate::world::AgentId, target_idx: usize) {
    // Moving the wrong agent to a stage is how a run silently ends up somewhere
    // its blueprint never sent it.
    let Some(entity) = agent.resolve_in(world) else {
        return;
    };
    // Phase 1 (scoped borrow): mutate the agent's own state via `enter_stage`,
    // returning the components Phase 2 must insert - or `None` if the agent is
    // gone or its system prompt overflowed (already marked `Error` in-place).
    let attach: Option<(StageInference, StageSetup, String, Option<LastTransition>)> = {
        let mut q = world.query::<(
            &RunSpecC,
            &mut StageCursor,
            &mut AgentState,
            &mut StageProgress,
            Option<&StageToolOverrides>,
            &mut VisitCounts,
            &mut ContextWindow,
            Option<&mut StageLedger>,
        )>();
        let Ok((
            spec,
            mut cursor,
            mut state,
            mut progress,
            overrides,
            mut visits,
            mut window,
            mut ledger,
        )) = q.get_mut(world, entity)
        else {
            return; // agent despawned
        };
        let spec = spec.0.clone();
        let setup = spec_view::stage_setup(&spec, target_idx);
        let stage_inf = spec_view::stage_inference(&spec, target_idx, overrides);
        let name = spec.graph.stages[target_idx].name.to_string();
        let from = state.current_stage.clone();
        match enter_stage(
            target_idx,
            &spec.graph,
            &setup,
            StageEntry {
                cursor: &mut cursor,
                state: &mut state,
                progress: &mut progress,
                visits: &mut visits,
                window: &mut window,
                ledger: ledger.as_deref_mut(),
            },
        ) {
            Ok(_) => {
                let taken = transition_record(&from, &state, None, TransitionReason::Forced);
                Some((stage_inf, setup, name, taken))
            }
            Err(message) => {
                state.status = AgentStatus::Error { message };
                None
            }
        }
    };

    // Phase 2 (borrow released): attach the new stage's components directly.
    let Some((stage_inf, setup, name, taken)) = attach else {
        return;
    };
    let mut em = world.entity_mut(entity);
    taken.into_iter().for_each(|t| {
        em.insert(t);
    });

    em.insert(stage_inf)
        .insert(setup.inference_config.clone())
        .insert(StageJustEntered {
            index: target_idx,
            name,
        })
        .insert(ReadyToInfer);
    match &setup.routing {
        Some(routing) => {
            em.insert(crate::components::ToolResultRoutingComponent {
                routing: routing.clone(),
            });
        }
        None => {
            em.remove::<crate::components::ToolResultRoutingComponent>();
        }
    }
}
