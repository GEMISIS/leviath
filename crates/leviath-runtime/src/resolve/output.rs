//! The output shape each stage asks for: the graph's, the stage's and the
//! caller's, combined.

use leviath_core::output::{ArtifactSpec, OutputSpec};

use crate::spec::graph::OutputDef;
use crate::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
use crate::spec::run_spec::{StagePlan, ToolDef};

/// Combine the graph's, the stage's and the caller's output shapes into the
/// one that governs a stage. Later levels win field by field.
///
/// `None` when no level asks for an output at all, which is how a stage with
/// nothing to hand back stays silent.
///
/// One rule is not field by field. A caller who names a `format` different
/// from the declared one retires the declared shape checks: the schema, the
/// validator and the validator tool (and what to do when they fail), and the
/// declared artifacts were
/// written for the old shape, so only the caller's own apply. A caller who
/// wants the new shape checked supplies their own.
pub(super) fn cascade(
    graph: Option<&OutputDef>,
    stage: Option<&OutputDef>,
    request: Option<&OutputDef>,
) -> Option<OutputDef> {
    let levels = [graph, stage, request];
    if levels.iter().all(Option::is_none) {
        return None;
    }
    let declared_format = pick(&levels[..2], |o| o.format.clone());
    let requested_format = request.and_then(|r| r.format.clone());
    let reshaped = requested_format.is_some() && requested_format != declared_format;
    let shape: &[Option<&OutputDef>] = match reshaped {
        true => &levels[2..],
        false => &levels[..],
    };
    Some(OutputDef {
        format: pick(&levels, |o| o.format.clone()),
        instructions: pick(&levels, |o| o.instructions.clone()),
        example: pick(&levels, |o| o.example.clone()),
        schema: pick(shape, |o| o.schema.clone()),
        validator: pick(shape, |o| o.validator.clone()),
        on_validator_error: pick(shape, |o| o.on_validator_error),
        validator_tool: pick(shape, |o| o.validator_tool.clone()),
        overwrite_artifacts: pick(&levels, |o| o.overwrite_artifacts),
        artifacts: pick(shape, |o| {
            (!o.artifacts.is_empty()).then_some(o.artifacts.clone())
        })
        .unwrap_or_default(),
    })
}

/// Hold a stage's validator tool to what the stage can call. A stage that
/// hands back an answer and has its answers judged by a tool must be given
/// that tool, spelled as its other tools are: the check reaches nothing the
/// stage could not reach itself, and one the machine does not offer is
/// refused here rather than found missing at the end of a long run. Its
/// arguments must be an object that leaves the content argument to the
/// answer. A stage with no `submit_output` never asks the tool anything.
pub(super) fn check_validator_tool(plan: &StagePlan, sat: &SpecPath, issues: &mut SpawnIssues) {
    let Some(tool) = plan.output.as_ref().and_then(|o| o.validator_tool.as_ref()) else {
        return;
    };
    let given = |name: &str| {
        let canonical = leviath_tools::canonical_tool_name(name);
        plan.tools
            .iter()
            .any(|t| leviath_tools::canonical_tool_name(t.name.as_str()) == canonical)
    };
    if !given(leviath_tools::SUBMIT_OUTPUT_TOOL) {
        return;
    }
    let at = sat.field("output").field("validator_tool");
    if let Some((field, problem)) = tool.problem() {
        issues.push(SpawnIssue::new(
            at.field(field),
            IssueCode::Invalid,
            problem,
        ));
    }
    if !given(tool.tool.as_str()) {
        issues.push(
            SpawnIssue::new(
                at.field("tool"),
                IssueCode::Unresolvable,
                format!(
                    "the stage's answers are checked by the tool \"{}\", and the stage is not \
                     given it",
                    tool.tool
                ),
            )
            .hint(
                "give the stage the tool (its available_tools, or the server or script that \
                 provides it), or name one it has",
            )
            .known(plan.tools.iter().map(|t| &t.name)),
        );
    }
}

/// The value from the latest level that sets it.
fn pick<T>(levels: &[Option<&OutputDef>], get: fn(&OutputDef) -> Option<T>) -> Option<T> {
    levels.iter().rev().flatten().find_map(|o| get(o))
}

/// Tell the model the shape it is asked for, in the `submit_output` tool's
/// description. This is the whole mechanism for arbitrary formats: the
/// label, the instructions and an example are what make a model write a
/// format it has never seen. A stage that declares nothing keeps the tool's
/// own wording.
pub(super) fn describe_submit(tools: &mut [ToolDef], output: Option<&OutputDef>) {
    let described = output
        .map(|o| leviath_core::describe_spec(&spec_of(o)))
        .unwrap_or_default();
    if described.is_empty() {
        return;
    }
    for tool in tools
        .iter_mut()
        .filter(|t| t.name.as_str() == leviath_tools::SUBMIT_OUTPUT_TOOL)
    {
        tool.description = leviath_tools::submit_output_description(&described);
    }
}

/// The parts of a shape the model is told about, in the form the wording is
/// written for.
fn spec_of(o: &OutputDef) -> OutputSpec {
    OutputSpec {
        format: o.format.clone(),
        instructions: o.instructions.clone(),
        example: o.example.clone(),
        schema: o.schema.as_ref().map(|s| s.value().clone()),
        artifacts: o
            .artifacts
            .iter()
            .map(|a| ArtifactSpec {
                name: a.name.clone(),
                mime_type: a.mime_type.to_string(),
                required: a.required,
                description: a.description.clone(),
            })
            .collect(),
        ..OutputSpec::default()
    }
}
