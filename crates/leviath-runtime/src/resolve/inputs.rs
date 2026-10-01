//! Steps 4 and 6: the request's inputs, checked, and the slots they fill.

use std::path::Path;

use super::attach::{self, Files};
use crate::spec::env::{Caller, ResolveEnv};
use crate::spec::graph::{OutputDef, RunGraph, StageMode};
use crate::spec::inputs::{
    CheckCtx, InputDecl, InputSlot, InputType, InputValue, InputValues, check_inputs,
};
use crate::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
use crate::spec::names::ModelRef;
use crate::spec::request::SpawnRequest;

/// The request's inputs, as far as they could be checked.
#[derive(Debug, Clone, Default)]
pub(super) struct Checked {
    /// The checked values. Empty when any input failed its check.
    pub(super) values: InputValues,
    /// Whether every input passed.
    pub(super) ok: bool,
}

/// Check the request's inputs against the graph's declarations, then the
/// two checks that need more than the value: a `file` input's accepted types
/// against the attached file's real type, and a `path` input that must exist
/// against the workdir.
///
/// A fan-out worker is not held to the inputs the graph requires: its work
/// item is its input, and its parent's caller gave the ones the graph asks
/// for.
pub(super) fn check(
    graph: &RunGraph,
    request: &SpawnRequest,
    caller: &Caller,
    files: &Files,
    workdir: Option<&Path>,
    env: &dyn ResolveEnv,
    issues: &mut SpawnIssues,
) -> Checked {
    let names: Vec<String> = request.attachments.iter().map(|a| a.name.clone()).collect();
    let cx = CheckCtx {
        attachments: &names,
    };
    let decls: Vec<InputDecl> = match caller {
        Caller::Worker { .. } => graph
            .inputs
            .iter()
            .cloned()
            .map(|decl| InputDecl {
                required: false,
                ..decl
            })
            .collect(),
        Caller::TopLevel | Caller::Child { .. } => graph.inputs.clone(),
    };
    let checked = match check_inputs(&decls, &request.inputs, &cx) {
        Ok(values) => Checked { values, ok: true },
        Err(found) => {
            issues.absorb(found);
            Checked::default()
        }
    };
    for decl in &graph.inputs {
        let at = SpecPath::root().field("inputs").key(decl.name.as_str());
        match (&decl.ty, checked.values.get(decl.name.as_str())) {
            (InputType::File { accepts }, Some(InputValue::File(name))) => {
                if let Some(file) = files.get(name)
                    && !attach::accepts(&file.mime, accepts)
                {
                    issues.push(
                        SpawnIssue::new(
                            at,
                            IssueCode::WrongType,
                            "this input does not take files of this type",
                        )
                        .expected(decl.ty.describe())
                        .got(format!("\"{name}\", a file of type {}", file.mime)),
                    );
                }
            }
            (
                InputType::Path {
                    kind,
                    must_exist: true,
                },
                Some(InputValue::Path(path)),
            ) => {
                if let Some(dir) = workdir
                    && !env.path_exists(dir, path, *kind)
                {
                    issues.push(
                        SpawnIssue::new(
                            at,
                            IssueCode::Unresolvable,
                            "nothing is at this path in the workdir",
                        )
                        .expected(decl.ty.describe())
                        .got(format!("\"{path}\""))
                        .hint(format!(
                            "the path is read from the run's workdir, {}",
                            dir.display()
                        )),
                    );
                }
            }
            _ => {}
        }
    }
    checked
}

/// Apply every input's graph slots: a stage's model, its iteration cap, a
/// fan-out's worker cap, the run's output format and instructions. Region
/// slots are placed with the rest of what a region holds at spawn.
///
/// The graph was validated first, so each slot names a stage that exists and
/// takes its input's type. A value is read back through its text, which for
/// the types a slot takes is exactly the value.
pub(super) fn apply_slots(graph: &mut RunGraph, values: &InputValues) {
    for decl in graph.inputs.clone() {
        let Some(value) = values.get(decl.name.as_str()) else {
            continue;
        };
        let text = value.render_text();
        let count: u32 = text.parse().unwrap_or(u32::MAX);
        for slot in &decl.binds {
            let stages = graph.stages.iter_mut();
            match slot {
                InputSlot::Region(_) => {}
                InputSlot::StageModel(name) => {
                    let model = ModelRef::parse(&text).ok();
                    for stage in stages.filter(|s| s.name == *name) {
                        put_first(&mut stage.model.models, model.clone());
                    }
                }
                InputSlot::StageMaxIterations(name) => {
                    for stage in stages.filter(|s| s.name == *name) {
                        stage.max_iterations = Some(count);
                    }
                }
                InputSlot::FanOutMaxWorkers(name) => {
                    let fans = stages.filter_map(|s| match &mut s.mode {
                        StageMode::FanOut(fan) if s.name == *name => Some(fan),
                        _ => None,
                    });
                    for fan in fans {
                        fan.max_workers = count;
                    }
                }
                InputSlot::OutputFormat => {
                    graph.output.get_or_insert_with(OutputDef::default).format = Some(text.clone());
                }
                InputSlot::OutputInstructions => {
                    graph
                        .output
                        .get_or_insert_with(OutputDef::default)
                        .instructions = Some(text.clone());
                }
            }
        }
    }
}

/// A caller's model goes to the front of the stage's list; the stage's own
/// choices stay behind it as fallbacks.
fn put_first(models: &mut Vec<ModelRef>, model: Option<ModelRef>) {
    models.retain(|m| Some(m) != model.as_ref());
    models.splice(0..0, model);
}
