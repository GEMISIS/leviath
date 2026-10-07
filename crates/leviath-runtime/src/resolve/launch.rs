//! Step 5: what the run is trusted with, where it sits in its tree, and what
//! it answers for itself.

use std::path::PathBuf;

use crate::spec::env::{Caller, ResolveEnv, SpawnLimits};
use crate::spec::graph::RunGraph;
use crate::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
use crate::spec::launch::{LaunchPolicy, LaunchRequest, Placement};
use crate::spec::request::SpawnRequest;
use crate::spec::run_spec::AutoAnswers;

/// The run's launch policy and placement. The placement's workdir is left
/// empty; the resolver fills it once the machine has checked it.
///
/// - A top-level run gets what it asked for. A depth it leaves open takes the
///   graph's `max_child_depth`, then the operator's default.
/// - A child or a fan-out worker is narrowed against its parent's policy, so
///   it is never trusted with more than the parent. A parent with no depth
///   left may not start one at all.
/// - Seed commands run only when the operator allows them, whoever asks.
/// - A run records its model input when it asks to, or when the operator
///   records every run's.
pub(super) fn decide(
    request: &SpawnRequest,
    caller: &Caller,
    graph: &RunGraph,
    limits: &SpawnLimits,
    issues: &mut SpawnIssues,
) -> (LaunchPolicy, Placement) {
    let (parent, policy, depth, worker_stage) = match caller {
        Caller::TopLevel => {
            let depth = graph.max_child_depth.unwrap_or(limits.default_max_depth);
            let mut policy =
                LaunchPolicy::top_level(&request.launch, depth, limits.seed_commands_allowed);
            policy.capture_model_input |= limits.defaults.capture_model_input;
            if let (Some(asked), Some(cap)) = (request.launch.max_depth, graph.max_child_depth)
                && asked > cap
            {
                issues.push(
                    SpawnIssue::new(
                        SpecPath::root().field("launch").field("max_depth"),
                        IssueCode::OutOfRange,
                        "the graph allows a shallower tree of child runs than this asks for",
                    )
                    .expected(format!("at most {cap}, the graph's max_child_depth"))
                    .got(asked.to_string())
                    .hint("ask for less, or raise the graph's max_child_depth"),
                );
                policy.max_depth = cap;
            }
            return (policy, placement(None, 0, None, None));
        }
        Caller::Child {
            parent,
            policy,
            depth,
        } => (parent, policy, *depth, None),
        Caller::Worker {
            parent,
            policy,
            depth,
            stage,
            ..
        } => (parent, policy, *depth, stage.as_ref()),
    };
    let at = SpecPath::root().field("launch");
    if policy.max_depth == 0 {
        issues.push(
            SpawnIssue::new(
                at.field("max_depth"),
                IssueCode::NotAllowed,
                format!(
                    "run \"{parent}\" may not start child runs: its depth allowance is used up"
                ),
            )
            .expected("a parent with a max_depth of at least 1")
            .got("max_depth 0")
            .hint(
                "raise the top run's launch.max_depth or its graph's max_child_depth, or do \
                 the work in this run",
            ),
        );
    }
    if let Some(stage) = worker_stage
        && graph.stage(stage.as_str()).is_none()
    {
        issues.push(
            SpawnIssue::new(
                SpecPath::root(),
                IssueCode::Dangling,
                format!("the worker stage \"{stage}\" is not in the graph"),
            )
            .known(graph.stages.iter().map(|s| &s.name)),
        );
    }
    let asked = LaunchRequest {
        max_depth: request.launch.max_depth.or(graph.max_child_depth),
        ..request.launch.clone()
    };
    let mut narrowed = LaunchPolicy::narrow(&asked, policy);
    narrowed.seed_commands &= limits.seed_commands_allowed;
    narrowed.capture_model_input |= limits.defaults.capture_model_input;
    let placed = placement(
        Some(parent.clone()),
        depth.saturating_add(1),
        worker_stage.cloned(),
        caller.work_item().map(str::to_string),
    );
    (narrowed, placed)
}

/// What the run's unattended setting answers without a person, as the host
/// reads it. A profile the host does not have is an issue at
/// `launch.unattended`, and the run is judged as attended meanwhile.
pub(super) fn auto_answers(
    launch: &LaunchPolicy,
    env: &dyn ResolveEnv,
    issues: &mut SpawnIssues,
) -> AutoAnswers {
    match env.auto_answers(&launch.unattended) {
        Ok(answers) => answers,
        Err(issue) => {
            let at = SpecPath::root().field("launch").field("unattended");
            issues.push(super::rebase(&at, *issue));
            AutoAnswers::default()
        }
    }
}

fn placement(
    parent: Option<crate::spec::names::RunId>,
    depth: u8,
    worker_stage: Option<crate::spec::names::StageName>,
    work_item: Option<String>,
) -> Placement {
    Placement {
        workdir: PathBuf::new(),
        parent,
        depth,
        worker_stage,
        work_item,
    }
}
