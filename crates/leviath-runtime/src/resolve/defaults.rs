//! Step 6's second half and step 8's last check: the operator's defaults
//! filled into the graph, and the compaction model judged.
//!
//! A graph says what it wants where it cares and leaves the rest open. The
//! operator's settings fill what it leaves open, here, once, so the run's spec
//! carries every setting the run will use and nothing at run time goes back to
//! the machine's config to finish the answer.

use leviath_tools::{FAN_OUT_TOOL, SUBMIT_OUTPUT_TOOL};

use crate::spec::env::{ResolveEnv, SpawnLimits};
use crate::spec::graph::{NudgeDef, RunGraph, StageMode, ToolSelector};
use crate::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
use crate::spec::names::ToolName;

/// Give each stage what its mode cannot work without, whoever wrote the
/// graph:
///
/// - An output stage hands its answer back with `submit_output`, so it gets
///   the tool and must call it, and it may end the run when no edge leaves
///   it.
/// - A fan-out stage starts its workers with `fan_out`, so it gets that.
///
/// A tool list that already names the tool is left as it is.
pub(super) fn grant_mode_tools(graph: &mut RunGraph) {
    let leaves: Vec<bool> = graph
        .stages
        .iter()
        .map(|s| graph.edges_from(s.name.as_str()).next().is_some())
        .collect();
    for (stage, leaves) in graph.stages.iter_mut().zip(leaves) {
        let tool = match stage.mode {
            StageMode::Output => {
                stage.require_output = true;
                stage.allow_complete |= !leaves;
                SUBMIT_OUTPUT_TOOL
            }
            StageMode::FanOut(_) => FAN_OUT_TOOL,
            _ => continue,
        };
        let tool = ToolName::new(tool).expect("a stage tool's name is a valid tool name");
        let named = stage.tools.contains(&ToolSelector::Tool(tool.clone()));
        if !named {
            stage.tools.push(ToolSelector::Tool(tool));
        }
    }
}

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
