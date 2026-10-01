//! The checks on a `fan_out` stage: where it goes when a worker fails, and
//! whether the workers it runs on this graph have anywhere to put their work.
//!
//! Next to, not inside, the shape checks: a fan-out is the one place a
//! blueprint spawns *itself*, so what it demands of the caller it also demands
//! of its own workers, and that is easy to miss when the checks that read a
//! stage in isolation sit in one file.

use super::*;
use leviath_runtime::spec::graph::{EdgeCondition, StageDef, WorkerFailure, WorkerSource};

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
pub(super) fn lint_fanout_escape(graph: &RunGraph, stage: &StageDef) -> Vec<LintFinding> {
    let StageMode::FanOut(config) = &stage.mode else {
        return Vec::new();
    };
    if config.on_worker_failure != WorkerFailure::FailAll {
        return Vec::new();
    }
    let escapes = graph
        .edges_from(stage.name.as_str())
        .any(|edge| matches!(edge.when, EdgeCondition::Error | EdgeCondition::DeadEnd));
    if escapes {
        return Vec::new();
    }
    vec![
        LintFinding::new(
            LintSeverity::Warning,
            "fanout-no-escape",
            "sets on_worker_failure = \"fail_all\" but has no 'error' or 'dead_end' \
             edge, so one failed worker ends the run with nowhere to go"
                .to_string(),
        )
        .in_stage(stage.name.as_str())
        .with_fix(
            "add an edge with when = \"error\" to a recovery stage, or use the \
             default on_worker_failure = \"continue\""
                .to_string(),
        ),
    ]
}

/// A fan-out whose workers run this graph, which declares no input for a work
/// item to fill.
///
/// Each work item is `{ id, inputs }`, its inputs checked against the inputs
/// the worker's graph declares. A graph that binds no input to a region has
/// nowhere to put what an item carries, so every worker starts without the
/// work it was split off to do, the merge is told to cover for all of them,
/// and the run completes looking like the parallel part happened. It never
/// did.
///
/// Only fan-outs onto a stage of this graph are checked. One onto another
/// blueprint is linted when that blueprint is validated itself.
pub(super) fn lint_fanout_worker_task(graph: &RunGraph, stage: &StageDef) -> Vec<LintFinding> {
    let StageMode::FanOut(config) = &stage.mode else {
        return Vec::new();
    };
    let WorkerSource::Stage(worker) = &config.worker else {
        return Vec::new();
    };
    let lands = graph.inputs.iter().any(|input| {
        input
            .binds
            .iter()
            .any(|b| matches!(b, leviath_runtime::spec::inputs::InputSlot::Region(_)))
    });
    if lands {
        return Vec::new();
    }
    vec![
        LintFinding::new(
            LintSeverity::Error,
            "fanout-worker-task-unheld",
            format!(
                "runs its workers on stage '{worker}' of this graph, but the graph declares \
                 no input bound to a region, so a work item's inputs have nowhere to land and \
                 every worker starts without its work"
            ),
        )
        .in_stage(stage.name.as_str())
        .with_fix(
            "declare an input the split prompt fills for each item and bind it to a region, \
             for example [[graph.inputs]] name = \"task\", type = \"text\", \
             binds = [{ region = \"task\" }]"
                .to_string(),
        ),
    ]
}
