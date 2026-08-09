//! Stage transitions: cursors, gates, stuck detection, spawning, and transition choices.

use super::*;

// ─── Stage transition ────────────────────────────────────────────────────────

/// The agent's blueprint (its stage graph), as a component.
#[derive(Component, Debug, Clone)]
pub struct AgentBlueprint(pub leviath_core::Blueprint);

/// The index of the agent's current stage within its blueprint.
#[derive(Component, Debug, Clone, Copy)]
pub struct StageCursor {
    /// Current stage index.
    pub index: usize,
}

/// Pre-resolved [`StageInference`] for every stage of the agent's blueprint,
/// built once when the agent is spawned (the CLI resolves each stage's provider,
/// model, and tool definitions). The transition system swaps the agent's
/// `StageInference` to the entry for its new stage by index.
#[derive(Component, Debug, Clone)]
pub struct StageInferences(pub Vec<StageInference>);

/// How many times the agent has entered each stage (for `max_revisits`).
#[derive(Component, Debug, Clone, Default)]
pub struct VisitCounts(pub std::collections::HashMap<String, usize>);

/// Pre-resolved per-stage setup, applied by `enter_stage` when an agent enters
/// a stage: inference parameters, tool-result routing, whether the stage accepts
/// live user input, an optional stage-specific context layout, and an optional
/// system prompt. Built once per stage when the agent is spawned (mirrors
/// [`StageInferences`]) so stage entry stays synchronous and query-friendly.
/// (Ported from the imperative loop's per-stage setup in the CLI executor.)
#[derive(Clone)]
pub struct StageSetup {
    /// Per-stage inference config (temperature / max output tokens).
    pub inference_config: InferenceConfig,
    /// Optional per-stage tool-result routing.
    pub routing: Option<leviath_core::ToolResultRouting>,
    /// Whether the stage delivers live user messages to the agent.
    pub accepts_messages: bool,
    /// Optional stage-specific context layout to swap to on entry.
    pub context_layout: Option<leviath_core::ContextLayout>,
    /// Optional stage instructions injected as pinned context on entry.
    pub system_prompt: Option<String>,
    /// The output shape resolved for this stage (agent, stage, and the
    /// launching caller's request combined), and whether the stage must produce
    /// one. `None` means no level asked for a shape.
    ///
    /// Held here as well as on [`StageInference`] because the two use it for
    /// different things: this copy is folded into the stage's system prompt, and
    /// that copy is what validates a submission at dispatch.
    pub output: Option<leviath_core::output::OutputSpec>,
}

/// Pre-resolved [`StageSetup`] for every stage of the agent's blueprint.
#[derive(Component, Clone)]
pub struct StageSetups(pub Vec<StageSetup>);

/// The stage completed with multiple candidate edges (or a single edge the stage
/// may decline); an LLM must choose. Holds the choosable edges for the async
/// transition-choice system.
#[derive(Component, Debug, Clone)]
pub struct AwaitingTransitionChoice(pub Vec<leviath_core::blueprint::TransitionEdge>);

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
    /// Advance to this stage index, applying the edge's context transform once
    /// the edge's gate (if any) is satisfied.
    /// Boxed rather than inline: `TransitionGate` grows every time a gate
    /// condition is added, and this variant is otherwise a `usize` and a small
    /// enum - carrying it by value made every `StageResolution` the size of the
    /// largest gate, including the five variants that hold nothing.
    Next(
        usize,
        leviath_core::blueprint::EdgeTransform,
        Option<Box<leviath_core::blueprint::TransitionGate>>,
    ),
    /// Multiple candidate edges - an LLM must choose among them.
    Choose(Vec<leviath_core::blueprint::TransitionEdge>),
    /// Not a transition after all - put the agent back to work in its current
    /// stage. Only a stuck interrupt produces this: it fires mid-stage, so when
    /// its escape edge is no longer available the stage must simply continue
    /// (falling through would end a stage the agent never said it had finished).
    Resume,
}

