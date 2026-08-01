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
    /// Advance to this stage index, applying the edge's context transform once
    /// the edge's gate (if any) is satisfied.
    Next(
        usize,
        leviath_core::blueprint::EdgeTransform,
        Option<leviath_core::blueprint::TransitionGate>,
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

/// How often (in per-stage iterations) [`check_workspace_health`] stats the
/// agent's working directory. One `metadata` call every few iterations is far
/// cheaper than the tool failures it replaces.
pub const WORKSPACE_CHECK_INTERVAL: usize = 5;

/// Workspace health guard: fail a run whose working directory has disappeared.
///
/// The motivating failure: an external harness deleted the workspace out from
/// under running agents, which then spent every remaining iteration collecting
/// `No such file or directory` from their tools - 16-17 of them in the observed
/// runs - with no way back. Nothing can recreate a deleted checkout from inside
/// the agent, so this stops immediately with a message that names the real
/// problem, instead of routing to error recovery to flail more cheaply.
#[allow(clippy::type_complexity)]
pub fn check_workspace_health(
    mut agents: Query<
        (
            Entity,
            &RunMetadata,
            &StageProgress,
            &mut AgentState,
            Option<&mut crate::persistence::RunOutcomeFlags>,
        ),
        With<ReadyToInfer>,
    >,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    for (entity, md, progress, mut state, flags) in agents.iter_mut() {
        crate::tick_scope::enter(entity);
        if state.status != AgentStatus::Active {
            continue;
        }
        if progress.iterations % WORKSPACE_CHECK_INTERVAL != 0 {
            continue;
        }
        if std::fs::metadata(&md.workdir).is_ok_and(|m| m.is_dir()) {
            continue;
        }
        tracing::error!(
            run_id = %md.run_id,
            workdir = %md.workdir,
            "working directory is gone; failing the run"
        );
        state.status = AgentStatus::Error {
            message: format!("workspace '{}' is no longer accessible", md.workdir),
        };
        if let Some(mut flags) = flags {
            flags.0.workspace_lost = true;
        }
        commands.entity(entity).remove::<ReadyToInfer>();
    }
}

/// Max-iterations guard: for each `ReadyToInfer` agent whose per-stage inference
/// count has reached the stage's `max_iterations`, end the stage (routing to a
/// `max_iterations` edge if one exists, else a normal transition) instead of
/// running another inference. Ported from the imperative `run_autonomous` cap.
#[allow(clippy::type_complexity)]
pub fn enforce_max_iterations(
    mut agents: Query<
        (
            Entity,
            &AgentState,
            &AgentBlueprint,
            &StageCursor,
            &StageProgress,
            Option<&mut crate::persistence::RunOutcomeFlags>,
        ),
        With<ReadyToInfer>,
    >,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    for (entity, state, bp, cursor, progress, flags) in agents.iter_mut() {
        crate::tick_scope::enter(entity);
        if state.status != AgentStatus::Active {
            continue;
        }
        let max = bp.0.stages[cursor.index].max_iterations.unwrap_or(0);
        if max > 0 && progress.iterations >= max {
            // Record it on the run: a stage that ran out of iterations is one of
            // the ways a run ends up with nothing to show (issue #107).
            if let Some(mut flags) = flags {
                flags.0.max_iterations_hit += 1;
            }
            commands
                .entity(entity)
                .remove::<ReadyToInfer>()
                .insert(ResolveTransition)
                .insert(StageOutcome::MaxIterations);
        }
    }
}

/// The context region a stuck diagnosis is written to when the blueprint declares
/// one. Pinned by convention, so the note survives the edge transform into the
/// stage that has to act on it.
pub(crate) const STUCK_REPORT_REGION: &str = "stuck_report";

/// The per-stage numbers a [`StuckConfig`](leviath_core::blueprint::StuckConfig)
/// is evaluated against.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct StuckMetrics {
    /// Inferences run in this stage.
    pub iterations: usize,
    /// Wall-clock seconds since the stage clock was stamped.
    pub elapsed_secs: u64,
    /// Total tool calls made in this stage.
    pub tool_calls: usize,
    /// The most-churned path this stage and how many write/edit calls it took.
    pub hottest_edit: Option<(String, usize)>,
}

/// Evaluate a stage's metrics against a stuck edge's thresholds, returning a
/// human-readable reason for the first one that trips.
///
/// Ordered most-diagnostic first: file churn names the actual mistake, while
/// iterations, tool calls and wall clock are only symptoms of it.
pub(crate) fn detect_stuck(
    cfg: &leviath_core::blueprint::StuckConfig,
    m: &StuckMetrics,
) -> Option<String> {
    if let (Some(limit), Some((path, hits))) = (cfg.after_same_file_edits, m.hottest_edit.as_ref())
        && *hits >= limit
    {
        return Some(format!(
            "you have written or edited '{path}' {hits} times in this stage without \
             resolving the task - the problem is very likely not in that file"
        ));
    }
    if let Some(limit) = cfg.after_iterations
        && m.iterations >= limit
    {
        return Some(format!(
            "you have run {} inference turns in this stage without finishing it",
            m.iterations
        ));
    }
    if let Some(limit) = cfg.after_tool_calls
        && m.tool_calls >= limit
    {
        return Some(format!(
            "you have made {} tool calls in this stage without finishing it",
            m.tool_calls
        ));
    }
    if let Some(limit) = cfg.after_minutes
        && m.elapsed_secs >= limit as u64 * 60
    {
        return Some(format!(
            "you have spent {} minutes in this stage without finishing it",
            m.elapsed_secs / 60
        ));
    }
    None
}

/// The single most-edited path in a stage. Ties break on path name so the
/// diagnosis is deterministic regardless of `HashMap` iteration order.
pub(crate) fn hottest_edit(
    edits: &std::collections::HashMap<String, usize>,
) -> Option<(String, usize)> {
    edits
        .iter()
        .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
        .map(|(path, n)| (path.clone(), *n))
}

