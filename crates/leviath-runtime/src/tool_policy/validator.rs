//! The rule that a stage's validator tool must simply run, and the hint for
//! one that would have to ask.
//!
//! Nobody is there to approve a call in the middle of a submission, so a
//! stage whose answers are judged by a tool is refused at spawn unless a
//! call to that tool from that stage runs: not denied, not set to `ask`, not
//! a question for a person. The refusal is the same wherever it is decided.
//! The daemon decides it when it binds a run, with the verdict of its tool
//! lane; a program that embeds the runtime decides it with
//! [`check_validator_tools`], from the permission layers alone.

use leviath_providers::ToolCall;

use super::{RunPermissions, ToolPermissions, graph_may_allow, graph_says_allow, user_sets};
use crate::pipeline::ToolVerdict;
use crate::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
use crate::spec::run_spec::RunSpec;

/// Every stage of `spec` whose answers a validator tool judges, where the
/// tool would not simply run, as a daemon whose user has `settings` refuses
/// it at spawn: one issue per stage, at
/// `stages.<stage>.output.validator_tool.tool`, saying why the call would
/// not run and what would let it.
///
/// The call is decided by [`RunPermissions::verdict`], from what is written
/// down: the graph's and the stage's `tool_permissions`, the graph's
/// `safe_commands`, the launch's `allow` and unattended setting, and the
/// user's `settings`, their safe list included. A host that knows more about
/// the call decides it with [`validator_tool_refusals`] and its own verdict.
pub fn check_validator_tools(spec: &RunSpec, settings: &ToolPermissions) -> SpawnIssues {
    let layers = RunPermissions::of(spec, settings);
    validator_tool_refusals(spec, &layers, &|stage_index, call| {
        layers.verdict(spec, stage_index, call)
    })
}

/// Every stage whose answers a validator tool judges, where the tool would
/// not simply run: denied, set to `ask`, or a question for a person.
/// `decide` gives the verdict on the tool's call as a mid-run call from the
/// stage at that index would get it, and only a call that runs passes. Nobody is
/// there to ask mid-submission, so `ask` is refused here, where it can be
/// fixed, and not at the end of a long run.
///
/// `layers` are the run's permission layers, which the hint for a call that
/// would ask is worked out from. A stage with no `submit_output` never asks
/// the tool anything and is not judged.
pub fn validator_tool_refusals(
    spec: &RunSpec,
    layers: &RunPermissions,
    decide: &dyn Fn(usize, &ToolCall) -> ToolVerdict,
) -> SpawnIssues {
    let mut issues = SpawnIssues::new();
    for plan in &spec.stages {
        let Some(validator) = plan.output.as_ref().and_then(|o| o.validator_tool.as_ref()) else {
            continue;
        };
        let submits = plan
            .tools
            .iter()
            .any(|t| t.name.as_str() == leviath_tools::SUBMIT_OUTPUT_TOOL);
        let Some(stage_index) = spec
            .graph
            .stages
            .iter()
            .position(|s| s.name == plan.stage)
            .filter(|_| submits)
        else {
            continue;
        };
        let call = ToolCall {
            id: "validator".to_string(),
            name: validator.tool.to_string(),
            arguments: validator.spec().arguments(""),
            thought_signature: None,
        };
        let verdict = decide(stage_index, &call);
        let asks = matches!(verdict, ToolVerdict::Ask { .. });
        let why = match verdict {
            ToolVerdict::Run { .. } => continue,
            // Its first sentence: the rest is written for a run already
            // going ("resume this run"), and the hint below says what to do
            // at spawn.
            ToolVerdict::Refuse(text) => text
                .split_once(". ")
                .map_or(text.as_str(), |(first, _)| first)
                .to_string(),
            ToolVerdict::Ask { .. } => {
                "it is set to ask, and nobody is there to approve it mid-submission".to_string()
            }
            ToolVerdict::Interact { .. } => {
                "it puts a question to a person, which no check can wait on".to_string()
            }
        };
        issues.push(
            SpawnIssue::new(
                SpecPath::root()
                    .field("stages")
                    .key(plan.stage.as_str())
                    .field("output")
                    .field("validator_tool")
                    .field("tool"),
                IssueCode::NotAllowed,
                format!(
                    "the validator tool \"{}\" would not run: {why}",
                    validator.tool
                ),
            )
            .hint(match asks {
                true => ask_hint(
                    layers,
                    stage_index,
                    validator.tool.as_str(),
                    spec.origin.blueprint_name(),
                ),
                false => format!(
                    "set `{} = \"allow\"` in tool_permissions, or name a tool the stage may \
                     call unprompted",
                    validator.tool
                ),
            }),
        );
    }
    issues
}

/// What to change so the validator tool `tool`, called from stage
/// `stage_index`, runs without asking. `blueprint` is the installed
/// blueprint the run came from, if any.
///
/// A graph's `tool_permissions` take effect only for a tool a graph may
/// loosen (see [`resolve_policy`](super::resolve_policy)), so asking the
/// caller to write one is right only then. For any other tool the spawn's
/// own `allow` lets it run, and so does the user's
/// `[agent_tool_permissions.<name>]` for an installed blueprint. `[security]
/// allow_blueprint_permissions` lets the graph's own `allow` count, so it is
/// named only where the graph says one and the user has no setting of their
/// own for the tool.
fn ask_hint(
    layers: &RunPermissions,
    stage_index: usize,
    tool: &str,
    blueprint: Option<&str>,
) -> String {
    let leave = "or name a tool the stage may call unprompted";
    if graph_may_allow(&layers.user, tool, layers.blueprint_may_loosen) {
        return format!("set `{tool} = \"allow\"` in tool_permissions, {leave}");
    }
    let said_allow = layers
        .stages
        .get(stage_index)
        .is_some_and(|stage| graph_says_allow(stage, &layers.graph, tool));
    let lead = match said_allow {
        true => format!(
            "`{tool} = \"allow\"` in tool_permissions has no effect, because a graph cannot \
             allow this tool by itself"
        ),
        false => "a graph cannot allow this tool by itself".to_string(),
    };
    let mut ways =
        vec!["name it in this spawn's `launch.allow` (`--allow` on the command line)".to_string()];
    if let Some(name) = blueprint {
        ways.push(format!(
            "set `{tool} = \"allow\"` under `[agent_tool_permissions.{name}]` in the user's config"
        ));
        if said_allow && !user_sets(&layers.user, tool) {
            ways.push(
                "turn on `[security] allow_blueprint_permissions` in the user's config".to_string(),
            );
        }
    }
    format!("{lead}; {}, {leave}", ways.join(", or "))
}

#[cfg(test)]
#[path = "validator_tests.rs"]
mod tests;
