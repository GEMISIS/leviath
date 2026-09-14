//! The checks on a `fan_out` stage: where it goes when a worker fails, and
//! whether the workers it runs on this blueprint can be spawned at all.
//!
//! Next to, not inside, the shape checks: a fan-out is the one place a
//! blueprint spawns *itself*, so what it demands of the caller it also demands
//! of its own workers, and that is easy to miss when the checks that read a
//! stage in isolation sit in one file.

use super::*;

/// A `fail_all` fan-out stage with nowhere to go when a worker fails.
///
/// `on_worker_failure = "fail_all"` means one failed worker ends the stage. That
/// is a deliberate choice - a merge that cannot be trusted with a partial set
/// should not run on one - but it only reads as that choice when the blueprint
/// says where to go instead. Without an edge the run simply stops, and a single
/// flaky worker takes the whole thing down.
///
/// The default, `continue`, needs none of this: it merges what succeeded and
/// reports the rest, so there is nothing to escape from.
///
/// A warning rather than an error, because a run that ends loudly on a failed
/// worker is a defensible design, just rarely the intended one.
pub(super) fn lint_fanout_escape(stage: &leviath_core::Stage) -> Vec<LintFinding> {
    let StageMode::FanOut { config } = &stage.mode else {
        return Vec::new();
    };
    if config.on_worker_failure != leviath_core::blueprint::WorkerFailurePolicy::FailAll {
        return Vec::new();
    }
    let escapes = stage
        .transitions
        .iter()
        .flat_map(|t| t.values())
        .any(|edge| {
            matches!(
                edge.condition,
                leviath_core::blueprint::TransitionCondition::Error
                    | leviath_core::blueprint::TransitionCondition::DeadEnd
            )
        });
    if escapes {
        return Vec::new();
    }
    vec![
        LintFinding::new(
            LintSeverity::Warning,
            "fanout-no-escape",
            "sets on_worker_failure = \"fail_all\" but declares no 'error' or 'dead_end' \
             transition, so one failed worker ends the run with nowhere to go"
                .to_string(),
        )
        .in_stage(&stage.name)
        .with_fix(
            "add a transition with condition = \"error\" to a recovery stage, or use the \
             default on_worker_failure = \"continue\""
                .to_string(),
        ),
    ]
}

/// A fan-out whose workers are this blueprint, which cannot take a task.
///
/// A worker is spawned with its work item as the task, the way `lev run --task`
/// hands one in. The spawn refuses a task the blueprint has nowhere to put,
/// because a run that drops its task answers a question nobody asked. That
/// refusal is right for a person and silent for a fan-out: every worker fails
/// the same way, the merge is told to cover for all of them, and the run
/// completes looking like the parallel part happened. It never did.
///
/// Only `worker_stage` fan-outs are checked. `worker_agent` names another
/// blueprint, which may or may not be installed here, and that one is linted
/// when it is validated itself.
pub(super) fn lint_fanout_worker_task(
    blueprint: &Blueprint,
    stage: &leviath_core::Stage,
) -> Vec<LintFinding> {
    let StageMode::FanOut { config } = &stage.mode else {
        return Vec::new();
    };
    let Some(worker) = config.worker_stage.as_deref() else {
        return Vec::new();
    };
    if blueprint.accepts_task() {
        return Vec::new();
    }
    vec![
        LintFinding::new(
            LintSeverity::Error,
            "fanout-worker-task-unheld",
            format!(
                "runs its workers on stage '{worker}' of this blueprint, and each worker \
                 is spawned with its work item as the task, but no region here is seeded \
                 from the task, so every worker is refused at spawn and the merge stage \
                 reviews alone"
            ),
        )
        .in_stage(&stage.name)
        .with_fix(
            "add a region seeded from the task, for example \
             task = { kind = \"pinned\", budget = \"10%\", seed = \"task\" }"
                .to_string(),
        ),
    ]
}
