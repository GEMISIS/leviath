//! Step 6's second half and step 8's last check: the operator's defaults
//! filled into the graph, and the compaction model judged.
//!
//! A graph says what it wants where it cares and leaves the rest open. The
//! operator's settings fill what it leaves open, here, once, so the run's spec
//! carries every setting the run will use and nothing at run time goes back to
//! the machine's config to finish the answer.

use crate::spec::env::{ResolveEnv, SpawnLimits};
use crate::spec::graph::{NudgeDef, RunGraph};
use crate::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};

/// Fill what `graph` leaves open from the operator's settings:
///
/// - A stage with no iteration ceiling, or a ceiling of `0` (which the
///   pipeline reads as none), gets the operator's. A graph may set its own
///   finite ceiling; it may not ask for none over an operator who set one.
/// - Each prompt hint the graph leaves unset takes the operator's.
/// - The nudge is filled field by field.
/// - Taint tracking is on when the operator or the graph turns it on: a
///   graph can only turn it on, so an installed blueprint never switches off
///   what the operator asked for.
pub(super) fn fold(graph: &mut RunGraph, limits: &SpawnLimits) {
    if let Some(ceiling) = limits.default_max_iterations {
        for stage in &mut graph.stages {
            if matches!(stage.max_iterations, None | Some(0)) {
                stage.max_iterations = Some(ceiling);
            }
        }
    }
    let ops = &limits.defaults;
    graph.batch_tool_hint = Some(graph.batch_tool_hint.unwrap_or(ops.batch_tool_hint));
    graph.shell_hint = Some(graph.shell_hint.unwrap_or(ops.shell_hint));
    let own = graph.nudge.take().unwrap_or_default();
    graph.nudge = Some(NudgeDef {
        enabled: own.enabled.or(ops.nudge.enabled),
        max: own.max.or(ops.nudge.max),
        text: own.text.or_else(|| ops.nudge.text.clone()),
    });
    let taint = ops.taint_tracking || graph.taint_tracking == Some(true);
    graph.taint_tracking = Some(taint);
}

/// Refuse a compaction model this machine would not send the run's context
/// to, on the same terms a stage's model is refused.
pub(super) fn check_compaction(
    graph: &RunGraph,
    at: &SpecPath,
    env: &dyn ResolveEnv,
    issues: &mut SpawnIssues,
) {
    let Some(compaction) = &graph.compaction else {
        return;
    };
    if let Err(message) = env.compaction_model(&compaction.model) {
        issues.push(
            SpawnIssue::new(
                at.field("compaction").field("model"),
                IssueCode::NotAllowed,
                format!("the compaction model is {message}"),
            )
            .got(compaction.model.to_string())
            .hint("name a compaction model this machine may send the run's context to"),
        );
    }
}
