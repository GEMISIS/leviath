//! Steps 4 and 6: the request's inputs, checked, and the slots they fill.

use std::path::Path;

use super::attach::{self, Files};
use crate::spec::env::{Caller, ResolveEnv};
use crate::spec::graph::{OutputDef, RunGraph, StageMode};
use crate::spec::inputs::{
    CheckCtx, InputDecl, InputSlot, InputType, InputValue, InputValues, PathKind, check_inputs,
    worker_decls,
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
    /// Every input that reads on its own, whether or not another failed: what
    /// the checks that need more than the value are made on, so one input's
    /// problem never hides another's.
    pub(super) passing: InputValues,
}

/// Check the request's inputs against the graph's declarations, then the
/// two checks that need more than the value: a `file` input's accepted types
/// against the attached file's real type, and a `path` input that must exist
/// against the workdir.
///
/// A fan-out worker that runs a stage of its parent's graph is not held to
/// the inputs the graph requires: its parent's caller gave them. A worker
/// running a blueprint of its own is held to them, as a child is.
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
    let decls = match caller {
        Caller::Worker { stage, .. } => worker_decls(&graph.inputs, stage.is_some()),
        Caller::TopLevel | Caller::Child { .. } => graph.inputs.clone(),
    };
    let mut checked = match check_inputs(&decls, &request.inputs, &cx) {
        Ok(values) => Checked {
            passing: values.clone(),
            values,
            ok: true,
        },
        Err(found) => {
            issues.absorb(found);
            Checked {
                passing: each_passing(&decls, request, &cx),
                ..Checked::default()
            }
        }
    };
    let passed = std::mem::take(&mut checked.passing);
    for decl in &graph.inputs {
        let at = SpecPath::root().field("inputs").key(decl.name.as_str());
        match (&decl.ty, passed.get(decl.name.as_str())) {
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
                    // Something there of the other kind is worth naming: a
                    // file given for a directory is not a missing path.
                    let (code, message) = match (kind, env.path_exists(dir, path, PathKind::Any)) {
                        (PathKind::Dir, true) => {
                            (IssueCode::WrongType, "this path is a file, not a directory")
                        }
                        (PathKind::File, true) => {
                            (IssueCode::WrongType, "this path is a directory, not a file")
                        }
                        _ => (
                            IssueCode::Unresolvable,
                            "nothing is at this path in the workdir",
                        ),
                    };
                    issues.push(
                        SpawnIssue::new(at, code, message)
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
    checked.passing = passed;
    checked
}

/// Hold each `blueprint` input to what its type promises: a blueprint this
/// machine has installed. Asked of every input that read, so a check finds
/// the name a spawn would not.
pub(super) async fn check_blueprints(
    graph: &RunGraph,
    checked: &Checked,
    env: &dyn ResolveEnv,
    issues: &mut SpawnIssues,
) {
    for decl in &graph.inputs {
        let Some(InputValue::Blueprint(reference)) = checked.passing.get(decl.name.as_str()) else {
            continue;
        };
        if let Err(mut issue) = env.blueprint(reference).await {
            issue.path = SpecPath::root().field("inputs").key(decl.name.as_str());
            issues.push(issue);
        }
    }
}

/// Every input that reads when checked on its own, with its value.
fn each_passing(decls: &[InputDecl], request: &SpawnRequest, cx: &CheckCtx<'_>) -> InputValues {
    let values = decls.iter().filter_map(|decl| {
        let given = request
            .inputs
            .get(decl.name.as_str())
            .map(|raw| [(decl.name.to_string(), raw.clone())].into())
            .unwrap_or_default();
        let value = check_inputs(std::slice::from_ref(decl), &given, cx).ok()?;
        value.0.into_iter().next()
    });
    InputValues(values.collect())
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
                        fan.max_workers = Some(count);
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
