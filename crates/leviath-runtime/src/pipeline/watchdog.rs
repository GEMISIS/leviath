//! Watchdogs that end a run the stage graph would otherwise let spin:
//! a workspace that disappeared, an iteration cap, and stuck detection -
//! plus the context notes each writes so the reason survives into the run's
//! transcript rather than only its status.

use super::*;

/// How often (in per-stage iterations) [`check_workspace_health`] stats the
/// agent's working directory. One `metadata` call every few iterations is far
/// cheaper than the tool failures it replaces.
pub const WORKSPACE_CHECK_INTERVAL: usize = 5;

/// What `check_workspace_health` selects.
///
/// `&'static` is bevy's `WorldQuery` convention, not a claim about
/// lifetimes: the borrow is bound when the query is fetched.
type WorkspaceHealthQuery = (
    Entity,
    &'static RunMetadata,
    &'static StageProgress,
    &'static mut AgentState,
    Option<&'static mut crate::persistence::RunOutcomeFlags>,
);

/// Workspace health guard: fail a run whose working directory has disappeared.
///
/// The motivating failure: an external harness deleted the workspace out from
/// under running agents, which then spent every remaining iteration collecting
/// `No such file or directory` from their tools - 16-17 of them in the observed
/// runs - with no way back. Nothing can recreate a deleted checkout from inside
/// the agent, so this stops immediately with a message that names the real
/// problem, instead of routing to error recovery to flail more cheaply.
pub fn check_workspace_health(
    mut agents: Query<WorkspaceHealthQuery, With<ReadyToInfer>>,
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
        if let Some(mut flags) = flags {
            flags.0.workspace_lost = true;
        }
        commands.entity(entity).remove::<ReadyToInfer>();
        // Through the stage's transition rather than straight to a dead run: an
        // `error_recovery` stage usually works in context alone, so it can still
        // say what happened even with the directory gone.
        crate::pipeline::fail_stage(
            &mut commands,
            entity,
            &mut state,
            format!("workspace '{}' is no longer accessible", md.workdir),
        );
    }
}

/// What `enforce_max_iterations` selects.
///
/// `&'static` is bevy's `WorldQuery` convention, not a claim about
/// lifetimes: the borrow is bound when the query is fetched.
type MaxIterationQuery = (
    Entity,
    &'static AgentState,
    &'static AgentBlueprint,
    &'static StageCursor,
    &'static StageProgress,
    Option<&'static mut crate::persistence::RunOutcomeFlags>,
);

/// Max-iterations guard: for each `ReadyToInfer` agent whose per-stage inference
/// count has reached the stage's `max_iterations`, end the stage (routing to a
/// `max_iterations` edge if one exists, else a normal transition) instead of
/// running another inference. Ported from the imperative `run_autonomous` cap.
pub fn enforce_max_iterations(
    mut agents: Query<MaxIterationQuery, With<ReadyToInfer>>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    for (entity, state, bp, cursor, progress, flags) in agents.iter_mut() {
        crate::tick_scope::enter(entity);
        if state.status != AgentStatus::Active {
            continue;
        }
        let stage = &bp.0.stages[cursor.index];
        // A fan-out stage is bounded by `max_attempts`, not by iterations. Its
        // "iterations" are the framework asking again for the one call the stage
        // exists to make, and letting the iteration cap count them means two
        // budgets bound the same loop - with the cap winning, because it fires
        // first and ends the stage.
        //
        // That is not hypothetical. `deep-researcher` allows `investigate` four
        // iterations; a live run spent three answering in prose and called
        // `fan_out` on the fourth, and the stage was already at its cap when the
        // three workers came back, so thirteen minutes of finished research was
        // discarded. `lev validate` has always held that a fan_out stage needs no
        // `max_iterations` (see the lint's `counts_iterations`); the runtime was
        // enforcing one anyway.
        if matches!(
            stage.mode,
            leviath_core::blueprint::StageMode::FanOut { .. }
        ) {
            continue;
        }
        let max = stage.max_iterations.unwrap_or(0);
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

/// The context region an abnormal-ending note (inference error, iteration cap)
/// is written to when the blueprint declares one. Pinned by convention, like
/// [`STUCK_REPORT_REGION`], so the note survives the edge transform into the
/// stage that has to act on it.
pub(crate) const ERROR_REPORT_REGION: &str = "error_report";

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

/// Write an abnormal-ending note where the next stage will read it: the
/// blueprint's `error_report` region when it declares one, else `conversation`.
/// Best-effort, like [`note_stuck`] - an overflowing region silently drops it.
fn note_abnormal_ending(window: &mut ContextWindow, content: String) {
    let region = if window.get_region(ERROR_REPORT_REGION).is_some() {
        ERROR_REPORT_REGION
    } else {
        "conversation"
    };
    let tokens = leviath_core::estimate_tokens(&content);
    let _ = window.add_to_region(region, content, tokens);
}

/// Write the inference error that ended a stage into context, so the recovery
/// stage an `error` edge routes to starts out knowing what failed instead of
/// being told to diagnose an error it cannot see.
pub(crate) fn note_error(window: &mut ContextWindow, stage: &str, message: &str) {
    note_abnormal_ending(
        window,
        format!(
            "[Inference error in stage '{stage}'] {message}. Diagnose this failure from \
             the error text above before retrying or working around it."
        ),
    );
}

/// Write a note saying a fan-out started no workers, so whatever runs next knows
/// there are no sub-findings coming.
///
/// Distinct from [`note_error`] because nothing failed to reach a provider and
/// the advice differs: the stage simply never handed any work out, and the merge
/// that follows has to work from what is already in context.
///
/// Assembled from parts rather than one long literal: rustfmt collapses a
/// `\`-continued string onto one line when it fits, and bakes the continuation's
/// indentation into the text. The run that proved this read
/// "No workers ran,              so there are no sub-findings".
pub(crate) fn note_unusable_split(window: &mut ContextWindow, stage: &str, message: &str) {
    let advice = concat!(
        " No workers ran, so there are no sub-findings from this stage.",
        " Work from what is already in context and say plainly which parts",
        " are unsupported because of it."
    );
    note_abnormal_ending(
        window,
        format!("[Fan-out in stage '{stage}' started no workers] {message}.{advice}"),
    );
}

/// Write an iteration-cap note into context when a stage runs out of
/// iterations, so whatever stage runs next - a `max_iterations` edge target or
/// the normal successor - knows the work was cut off rather than finished.
pub(crate) fn note_max_iterations(window: &mut ContextWindow, stage: &str, cap: usize) {
    note_abnormal_ending(
        window,
        format!(
            "[Stage '{stage}' hit its iteration cap ({cap})] The stage was cut off before \
             it declared completion - treat its output as possibly incomplete and verify \
             it before building on it."
        ),
    );
}

/// What `detect_stuck_stage` selects.
///
/// `&'static` is bevy's `WorldQuery` convention, not a claim about
/// lifetimes: the borrow is bound when the query is fetched.
type StuckStageQuery = (
    Entity,
    &'static AgentState,
    &'static AgentBlueprint,
    &'static StageCursor,
    &'static mut StageProgress,
    &'static VisitCounts,
    &'static mut ContextWindow,
    Option<&'static mut StageIoBuffer>,
);

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
pub fn detect_stuck_stage(
    mut agents: Query<StuckStageQuery, With<ReadyToInfer>>,
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