/// Find the first available edge with the given `condition` (e.g. `Error` or
/// `MaxIterations`) whose target exists and hasn't exhausted its revisit budget.
pub(crate) fn find_conditioned_edge_ref<'a>(
    blueprint: &leviath_core::Blueprint,
    stage: &'a leviath_core::Stage,
    visits: &std::collections::HashMap<String, usize>,
    condition: leviath_core::blueprint::TransitionCondition,
) -> Option<(usize, &'a leviath_core::blueprint::TransitionEdge)> {
    let transitions = stage.transitions.as_ref()?;
    transitions.values().find_map(|edge| {
        if edge.condition != condition {
            return None;
        }
        let idx = blueprint
            .stages
            .iter()
            .position(|s| s.name == edge.target)?;
        let within_budget = match blueprint.stages[idx].max_revisits {
            Some(max) => visits.get(&edge.target).copied().unwrap_or(0) <= max,
            None => true,
        };
        within_budget.then_some((idx, edge))
    })
}

/// As [`find_conditioned_edge_ref`], projected to the target index and a cloned
/// edge transform - what the transition systems need.
pub(crate) fn find_conditioned_edge(
    blueprint: &leviath_core::Blueprint,
    stage: &leviath_core::Stage,
    visits: &std::collections::HashMap<String, usize>,
    condition: leviath_core::blueprint::TransitionCondition,
) -> Option<(usize, leviath_core::blueprint::EdgeTransform)> {
    find_conditioned_edge_ref(blueprint, stage, visits, condition)
        .map(|(idx, edge)| (idx, edge.transform.clone()))
}

/// Resolve the next stage for a normally-completed stage without any LLM call.
/// (Ported from the synchronous portion of `graph::resolve_transition`; the
/// `Error`/`MaxIterations` auto-transitions don't apply to a normal completion,
/// and the LLM-choice case is returned as [`StageResolution::Choose`].)
pub(crate) fn resolve_transition_sync(
    blueprint: &leviath_core::Blueprint,
    stage: &leviath_core::Stage,
    stage_idx: usize,
    visits: &std::collections::HashMap<String, usize>,
) -> StageResolution {
    use leviath_core::blueprint::TransitionCondition;
    match &stage.transitions {
        None => {
            if stage_idx + 1 < blueprint.stages.len() {
                // A linear fall-through carries context as-is (Direct), and has
                // no edge to hang a gate on.
                StageResolution::Next(
                    stage_idx + 1,
                    leviath_core::blueprint::EdgeTransform::Direct,
                    None,
                )
            } else {
                StageResolution::Terminal
            }
        }
        Some(transitions) => {
            if transitions.is_empty() {
                return StageResolution::Terminal;
            }
            // Filter edges whose target hasn't exhausted its revisit budget.
            let available: Vec<&leviath_core::blueprint::TransitionEdge> = transitions
                .values()
                .filter(|e| match blueprint.find_stage(&e.target) {
                    Some(ts) => match ts.max_revisits {
                        Some(max) => visits.get(&e.target).copied().unwrap_or(0) <= max,
                        None => true,
                    },
                    None => false, // unknown target
                })
                .collect();
            // Only Always/LlmChoice edges are auto/LLM-followable on completion.
            let choosable: Vec<&leviath_core::blueprint::TransitionEdge> = available
                .into_iter()
                .filter(|e| {
                    matches!(
                        e.condition,
                        TransitionCondition::Always | TransitionCondition::LlmChoice
                    )
                })
                .collect();
            match choosable.len() {
                0 => {
                    // No followable edge left. If the stage never declared a
                    // normal (Always/LlmChoice) edge, this is a legitimate
                    // terminal whose conditioned edges are alternates. If it
                    // DID - and they were all filtered out above - the graph
                    // dead-ended mid-run, which must not read as success.
                    let declared_normal = transitions.values().any(|e| {
                        matches!(
                            e.condition,
                            TransitionCondition::Always | TransitionCondition::LlmChoice
                        )
                    });
                    if declared_normal {
                        StageResolution::DeadEnd
                    } else {
                        StageResolution::Terminal
                    }
                }
                1 if !stage.allow_complete => {
                    let idx = blueprint
                        .stages
                        .iter()
                        .position(|s| s.name == choosable[0].target)
                        .unwrap_or(0);
                    StageResolution::Next(
                        idx,
                        choosable[0].transform.clone(),
                        choosable[0].gate.clone().map(Box::new),
                    )
                }
                _ => StageResolution::Choose(choosable.into_iter().cloned().collect()),
            }
        }
    }
}

/// Marks a parent agent held at a `requires_children` stage boundary until all
/// its spawned sub-agents are terminal. Distinct from `FanOutWaiting` (which is
/// the fan-out split/merge wait).
#[derive(Component, Debug, Clone, Copy)]
pub struct WaitingForChildren;

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