/// Write the "why you're stuck" note where the next stage will read it: the
/// blueprint's `stuck_report` region when it declares one, else `conversation`
/// (which every blueprint is required to declare). Best-effort, like the
/// repetition nudge - an overflowing region silently drops the note.
pub(crate) fn note_stuck(window: &mut ContextWindow, stage: &str, reason: &str) {
    let region = if window.get_region(STUCK_REPORT_REGION).is_some() {
        STUCK_REPORT_REGION
    } else {
        "conversation"
    };
    let content = format!(
        "[Stuck detected in stage '{stage}'] {reason}. Stop repeating what you have been \
         doing. Re-read the original task, separate what you have actually verified from \
         what you assumed, and take a different approach - including reverting changes \
         that made things worse."
    );
    let tokens = leviath_core::estimate_tokens(&content);
    let _ = window.add_to_region(region, content, tokens);
}

/// Stuck-detection guard: for each `ReadyToInfer` agent whose current stage
/// declares a `stuck`-conditioned edge, evaluate that edge's thresholds against
/// the stage's progress. When one trips, write the diagnosis into context and
/// route the agent down the stuck edge (`ResolveTransition` +
/// [`StageOutcome::Stuck`]) instead of running another inference.
///
/// Fires at most once per stage entry (`StageProgress::stuck_fired`, cleared by
/// `enter_stage`'s progress reset), and never once the stuck edge's target has
/// spent its `max_revisits` - an exhausted escape hatch must leave the agent
/// working the stage normally (its `max_iterations` is still the hard cap) rather
/// than kick it out down an unrelated edge.
#[allow(clippy::type_complexity)]
pub fn detect_stuck_stage(
    mut agents: Query<
        (
            Entity,
            &AgentState,
            &AgentBlueprint,
            &StageCursor,
            &mut StageProgress,
            &VisitCounts,
            &mut ContextWindow,
            Option<&mut StageIoBuffer>,
        ),
        With<ReadyToInfer>,
    >,
    mut commands: Commands,
) {
    use leviath_core::blueprint::TransitionCondition;
    let now = chrono::Utc::now().timestamp();
    crate::tick_scope::clear();
    for (entity, state, bp, cursor, mut progress, visits, mut window, buffer) in agents.iter_mut() {
        crate::tick_scope::enter(entity);
        if state.status != AgentStatus::Active || progress.stuck_fired {
            continue; // paused/waiting, or this stage already used its escape
        }
        let stage = &bp.0.stages[cursor.index];
        let Some(cfg) =
            find_conditioned_edge_ref(&bp.0, stage, &visits.0, TransitionCondition::Stuck)
                .and_then(|(_, edge)| edge.stuck)
        else {
            continue; // no stuck edge here, or its escape hatch is spent
        };
        // Lazy stamp: one place covers spawn, `enter_stage`, `force_transition`
        // and snapshot restore, and it measures time the agent was actually
        // runnable rather than time spent queued behind other work.
        let started = *progress.stage_started_at.get_or_insert(now);
        let metrics = StuckMetrics {
            iterations: progress.iterations,
            elapsed_secs: (now - started).max(0) as u64,
            tool_calls: progress.total_tool_calls,
            hottest_edit: hottest_edit(&progress.edits_by_path),
        };
        let Some(reason) = detect_stuck(&cfg, &metrics) else {
            continue;
        };
        progress.stuck_fired = true;
        note_stuck(&mut window, &stage.name, &reason);
        if let Some(mut buffer) = buffer {
            buffer
                .logs
                .push((cursor.index, format!("[stuck] {reason}")));
        }
        commands
            .entity(entity)
            .remove::<ReadyToInfer>()
            .insert(ResolveTransition)
            .insert(StageOutcome::Stuck(reason));
    }
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
                0 => StageResolution::Terminal,
                1 if !stage.allow_complete => {
                    let idx = blueprint
                        .stages
                        .iter()
                        .position(|s| s.name == choosable[0].target)
                        .unwrap_or(0);
                    StageResolution::Next(
                        idx,
                        choosable[0].transform.clone(),
                        choosable[0].gate.clone(),
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

/// `requires_children` gate (exclusive, mirrors the fan-out wait): a stage marked
/// `requires_children` may not transition while any of the agent's spawned
/// sub-agents ([`SubAgentChildren`](crate::components::SubAgentChildren)) are
/// still running - the parent is held `Waiting` (`WaitingForChildren`) and
/// resumes (re-inserting `ResolveTransition`, back to `Active`) once every child
/// is terminal.
pub fn gate_requires_children(world: &mut World) {
    crate::tick_scope::clear();
    use crate::components::SubAgentChildren;

    // Hold: transitioning agents whose stage requires children that aren't done.
    // `&AgentState` in the query guarantees the later `.expect()` never fires.
    let mut candidates: Vec<(Entity, Vec<Entity>)> = Vec::new();
    {
        let mut q = world.query_filtered::<(
            Entity,
            &AgentBlueprint,
            &StageCursor,
            &SubAgentChildren,
            &AgentState,
        ), With<ResolveTransition>>();
        for (e, bp, cursor, children, _) in q.iter(world) {
            if bp.0.stages[cursor.index].requires_children {
                candidates.push((e, children.children.clone()));
            }
        }
    }
    for (entity, children) in candidates {
        crate::tick_scope::enter(entity);
        let pending = children.iter().any(|&c| {
            world
                .get::<AgentState>(c)
                .is_some_and(|s| !is_terminal_status(&s.status))
        });
        if pending {
            world
                .entity_mut(entity)
                .remove::<ResolveTransition>()
                .insert(WaitingForChildren);
            world
                .get_mut::<AgentState>(entity)
                .expect("held agent has AgentState")
                .status = AgentStatus::Waiting;
        }
    }

    // Resume: held agents whose children have all finished.
    crate::tick_scope::clear();
    let mut waiting: Vec<(Entity, Vec<Entity>)> = Vec::new();
    {
        let mut q = world.query_filtered::<
            (Entity, Option<&SubAgentChildren>, &AgentState),
            With<WaitingForChildren>,
        >();
        for (e, children, _) in q.iter(world) {
            waiting.push((e, children.map(|c| c.children.clone()).unwrap_or_default()));
        }
    }
    for (entity, children) in waiting {
        crate::tick_scope::enter(entity);
        let all_done = children.iter().all(|&c| {
            world
                .get::<AgentState>(c)
                .is_none_or(|s| is_terminal_status(&s.status))
        });
        if all_done {
            world
                .entity_mut(entity)
                .remove::<WaitingForChildren>()
                .insert(ResolveTransition);
            world
                .get_mut::<AgentState>(entity)
                .expect("waiting agent has AgentState")
                .status = AgentStatus::Active;
        }
    }
}

/// Default re-entry cap for required-region gating: how many times a stage is
/// re-run to populate an empty `required` region before proceeding anyway (with a
/// warning). Overridable per stage via `max_revisits`.
pub(crate) const DEFAULT_REQUIRED_REENTRY_CAP: usize = 3;

/// Counts how many times the current stage has been re-run to satisfy required
/// context regions. Absent ⇒ 0; reset when a new stage is entered.
#[derive(Component, Debug, Clone, Copy)]
pub struct RequiredReentries(pub usize);

/// Required regions (from the stage's effective layout) still empty at stage end,
/// as `(name, optional custom message)`. Empty when the stage has no
/// context-writing tool (gating a stage that can't populate the region would loop
/// pointlessly). Ported from the imperative `unmet_required_regions`.
pub(crate) fn unmet_required_regions(
    blueprint: &leviath_core::Blueprint,
    stage: &leviath_core::Stage,
    window: &ContextWindow,
) -> Vec<(String, Option<String>)> {
    let can_write = stage
        .available_tools
        .iter()
        .any(|t| t == "context_write" || t == "context_append");
    if !can_write {
        return Vec::new();
    }
    let layout = stage
        .context_layout
        .as_ref()
        .unwrap_or(&blueprint.context_layout);
    layout
        .regions
        .iter()
        .filter(|r| r.required)
        // Caller-input regions are validated (and seeded) at spawn, not written
        // by the agent - skip them here so this gate never nags the agent to
        // populate a slot the caller owns.
        .filter(|r| {
            !matches!(
                r.seed,
                Some(leviath_core::layout::RegionSeed::CallerInput { .. })
            )
        })
        .filter(|r| {
            window
                .get_region(&r.name)
                .map(|reg| reg.content.is_empty())
                .unwrap_or(true)
        })
        .map(|r| (r.name.clone(), r.required_message.clone()))
        .collect()
}

/// Inject a `[System]` nudge into the conversation region for each unmet required
/// region, so the stage re-run tells the agent exactly what to populate.
pub(crate) fn inject_required_region_nudges(
    window: &mut ContextWindow,
    unmet: &[(String, Option<String>)],
) {
    for (name, msg) in unmet {
        let text = msg.clone().unwrap_or_else(|| {
            format!(
                "Required context region '{name}' is still empty. You must populate it \
                 (e.g. via context_write with region=\"{name}\") before this stage can complete."
            )
        });
        let content = format!("[System] {text}");
        let tokens = leviath_core::estimate_tokens(&content);
        let _ = window.add_to_region("conversation", content, tokens);
    }
}

/// Required-region gate: before a normally-completed stage transitions, if it can
/// write context and a `required` region is still empty, inject a nudge and re-run
/// the stage (loop back to `ReadyToInfer`) instead of transitioning - bounded by
/// the stage's `max_revisits` (or a default cap), after which
/// it proceeds with a warning. Skipped when the stage ended on an error / max-iter
/// outcome (those transitions take precedence). Ported from the imperative gate.
#[allow(clippy::type_complexity)]
pub fn require_context_regions(
    mut agents: Query<
        (
            Entity,
            &AgentBlueprint,
            &StageCursor,
            &mut ContextWindow,
            Option<&RequiredReentries>,
            Option<&StageOutcome>,
        ),
        With<ResolveTransition>,
    >,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    for (entity, bp, cursor, mut window, reentries, outcome) in agents.iter_mut() {
        crate::tick_scope::enter(entity);
        if outcome.is_some() {
            continue; // error / max-iterations transition takes precedence
        }
        let stage = &bp.0.stages[cursor.index];
        let unmet = unmet_required_regions(&bp.0, stage, &window);
        if unmet.is_empty() {
            continue;
        }
        let cap = stage.max_revisits.unwrap_or(DEFAULT_REQUIRED_REENTRY_CAP);
        let round = reentries.map_or(0, |r| r.0);
        if round >= cap {
            let names: Vec<&str> = unmet.iter().map(|(n, _)| n.as_str()).collect();
            tracing::warn!(
                stage = %stage.name,
                regions = ?names,
                attempts = cap,
                "required context regions still empty after re-run attempts; proceeding"
            );
            continue; // proceed with the transition despite the unmet regions
        }
        inject_required_region_nudges(&mut window, &unmet);
        commands
            .entity(entity)
            .remove::<ResolveTransition>()
            .insert(ReadyToInfer)
            .insert(RequiredReentries(round + 1));
    }
}

/// What a chosen edge's gate says about the transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GateDecision {
    /// The gate is satisfied (or absent) - follow the edge.
    Pass,
    /// The gate is unsatisfied but out of re-run budget - follow the edge and
    /// record it in the run's flags so the run explains itself afterwards.
    Forced,
    /// Hold the agent in this stage and show it this nudge.
    Block(String),
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
    let cap = gate
        .max_attempts
        .unwrap_or(leviath_core::blueprint::DEFAULT_GATE_ATTEMPTS);
    if progress.gate_reentries >= cap {
        tracing::warn!(
            stage = %stage.name,
            attempts = cap,
            "stage still has no file modifications after re-run attempts; proceeding"
        );
        return GateDecision::Forced;
    }
    GateDecision::Block(gate.message.clone().unwrap_or_else(|| {
        "No file modifications were recorded in this stage. Changes made through the shell \
         (sed -i, tee, >, >>) are not tracked by the framework. Re-apply your changes with \
         edit_file or write_file before moving on."
            .to_string()
    }))
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
    let content = format!("[System] {nudge}");
    let tokens = leviath_core::estimate_tokens(&content);
    let _ = window.add_to_region("conversation", content, tokens);
    progress.gate_reentries += 1;
    commands
        .entity(entity)
        .remove::<ResolveTransition>()
        .remove::<AwaitingTransitionResponse>()
        .remove::<StageOutcome>()
        .insert(ReadyToInfer);
}

/// Transition-resolution system: for each `ResolveTransition` agent, resolve the
/// next stage. Terminal ⇒ mark the agent `Complete`. A single/linear target ⇒
/// enter the new stage (swap its `StageInference`, reset stage progress, bump the
/// visit count) and loop to `ReadyToInfer`. Multiple candidate edges ⇒ hand off
/// to the async transition-choice system via `AwaitingTransitionChoice`.
#[allow(clippy::type_complexity)]
pub fn resolve_transition(
    mut agents: Query<
        (
            Entity,
            &AgentBlueprint,
            &mut StageCursor,
            &mut AgentState,
            &mut StageProgress,
            &StageInferences,
            &StageSetups,
            &mut VisitCounts,
            &mut ContextWindow,
            Option<&StageOutcome>,
            Option<&mut crate::persistence::RunOutcomeFlags>,
        ),
        With<ResolveTransition>,
    >,
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
    ) in agents.iter_mut()
    {
        crate::tick_scope::enter(entity);
        let stage = &bp.0.stages[cursor.index];
        // How the stage ended governs the transition: an error/max-iterations
        // outcome follows its conditioned edge (e.g. → error_recovery) if present.
        let resolution = match outcome {
            // An error/max-iterations edge is never gated: the stage already
            // failed, and holding it back to demand file changes would strand a
            // run that can't make any.
            Some(StageOutcome::Errored(_)) => {
                find_conditioned_edge(&bp.0, stage, &visits.0, TransitionCondition::Error)
                    .map(|(i, t)| StageResolution::Next(i, t, None))
                    .unwrap_or(StageResolution::TerminalError)
            }
            Some(StageOutcome::MaxIterations) => {
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
        match resolution {
            StageResolution::Terminal => {
                state.status = AgentStatus::Complete;
                commands
                    .entity(entity)
                    .remove::<ResolveTransition>()
                    .remove::<StageOutcome>();
            }
            StageResolution::TerminalError => {
                // Status was set to Error by the collect system; just stop.
                commands
                    .entity(entity)
                    .remove::<ResolveTransition>()
                    .remove::<StageOutcome>();
            }
            StageResolution::Next(idx, transform, gate) => {
                // Check the edge's gate BEFORE the transform runs: the transform
                // compacts/clears regions, and a held stage must keep its context.
                let gate = outcome.is_none().then_some(gate).flatten();
                match gate_blocks(gate.as_ref(), stage, &progress, &window) {
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
                match enter_stage(
                    idx,
                    &bp.0,
                    &mut cursor,
                    &mut state,
                    &mut progress,
                    &mut visits,
                    setup,
                    &mut window,
                ) {
                    Ok(()) => {
                        // Entering a stage is active work; clears a prior error
                        // status when recovering down an `error` edge.
                        state.status = AgentStatus::Active;
                        let name = bp.0.stages[idx].name.clone();
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
#[allow(clippy::too_many_arguments)]
pub(crate) fn enter_stage(
    idx: usize,
    blueprint: &leviath_core::Blueprint,
    cursor: &mut StageCursor,
    state: &mut AgentState,
    progress: &mut StageProgress,
    visits: &mut VisitCounts,
    setup: &StageSetup,
    window: &mut ContextWindow,
) -> Result<(), String> {
    cursor.index = idx;
    let name = blueprint.stages[idx].name.clone();
    state.current_stage = name.clone();
    state.accepts_messages = setup.accepts_messages;
    *progress = StageProgress::default();
    *visits.0.entry(name).or_insert(0) += 1;

    apply_stage_context(setup, window)
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
        // A fresh stage re-arms its interaction points + required-region gate.
        .remove::<crate::interaction_points::InteractionPointCursor>()
        .remove::<crate::interaction_points::InteractionPointRounds>()
        .remove::<RequiredReentries>()
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
pub fn force_transition(world: &mut World, entity: Entity, target_idx: usize) {
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
            &mut cursor,
            &mut state,
            &mut progress,
            &mut visits,
            &setup,
            &mut window,
        ) {
            Ok(()) => Some((stage_inf, setup, name)),
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

/// A blueprint stage resolved to a concrete provider, model, and effective tool
/// set - the per-stage input to [`spawn_agent`]. The caller (CLI / daemon) owns
/// the model-selection policy (overrides, availability, user defaults) and tool
/// filtering; the runtime just turns the result into agent data.
pub struct ResolvedStage {
    /// The provider to call for this stage.
    pub provider_name: String,
    /// The resolved model name.
    pub model: String,
    /// The effective tool set for this stage (already filtered).
    pub tools: Vec<Tool>,
}

/// Fallback context window used when a stage's provider isn't registered (so
/// percentage budgets can't be resolved against a real model). Matches
/// [`leviath_providers::ModelCapabilities`]'s default `max_context_tokens`.
pub(crate) const DEFAULT_CONTEXT_WINDOW_TOKENS: usize = 8192;

/// Look up a model's context window (for resolving percentage region budgets)
/// via the registered [`Providers`]. Falls back to
/// [`DEFAULT_CONTEXT_WINDOW_TOKENS`] with a warning when the provider isn't
/// registered - non-fatal, and `min_tokens` floors still protect regions.
pub(crate) fn context_window_tokens(world: &World, provider_name: &str, model: &str) -> usize {
    match world
        .get_resource::<Providers>()
        .and_then(|p| p.0.get(provider_name))
    {
        Some(provider) => provider.max_context_tokens(model),
        None => {
            tracing::warn!(
                provider = provider_name,
                model,
                "provider not registered; using default context window for percentage budgets"
            );
            DEFAULT_CONTEXT_WINDOW_TOKENS
        }
    }
}

/// Build a stage's [`StageSetup`] from its blueprint definition: inference config
/// (from the model parameters), tool-result routing, accepts-messages, layout,
/// and system prompt.
pub(crate) fn stage_setup_from(
    stage: &leviath_core::Stage,
    global_batch_tool_hint: bool,
    agent_batch_tool_hint: Option<bool>,
) -> StageSetup {
    let temperature = stage
        .model
        .parameters
        .get("temperature")
        .and_then(|v| v.as_f64())
        .map(|t| t as f32);
    // Every other model parameter (top_p, stop, seed, frequency_penalty, …) is
    // passed through to the provider verbatim; only temperature/max_output_tokens
    // are consumed specially above.
    let extra_params: serde_json::Map<String, serde_json::Value> = stage
        .model
        .parameters
        .iter()
        .filter(|(k, _)| k.as_str() != "temperature" && k.as_str() != "max_output_tokens")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let max_output_tokens = stage
        .model
        .parameters
        .get("max_output_tokens")
        .and_then(|v| v.as_u64())
        .map(|t| t as usize);
    let base_prompt = stage
        .config
        .get("system_prompt")
        .and_then(|v| v.as_str())
        .map(String::from);
    // A fan-out stage's single inference IS the "split": fold its `split_prompt`
    // (which asks for the JSON array of work items) onto any base instructions so
    // the stage's normal inference produces the work items the split system parses.
    let system_prompt = match &stage.mode {
        leviath_core::blueprint::StageMode::FanOut { config }
            if !config.split_prompt.trim().is_empty() =>
        {
            Some(match base_prompt {
                Some(base) => format!("{base}\n\n{}", config.split_prompt),
                None => config.split_prompt.clone(),
            })
        }
        _ => base_prompt,
    };
    // Cascade the batch-tool-hint toggle: stage > agent > global (default on).
    let batch_tool_hint = leviath_core::taint::resolve_batch_tool_hint(
        global_batch_tool_hint,
        agent_batch_tool_hint,
        stage.batch_tool_hint,
    );
    StageSetup {
        inference_config: InferenceConfig {
            temperature,
            max_output_tokens,
            extra_params,
            batch_tool_hint,
            request_timeout_secs: stage.model.request_timeout_secs,
        },
        routing: stage.tool_result_routing.clone(),
        accepts_messages: stage.accepts_messages,
        context_layout: stage.context_layout.clone(),
        system_prompt,
    }
}

/// Spawn a fully-formed agent into `world` from its blueprint, task, and
/// per-stage resolution, and return its entity. Builds every stage's
/// `StageInference`/`StageSetup` up front (so transitions are pure component
/// swaps), seeds the context window, applies the **first** stage's setup (its
/// layout and system prompt), pre-counts the first stage's visit, and marks the
/// agent `ReadyToInfer`. Returns `Err` if the first stage's system prompt doesn't fit
/// its region (the same hard failure the imperative loop raises at stage 0).
///
/// `stages` must be aligned with `blueprint.stages` (one [`ResolvedStage`] each).
///
/// `global_batch_tool_hint` is the caller's global config toggle for the
/// batch-tool-calls system-prompt hint; it is resolved per stage against the
/// blueprint's agent-level and each stage's `batch_tool_hint`.
pub fn spawn_agent(
    world: &mut World,
    agent_id: String,
    blueprint: leviath_core::Blueprint,
    task: &str,
    stages: Vec<ResolvedStage>,
    global_batch_tool_hint: bool,
) -> Result<Entity, String> {
    let seeds = std::collections::HashMap::from([("task".to_string(), task.to_string())]);
    // No compiled custom-region scripts on this path: script-backed regions
    // require the seeded spawn (the CLI resolves and compiles them). A custom
    // region spawned through here renders its fallback shape.
    spawn_agent_seeded(
        world,
        agent_id,
        blueprint,
        &seeds,
        stages,
        global_batch_tool_hint,
        std::collections::HashMap::new(),
    )
}

/// Like [`spawn_agent`], but seeds the context window from a name→content map
/// (caller-input regions filled by the CLI/ACP/API, plus blueprint-resolved
/// seeds) rather than a single task string. `spawn_agent` is the thin wrapper
/// that seeds only the `task` key.
pub fn spawn_agent_seeded(
    world: &mut World,
    agent_id: String,
    mut blueprint: leviath_core::Blueprint,
    seeds: &std::collections::HashMap<String, String>,
    stages: Vec<ResolvedStage>,
    global_batch_tool_hint: bool,
    region_scripts: std::collections::HashMap<
        String,
        std::sync::Arc<leviath_scripting::region_hook::RegionScript>,
    >,
) -> Result<Entity, String> {
    // Resolve any percentage region budgets against each stage's model context
    // window (the only place the model - and hence the window - is known). The
    // global layout resolves against the entry stage (stage 0); each per-stage
    // layout resolves against that stage's own model. Absolute layouts resolve to
    // themselves, so this is a no-op for legacy blueprints.
    let stage_windows: Vec<usize> = stages
        .iter()
        .map(|rs| context_window_tokens(world, &rs.provider_name, &rs.model))
        .collect();
    blueprint.context_layout = blueprint.context_layout.resolved(stage_windows[0]);
    for (i, stage) in blueprint.stages.iter_mut().enumerate() {
        if let Some(layout) = &stage.context_layout {
            stage.context_layout = Some(layout.resolved(stage_windows[i]));
        }
    }
    // Validate the resolved (fully-absolute) layouts, now that percentages are
    // concrete numbers judged against the real model window.
    blueprint
        .context_layout
        .validate()
        .map_err(|e| e.to_string())?;
    for stage in &blueprint.stages {
        if let Some(layout) = &stage.context_layout {
            layout.validate().map_err(|e| e.to_string())?;
        }
    }

    let stage_infs: Vec<StageInference> = stages
        .into_iter()
        .map(|rs| StageInference {
            provider_name: rs.provider_name,
            model: rs.model,
            tools: rs.tools,
            tool_filter: None, // tools already resolved to the effective set
        })
        .collect();
    let agent_batch_tool_hint = blueprint.batch_tool_hint;
    let setups: Vec<StageSetup> = blueprint
        .stages
        .iter()
        .map(|s| stage_setup_from(s, global_batch_tool_hint, agent_batch_tool_hint))
        .collect();

    // Seed the window from the blueprint layout + task, then apply stage 0's
    // context setup (layout swap + system-prompt injection) just as entering any
    // later stage would.
    let interner = world
        .get_resource::<crate::ContentInternerRes>()
        .map(|r| r.handle())
        .unwrap_or_default();
    let mut window =
        ContextWindow::with_interner(blueprint.context_layout.total_budget_tokens, interner);
    // Attach compiled custom-region scripts BEFORE seeding, so seed writes
    // pass through each region's on_write hook like any other entry.
    window.region_scripts = region_scripts;
    crate::context_setup::init_window_seeded(&mut window, &blueprint, seeds);
    apply_stage_context(&setups[0], &mut window)?;

    let stage0_name = blueprint.stages[0].name.clone();
    let stage0_inf = stage_infs[0].clone();
    let setup0 = &setups[0];
    let stage0_cfg = setup0.inference_config.clone();
    let stage0_routing = setup0.routing.clone();
    let accepts_messages = setup0.accepts_messages;

    // Pre-count stage 0's visit: the imperative loop bumps a stage's visit after
    // it runs and before resolving its transition, so stage 0 must read as
    // visited once by the time its first transition resolves.
    let mut visits = VisitCounts::default();
    *visits.0.entry(stage0_name.clone()).or_insert(0) += 1;

    // Seed the per-stage ledger (names + Pending) so the dashboard shows every
    // stage's real name from the first persist, not just the active one.
    let ledger = StageLedger(
        blueprint
            .stages
            .iter()
            .enumerate()
            .map(|(i, s)| leviath_core::run_meta::StageRecord::new(s.name.clone(), i))
            .collect(),
    );

    // Repetition detection is opt-in per blueprint.
    let repetition = blueprint
        .repetition_detection
        .as_ref()
        .map(crate::repetition::RepetitionDetector::from_detection_config);

    let entity = world
        .spawn((
            AgentBlueprint(blueprint),
            AgentState {
                agent_id,
                current_stage: stage0_name,
                iteration: 0,
                status: AgentStatus::Active,
                spawned_children_ids: vec![],
                pending_wait: None,
                accepts_messages,
            },
            MessageInbox::default(),
            StageCursor { index: 0 },
            StageProgress::default(),
            StageInferences(stage_infs),
            StageSetups(setups),
            visits,
            window,
            stage0_inf,
            stage0_cfg,
            ReadyToInfer,
        ))
        .id();
    // Inserted after spawn: the bundle above is already at bevy's 15-tuple limit.
    world
        .entity_mut(entity)
        .insert((ledger, StageIoBuffer::default()));
    if let Some(detector) = repetition {
        world.entity_mut(entity).insert(detector);
    }
    if let Some(routing) = stage0_routing {
        world
            .entity_mut(entity)
            .insert(crate::components::ToolResultRoutingComponent { routing });
    }
    Ok(entity)
}

/// A transition-choice inference is in flight (an LLM is picking the next stage);
/// holds the choosable edges so the collect system can match the response back to
/// one. (Ported from the async portion of `graph::prompt_llm_transition`.)
#[derive(Component, Debug, Clone)]
pub struct AwaitingTransitionResponse(pub Vec<leviath_core::blueprint::TransitionEdge>);

/// The receiving end of the transition-choice outcomes channel, as a world
/// resource for the collect system. (The sending end lives in
/// [`InferenceStage::transition_outcomes`].)
#[derive(Resource)]
pub struct TransitionResults(pub UnboundedReceiver<InferenceOutcome>);

/// Build the LLM prompt that asks which stage to run next. (Ported from the
/// prompt-building portion of `graph::prompt_llm_transition`.)
pub(crate) fn build_transition_prompt(
    stage: &leviath_core::Stage,
    edges: &[leviath_core::blueprint::TransitionEdge],
) -> String {
    let mut p = match &stage.transition_prompt {
        Some(custom) => {
            let mut p = custom.clone();
            p.push_str("\n\nAvailable transitions:\n");
            p
        }
        None => format!(
            "Stage '{}' is complete. Available next stages:\n",
            stage.name
        ),
    };
    for edge in edges {
        p.push_str(&format!("- {}", edge.target));
        if let Some(hint) = &edge.hint {
            p.push_str(&format!(": {hint}"));
        }
        p.push('\n');
    }
    if stage.transition_prompt.is_some() {
        if stage.allow_complete {
            p.push_str(
                "\nRespond with ONLY the stage name you want to transition to, or ONLY the \
                 word DONE if no further stage is needed and the run should end here.",
            );
        } else {
            p.push_str(
                "\nRespond with ONLY the stage name you want to transition to, nothing else.",
            );
        }
    } else if stage.allow_complete {
        p.push_str(
            "\nWhich stage should run next? Respond with ONLY the stage name, or ONLY the \
             word DONE if no further stage is needed and the run should end here.",
        );
    } else {
        p.push_str("\nWhich stage should run next? Respond with ONLY the stage name.");
    }
    p
}

/// Match an LLM transition response to one of the choosable edges' target stages,
/// or `None` if the stage may complete and the LLM chose to end here.
///
/// Models are asked to answer with only the target stage name (or `DONE`), but
/// frequently wrap it in prose or re-explain the stage. We therefore look for a
/// clean, standalone decision - scanning the first line, then the concluding
/// line, for a **whole-word** match against a stage name or `DONE` - instead of
/// substring-scanning the whole response, where a stage name mentioned in
/// passing ("the implementation", "the approved plan") would hijack the routing.
/// When nothing matches, a stage that may complete ends the run; otherwise the
/// run advances along the first declared edge.
pub(crate) fn match_transition_choice(
    choice: &str,
    edges: &[leviath_core::blueprint::TransitionEdge],
    allow_complete: bool,
) -> Option<String> {
    let lines: Vec<&str> = choice
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    // Candidate decision lines, in priority order: the first line (the model was
    // told to reply with only the name, so the answer leads), then - only if it
    // is short and answer-like (≤ 3 words) - the concluding line, which catches
    // models that reason first and answer last without matching a stage name
    // buried in a prose summary ("the approved plan was implemented").
    let words_in = |line: &str| {
        line.split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|w| !w.is_empty())
            .count()
    };
    let first = lines.first().copied();
    let last = lines
        .last()
        .copied()
        .filter(|l| lines.len() > 1 && words_in(l) <= 3);
    for line in first.into_iter().chain(last) {
        for word in line.split(|c: char| !c.is_alphanumeric() && c != '_') {
            if word.is_empty() {
                continue;
            }
            if allow_complete && word.eq_ignore_ascii_case("done") {
                return None;
            }
            if let Some(edge) = edges.iter().find(|e| word.eq_ignore_ascii_case(&e.target)) {
                return Some(edge.target.clone());
            }
        }
    }
    // No clear decision: a stage that may end prefers ending over looping back;
    // otherwise the run advances along the first declared edge.
    if allow_complete {
        None
    } else {
        edges.first().map(|edge| edge.target.clone())
    }
}

/// Transition-choice dispatch: for each `AwaitingTransitionChoice` agent, inject
/// the "which stage next?" prompt into its context, build a short deterministic
/// request, acquire a per-model permit, spawn the inference onto the transition
/// lane, and move it to `AwaitingTransitionResponse`. Provider-missing / pool-full
/// leaves it choosing and retries next tick (same backpressure as
/// [`dispatch_inference`]).
#[allow(clippy::type_complexity)]
pub fn dispatch_transition_choice(
    mut agents: Query<
        (
            Entity,
            &AgentState,
            &mut ContextWindow,
            &StageInference,
            &AgentBlueprint,
            &StageCursor,
            &AwaitingTransitionChoice,
            Option<&InFlightWork>,
        ),
        With<AwaitingTransitionChoice>,
    >,
    stage: Res<InferenceStage>,
    providers: Res<Providers>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    for (entity, state, mut window, si, bp, cursor, choice, in_flight) in agents.iter_mut() {
        crate::tick_scope::enter(entity);
        if state.status != AgentStatus::Active {
            continue; // paused / waiting / cancelled - don't start new work
        }
        let Some(provider) = providers.0.get(&si.provider_name) else {
            continue; // provider not registered - retry later
        };
        let Some(permit) = stage.pools.try_acquire(&si.model) else {
            continue; // pool full - retry next tick
        };

        let current = &bp.0.stages[cursor.index];
        let prompt = build_transition_prompt(current, &choice.0);
        let tokens = leviath_core::estimate_tokens(&prompt);
        let _ = window.add_typed_entry(
            "conversation",
            leviath_core::EntryKind::UserMessage,
            prompt,
            tokens,
        );

        // Plain `assemble()` (default meta): this is the deterministic
        // 256-token routing call, not stage inference - custom regions still
        // render (they may hold the whole context), just with empty stage
        // fields in their ctx.
        let assembled = window.assemble();
        let remaining = window.max_tokens.saturating_sub(window.current_tokens);
        let request = InferenceRequest {
            system: assembled.system_blocks,
            messages: assembled.messages,
            model: si.model.clone(),
            max_tokens: remaining.min(256), // short routing response
            temperature: 0.0,               // deterministic routing
            tools: Vec::new(),
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let job = InferenceJob {
            entity,
            provider,
            request,
            permit,
            // Routing responses are tiny (≤256 tokens) and always fit; skip the
            // extra count call for them.
            exact_token_counting: false,
        };
        let cancel = crate::cancel::CancelToken::new();
        stage.runtime.spawn(run_inference_job(
            job,
            stage.transition_outcomes.clone(),
            stage.wake.clone(),
            crate::inference_bridge::RetryPolicy::default(),
            cancel.clone(),
        ));
        track_in_flight(&mut commands, entity, in_flight, cancel);
        commands
            .entity(entity)
            .remove::<AwaitingTransitionChoice>()
            .insert(AwaitingTransitionResponse(choice.0.clone()));
    }
}

/// Transition-choice collect: drain completed routing inferences, match each to a
/// target stage (or completion), record the decision in context, and either enter
/// the chosen stage (loop to `ReadyToInfer`) or mark the agent `Complete`. A
/// provider error marks the agent `Error`.
#[allow(clippy::type_complexity)]
pub fn collect_transition_choice(
    mut results: ResMut<TransitionResults>,
    mut agents: Query<(
        &AgentBlueprint,
        &mut StageCursor,
        &mut AgentState,
        &mut StageProgress,
        &StageInferences,
        &StageSetups,
        &mut VisitCounts,
        &mut ContextWindow,
        &AwaitingTransitionResponse,
        Option<&mut crate::persistence::RunOutcomeFlags>,
    )>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    while let Ok(outcome) = results.0.try_recv() {
        let Ok((
            bp,
            mut cursor,
            mut state,
            mut progress,
            stage_infs,
            setups,
            mut visits,
            mut window,
            resp,
            mut flags,
        )) = agents.get_mut(outcome.entity)
        else {
            continue; // stale: agent cancelled/despawned since dispatch
        };
        crate::tick_scope::enter(outcome.entity);
        // Cancelled/failed mid-choice: every arm below rewrites the status
        // (including a bare `Complete` when nothing matches), which would report
        // a cancelled run as having finished normally.
        if is_terminal_status(&state.status) {
            commands
                .entity(outcome.entity)
                .remove::<AwaitingTransitionResponse>()
                .remove::<InFlightWork>();
            continue;
        }
        let response = match outcome.result {
            Ok(response) => response,
            Err(err) => {
                state.status = AgentStatus::Error {
                    message: err.to_string(),
                };
                commands
                    .entity(outcome.entity)
                    .remove::<AwaitingTransitionResponse>();
                continue;
            }
        };

        let choice = response.content.trim().to_string();
        let tokens = leviath_core::estimate_tokens(&choice);
        let _ = window.add_typed_entry(
            "conversation",
            leviath_core::EntryKind::AssistantTurn { tool_calls: vec![] },
            format!("Transitioning to: {choice}"),
            tokens,
        );

        let allow_complete = bp.0.stages[cursor.index].allow_complete;
        match match_transition_choice(&choice, &resp.0, allow_complete) {
            Some(target) => {
                let idx =
                    bp.0.stages
                        .iter()
                        .position(|s| s.name == target)
                        .unwrap_or(0);
                // The chosen edge (absent when the matched target has no explicit
                // edge, e.g. a fallback - then Direct, ungated).
                let edge = resp.0.iter().find(|e| e.target == target);
                let transform = edge.map(|e| e.transform.clone()).unwrap_or_default();
                // The edge's gate is checked BEFORE its transform runs, so a
                // held stage keeps the context it still needs.
                let stage = &bp.0.stages[cursor.index];
                match gate_blocks(
                    edge.and_then(|e| e.gate.as_ref()),
                    stage,
                    &progress,
                    &window,
                ) {
                    GateDecision::Block(nudge) => {
                        hold_for_gate(
                            outcome.entity,
                            &nudge,
                            &mut progress,
                            &mut window,
                            &mut commands,
                        );
                        continue;
                    }
                    GateDecision::Forced => {
                        if let Some(flags) = flags.as_mut() {
                            flags.0.gates_forced += 1;
                        }
                    }
                    GateDecision::Pass => {}
                }
                let to_compact = apply_edge_transform(&mut window, &transform);
                let setup = &setups.0[idx];
                match enter_stage(
                    idx,
                    &bp.0,
                    &mut cursor,
                    &mut state,
                    &mut progress,
                    &mut visits,
                    setup,
                    &mut window,
                ) {
                    Ok(()) => {
                        let name = bp.0.stages[idx].name.clone();
                        let mut ec = commands.entity(outcome.entity);
                        ec.remove::<AwaitingTransitionResponse>();
                        attach_stage_components(ec, stage_infs.0[idx].clone(), setup, idx, name);
                        if !to_compact.is_empty() {
                            commands
                                .entity(outcome.entity)
                                .insert(PendingEdgeCompact(to_compact));
                        }
                    }
                    Err(message) => {
                        state.status = AgentStatus::Error { message };
                        commands
                            .entity(outcome.entity)
                            .remove::<AwaitingTransitionResponse>();
                    }
                }
            }
            None => {
                state.status = AgentStatus::Complete;
                commands
                    .entity(outcome.entity)
                    .remove::<AwaitingTransitionResponse>();
            }
        }
    }
}

/// Notify the [`ToolService`] of every agent that just entered a stage (tagged
/// with [`StageJustEntered`] by the transition systems), so it can re-sync that
/// agent's per-stage tool permissions, then clear the tag. Runs after the
/// transition systems each tick.
pub fn sync_tool_stages(
    service: Res<ToolServiceRes>,
    entered: Query<(Entity, &StageJustEntered)>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    for (entity, stage) in entered.iter() {
        crate::tick_scope::enter(entity);
        service.0.sync_stage(entity, stage.index, &stage.name);
        commands.entity(entity).remove::<StageJustEntered>();
    }
}

/// Re-advertise an agent's tools mid-run: when tagged [`ToolsNeedRefresh`], ask
/// the tool service for this stage's freshly-resolved tool defs and, if it
/// returns a set, write it into the live [`StageInference`] (what the next
/// inference request advertises, read fresh by `build_request`) and the matching
/// [`StageInferences`] catalog entry (so a later revisit of this stage keeps the
/// updated set). Always consumes the marker. This is the mechanism behind
/// mid-run dynamic tool discovery and lazily-listed MCP tools.
pub fn refresh_advertised_tools(
    service: Res<ToolServiceRes>,
    mut agents: Query<
        (
            Entity,
            &StageCursor,
            &mut StageInference,
            &mut StageInferences,
        ),
        With<ToolsNeedRefresh>,
    >,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    for (entity, cursor, mut si, mut sis) in agents.iter_mut() {
        crate::tick_scope::enter(entity);
        if let Some(tools) = service.0.refresh_tools(entity, cursor.index) {
            si.tools = tools.clone();
            // Keep the catalog entry in sync so re-entering this stage advertises
            // the same refreshed set.
            if let Some(slot) = sis.0.get_mut(cursor.index) {
                slot.tools = tools;
            }
        }
        commands.entity(entity).remove::<ToolsNeedRefresh>();
    }
}

/// Poll each `dynamic_tools` agent for a pending tool re-scan and, when the tool
/// service reports one, tag it [`ToolsNeedRefresh`] so [`refresh_advertised_tools`]
/// re-advertises before its next turn. Only agents carrying [`DynamicTools`] are
/// queried, so static agents (the default) cost nothing.
pub fn poll_dynamic_tool_refresh(
    service: Res<ToolServiceRes>,
    agents: Query<Entity, With<DynamicTools>>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    for entity in agents.iter() {
        crate::tick_scope::enter(entity);
        if service.0.wants_refresh(entity) {
            commands.entity(entity).insert(ToolsNeedRefresh);
        }
    }
}