/// Decide whether a chosen edge's [gate](leviath_core::blueprint::TransitionGate)
/// blocks the transition.
///
/// The failure this guards against: an agent can read and reason about a
/// codebase entirely through `shell` and arrive at the review stage having
/// changed nothing, producing a run
/// with no output. A `require_modifications` gate keeps it in the stage until it
/// has actually written something.
///
/// The gate passes when any of these hold:
/// - the stage advertises no file-modifying tool (it could never pass, so gating
///   it would only burn iterations);
/// - a modifying tool call succeeded in this stage;
/// - one was refused by the permission layer (the agent is trying and cannot);
/// - the gate names a region and that region is non-empty (the durable signal:
///   per-stage counters don't survive a daemon restart, but regions do).
///
/// When the gate's re-run budget is spent it gives up loudly, as
/// [`GateDecision::Forced`].
pub(crate) fn gate_blocks(
    gate: Option<&leviath_core::blueprint::TransitionGate>,
    stage: &leviath_core::Stage,
    progress: &StageProgress,
    window: &ContextWindow,
) -> GateDecision {
    let Some(gate) = gate else {
        return GateDecision::Pass;
    };
    // Checked before `require_modifications` and independently of it: an edge
    // may ask for a changed region without asking for a file write, and a
    // revise loop usually does exactly that.
    //
    // A missing baseline means the gate names a region the window does not
    // hold. A gate cannot demand an update to something that does not exist,
    // and blocking on it would strand the run, so it passes.
    if let Some(name) = &gate.require_region_updated
        && let (Some(before), Some(region)) = (
            progress.entry_region_digests.get(name),
            window.get_region(name),
        )
        && *before == region_digest(region)
    {
        return spend_gate_attempt(
            gate,
            stage,
            progress,
            gate.message.clone().unwrap_or_else(|| {
                format!(
                    "The `{name}` region is unchanged since this stage began. Whatever sent \
                     you back here was not answered by repeating the same content - revise it \
                     before moving on."
                )
            }),
        );
    }
    // Checked before `require_modifications` and independently of it: a stage
    // whose work is a set of items usually has no file write to require.
    //
    // A gate naming a region the window does not hold passes rather than
    // blocking - no amount of work could satisfy it, and stranding the run over
    // a typo in a region name would be worse than the missing check.
    if let Some(name) = &gate.require_no_open_items
        && let Some(region) = window.get_region(name)
    {
        let open = region.open_checklist_items();
        if !open.is_empty() {
            let cap = gate
                .max_attempts
                .unwrap_or(leviath_core::blueprint::DEFAULT_GATE_ATTEMPTS);
            if progress.gate_reentries >= cap {
                tracing::warn!(
                    stage = %stage.name,
                    open = open.len(),
                    attempts = cap,
                    "stage still has open checklist items after re-run attempts; proceeding"
                );
                return GateDecision::Forced;
            }
            let listed = open
                .iter()
                .map(|i| format!("{} {}", i.id, i.text))
                .collect::<Vec<_>>()
                .join("; ");
            return GateDecision::Block(gate.message.clone().unwrap_or_else(|| {
                format!(
                    "{} item(s) are still open in `{name}`: {listed}. Finish them, or use \
                     todo_done to drop the ones that no longer apply, before moving on.",
                    open.len()
                )
            }));
        }
    }
    if !gate.require_modifications {
        return GateDecision::Pass;
    }
    let can_modify = stage.available_tools.iter().any(|t| {
        let canonical = leviath_tools::canonical_tool_name(t);
        leviath_core::blueprint::MODIFYING_TOOLS.contains(&canonical)
            || gate
                .tools
                .iter()
                .any(|extra| leviath_tools::canonical_tool_name(extra) == canonical)
    });
    if !can_modify {
        return GateDecision::Pass;
    }
    if progress.modifying_tool_calls > 0 {
        return GateDecision::Pass;
    }
    if progress.blocked_modification_calls > 0 {
        tracing::warn!(
            stage = %stage.name,
            blocked = progress.blocked_modification_calls,
            "file modifications were denied by policy; letting the gated transition through"
        );
        return GateDecision::Pass;
    }
    if let Some(region) = &gate.region
        && window
            .get_region(region)
            .is_some_and(|r| !r.content.is_empty())
    {
        return GateDecision::Pass;
    }
    spend_gate_attempt(
        gate,
        stage,
        progress,
        gate.message.clone().unwrap_or_else(|| {
            "No file modifications were recorded in this stage. Changes made through the shell \
             (sed -i, tee, >, >>) are not tracked by the framework. Re-apply your changes with \
             edit_file or write_file before moving on."
                .to_string()
        }),
    )
}

/// Block with `nudge`, or give up and let the edge through once the gate's
/// re-run budget is spent.
///
/// Shared by every gate condition so one blueprint key (`max_attempts`) bounds
/// all of them: a gate that could block forever would strand the run, which is
/// worse than letting a questionable transition through with a warning.
fn spend_gate_attempt(
    gate: &leviath_core::blueprint::TransitionGate,
    stage: &leviath_core::Stage,
    progress: &StageProgress,
    nudge: String,
) -> GateDecision {
    let cap = gate
        .max_attempts
        .unwrap_or(leviath_core::blueprint::DEFAULT_GATE_ATTEMPTS);
    if progress.gate_reentries >= cap {
        tracing::warn!(
            stage = %stage.name,
            attempts = cap,
            "transition gate still unsatisfied after re-run attempts; proceeding"
        );
        return GateDecision::Forced;
    }
    GateDecision::Block(nudge)
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

/// What `resolve_transition` selects.
///
/// `&'static` is bevy's `WorldQuery` convention, not a claim about
/// lifetimes: the borrow is bound when the query is fetched.
type ResolveTransitionQuery = (
    Entity,
    &'static AgentBlueprint,
    &'static mut StageCursor,
    &'static mut AgentState,
    &'static mut StageProgress,
    &'static StageInferences,
    &'static StageSetups,
    &'static mut VisitCounts,
    &'static mut ContextWindow,
    Option<&'static StageOutcome>,
    Option<&'static mut crate::persistence::RunOutcomeFlags>,
    Option<&'static crate::persistence::RunMetadata>,
    Option<&'static crate::persistence::FinalOutput>,
);

/// Transition-resolution system: for each `ResolveTransition` agent, resolve the
/// next stage. Terminal ⇒ mark the agent `Complete`. A single/linear target ⇒
/// enter the new stage (swap its `StageInference`, reset stage progress, bump the
/// visit count) and loop to `ReadyToInfer`. Multiple candidate edges ⇒ hand off
/// to the async transition-choice system via `AwaitingTransitionChoice`.
pub fn resolve_transition(
    mut agents: Query<ResolveTransitionQuery, With<ResolveTransition>>,
    sink: Option<Res<crate::host::WorldEventSink>>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    use leviath_core::blueprint::TransitionCondition;
    for (
        entity,
        bp,
        mut cursor,
        mut state,
        mut progress,
        stage_infs,
        setups,
        mut visits,
        mut window,
        outcome,
        mut flags,
        metadata,
        submitted,
    ) in agents.iter_mut()
    {
        crate::tick_scope::enter(entity);
        // A pause that lands while a transition is pending must hold: entering
        // the next stage flips the agent back to Active. The marker stays put,
        // so the transition resolves on the first tick after resume.
        if state.status == AgentStatus::Paused {
            continue;
        }
        let stage = &bp.0.stages[cursor.index];
        // How the stage ended governs the transition: an error/max-iterations
        // outcome follows its conditioned edge (e.g. → error_recovery) if present.
        let resolution = match outcome {
            // An error/max-iterations edge is never gated: the stage already
            // failed, and holding it back to demand file changes would strand a
            // run that can't make any.
            Some(StageOutcome::Errored(message)) => {
                match find_conditioned_edge(&bp.0, stage, &visits.0, TransitionCondition::Error) {
                    Some((i, t)) => {
                        // Put the error where the recovery stage will read it;
                        // without an error edge the run terminates and the
                        // status already carries the message.
                        note_error(&mut window, &stage.name, message);
                        StageResolution::Next(i, t, None)
                    }
                    None => StageResolution::TerminalError,
                }
            }
            Some(StageOutcome::MaxIterations) => {
                // Whatever runs next - a max_iterations edge target, the normal
                // successor, or the transition-choice model - should know the
                // stage was cut off, not finished.
                note_max_iterations(&mut window, &stage.name, stage.max_iterations.unwrap_or(0));
                find_conditioned_edge(&bp.0, stage, &visits.0, TransitionCondition::MaxIterations)
                    .map(|(i, t)| StageResolution::Next(i, t, None))
                    .unwrap_or_else(|| {
                        resolve_transition_sync(&bp.0, stage, cursor.index, &visits.0)
                    })
            }
            Some(StageOutcome::Stuck(_)) => {
                // A stuck interrupt is mid-stage, not a stage end. If the escape
                // hatch went away between detection and here (its target spent
                // its last revisit), resume - falling through to
                // `resolve_transition_sync` would end a stage the agent never
                // said it had finished, e.g. shunting `implement` into `review`
                // with the work half-done.
                find_conditioned_edge(&bp.0, stage, &visits.0, TransitionCondition::Stuck)
                    .map(|(i, t)| StageResolution::Next(i, t, None))
                    .unwrap_or(StageResolution::Resume)
            }
            None => resolve_transition_sync(&bp.0, stage, cursor.index, &visits.0),
        };
        // A dead end resolves like a stage error: down the `error` edge when one
        // has budget left (this is what finally makes `error_recovery` reachable
        // for exhaustion, not just for provider failures), and otherwise the run
        // FAILS. It used to resolve as `Terminal`, so a wide-researcher whose
        // deep_dive ran `compare` out of revisits reported `complete` from the
        // middle of its graph with the output stage still pending and nothing
        // produced - success indistinguishable from the run that worked.
        let resolution = match resolution {
            StageResolution::DeadEnd => {
                let message = format!(
                    "stage '{}' dead-ended: every declared transition's target has spent \
                     its max_revisits budget before an output or terminal stage was reached",
                    stage.name
                );
                // A `dead_end` edge first, then the `error` edge. Both are
                // escapes from this exact situation, but one was declared *for*
                // it: an author who wrote both means the specific one to win,
                // and an `error` edge is also carrying provider failures.
                let escape =
                    find_conditioned_edge(&bp.0, stage, &visits.0, TransitionCondition::DeadEnd)
                        .or_else(|| {
                            find_conditioned_edge(
                                &bp.0,
                                stage,
                                &visits.0,
                                TransitionCondition::Error,
                            )
                        });
                match escape {
                    Some((i, t)) => {
                        note_error(&mut window, &stage.name, &message);
                        StageResolution::Next(i, t, None)
                    }
                    None => {
                        state.status = AgentStatus::Error { message };
                        StageResolution::TerminalError
                    }
                }
            }
            other => other,
        };
        match resolution {
            StageResolution::Terminal => {
                // A run that owed a final output and never produced one is not
                // a success. `require_final_output` forces past the obligation
                // rather than stranding the run - correct, since a later stage
                // may still answer - but nothing downgraded the *terminal*
                // status, so a run ended `complete` with no `final_output` on
                // disk. `lev result` already exits non-zero there, so the two
                // disagreed in exactly the case a caller most needs to know
                // about, and anything polling `status` read it as success.
                let owed_output = bp.0.stages.iter().any(|s| s.require_output);
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
            StageResolution::Next(idx, transform, gate) => {
                // Check the edge's gate BEFORE the transform runs: the transform
                // compacts/clears regions, and a held stage must keep its context.
                let gate = outcome.is_none().then_some(gate).flatten();
                match gate_blocks(gate.as_deref(), stage, &progress, &window) {
                    GateDecision::Block(nudge) => {
                        hold_for_gate(entity, &nudge, &mut progress, &mut window, &mut commands);
                        continue;
                    }
                    GateDecision::Forced => {
                        if let Some(flags) = flags.as_mut() {
                            flags.0.gates_forced += 1;
                        }
                    }
                    GateDecision::Pass => {}
                }
                // Reshape the outgoing context per the edge transform before the
                // new stage's layout/prompt setup.
                let to_compact = apply_edge_transform(&mut window, &transform);
                let setup = &setups.0[idx];
                let from = state.current_stage.clone();
                match enter_stage(
                    idx,
                    &bp.0,
                    setup,
                    StageEntry {
                        cursor: &mut cursor,
                        state: &mut state,
                        progress: &mut progress,
                        visits: &mut visits,
                        window: &mut window,
                    },
                ) {
                    Ok(visit) => {
                        // Entering a stage is active work; clears a prior error
                        // status when recovering down an `error` edge.
                        state.status = AgentStatus::Active;
                        let name = bp.0.stages[idx].name.clone();
                        emit_stage_transition(&sink, metadata, &state.agent_id, from, &name, visit);
                        let mut ec = commands.entity(entity);
                        ec.remove::<ResolveTransition>().remove::<StageOutcome>();
                        attach_stage_components(ec, stage_infs.0[idx].clone(), setup, idx, name);
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
}

pub(crate) fn enter_stage(
    idx: usize,
    blueprint: &leviath_core::Blueprint,
    setup: &StageSetup,
    entry: StageEntry<'_>,
) -> Result<usize, String> {
    let StageEntry {
        cursor,
        state,
        progress,
        visits,
        window,
    } = entry;
    cursor.index = idx;
    let name = blueprint.stages[idx].name.clone();
    state.current_stage = name.clone();
    state.accepts_messages = setup.accepts_messages;
    *progress = StageProgress::default();
    let visit = visits.0.entry(name).or_insert(0);
    *visit += 1;
    let visit = *visit;

    let result = apply_stage_context(setup, window).map(|()| visit);
    // After the layout swap, so the digest is of the region this stage will
    // actually work on rather than the one the previous stage left behind.
    progress.entry_region_digests = watched_region_digests(&blueprint.stages[idx], window);
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
    stage: &leviath_core::Stage,
    window: &ContextWindow,
) -> std::collections::HashMap<String, u64> {
    let mut digests = std::collections::HashMap::new();
    let Some(transitions) = &stage.transitions else {
        return digests;
    };
    for edge in transitions.values() {
        let Some(name) = edge
            .gate
            .as_ref()
            .and_then(|g| g.require_region_updated.as_ref())
        else {
            continue;
        };
        if let Some(region) = window.get_region(name) {
            digests.insert(name.clone(), region_digest(region));
        }
    }
    digests
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

/// Apply a stage's context setup to a window: swap to the stage's layout (if any)
/// and (re)inject its system prompt as pinned `[Stage instructions: …]` context,
/// clearing any previous stage's first. Returns `Err` only when the prompt
/// doesn't fit its region. Shared by [`enter_stage`] (transitions) and
/// [`build_agent`] (the first stage, at spawn).
pub(crate) fn apply_stage_context(
    setup: &StageSetup,
    window: &mut ContextWindow,
) -> Result<(), String> {
    if let Some(layout) = &setup.context_layout {
        crate::context_setup::apply_layout(window, layout);
    }

    // Inject stage instructions into the first pinned region (cacheable), or the
    // conversation region if there is none - clearing any prior stage's first.
    let target = window
        .regions
        .iter()
        .find(|r| matches!(r.kind, leviath_core::RegionKind::Pinned))
        .map(|r| r.name.clone())
        .unwrap_or_else(|| "conversation".to_string());
    if let Some(region) = window.regions.iter_mut().find(|r| r.name == target) {
        region.remove_entries_by_prefix("[Stage instructions:");
    }
    if let Some(sp) = &setup.system_prompt {
        let content = format!("[Stage instructions: {sp}]");
        let tokens = leviath_core::estimate_tokens(&content);
        window
            .add_to_region(&target, content, tokens)
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
/// same effect as [`resolve_transition`]'s linear-`Next` arm, but callable from
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
    let attach: Option<(StageInference, StageSetup, String)> = {
        let mut q = world.query::<(
            &AgentBlueprint,
            &mut StageCursor,
            &mut AgentState,
            &mut StageProgress,
            &StageInferences,
            &StageSetups,
            &mut VisitCounts,
            &mut ContextWindow,
        )>();
        let Ok((
            bp,
            mut cursor,
            mut state,
            mut progress,
            stage_infs,
            setups,
            mut visits,
            mut window,
        )) = q.get_mut(world, entity)
        else {
            return; // agent despawned
        };
        let setup = setups.0[target_idx].clone();
        let stage_inf = stage_infs.0[target_idx].clone();
        let name = bp.0.stages[target_idx].name.clone();
        let bp = bp.0.clone();
        match enter_stage(
            target_idx,
            &bp,
            &setup,
            StageEntry {
                cursor: &mut cursor,
                state: &mut state,
                progress: &mut progress,
                visits: &mut visits,
                window: &mut window,
            },
        ) {
            Ok(_) => Some((stage_inf, setup, name)),
            Err(message) => {
                state.status = AgentStatus::Error { message };
                None
            }
        }
    };

    // Phase 2 (borrow released): attach the new stage's components directly.
    let Some((stage_inf, setup, name)) = attach else {
        return;
    };
    let mut em = world.entity_mut(entity);
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
