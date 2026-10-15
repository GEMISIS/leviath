//! What a fan-out hands each worker, and the request that starts one.
//!
//! A `fan_out` call lists work items. In a fan-out stage each item goes to one
//! of the workers the stage declares (`workers_available`): the item names it
//! when there are several, and there is nothing to name when there is one. A
//! name the stage does not declare refuses the whole call, before anything
//! starts. A call from an ordinary stage names one blueprint, its `agent`,
//! for every item.
//!
//! Each item carries typed inputs for the graph its worker runs, and every
//! item is checked against that graph's declared inputs before any worker
//! starts: this graph's for a stage worker, the blueprint's when the spawner
//! can read it. A mistyped item refuses the whole call, with the path of the
//! value that did not fit.
//!
//! Kept apart from [`super`], which starts, tracks and merges workers: this
//! is the shape of the work and the request each worker is spawned from.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::spec::env::Caller;
use crate::spec::graph::{FanOutDef, WorkerOption, WorkerSource};
use crate::spec::inputs::{InputDecl, RawInput, check_inputs_at, worker_decls};
use crate::spec::issues::{SpawnIssues, SpecPath};
use crate::spec::launch::LaunchRequest;
use crate::spec::names::{StageName, WorkerName};
use crate::spec::request::{SpawnRequest, SpawnSource};
use crate::spec::run_spec::RunSpec;
use std::path::Path;

/// One unit of work produced by a fan-out call, and the worker that takes it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct WorkItem {
    /// Stable id (used to label the worker in the consolidated report).
    #[serde(default)]
    pub id: String,
    /// The fan-out's worker that takes it, one of its `workers_available`.
    pub worker: WorkerName,
    /// The worker's inputs, by the name its graph declares them under.
    #[serde(default)]
    pub inputs: BTreeMap<String, RawInput>,
}

/// A work item as a `fan_out` call writes it: an id, the worker that takes
/// it, and the worker's inputs by name. Nothing else is accepted, so a
/// misspelled key is refused rather than dropped.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ItemArgs {
    id: String,
    #[serde(default)]
    worker: Option<String>,
    #[serde(default)]
    inputs: BTreeMap<String, RawInput>,
}

/// A work item as the call wrote it, before its worker is settled.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RequestedItem {
    /// The item's id.
    pub id: String,
    /// The worker the call named for it, if it named one.
    pub worker: Option<String>,
    /// The worker's inputs, by name.
    pub inputs: BTreeMap<String, RawInput>,
}

/// A `fan_out` call the dispatcher has read but not yet started.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FanOutRequest {
    /// The blueprint to run for every item, when the caller named one: an
    /// installed name, or the absolute directory of one that is not installed.
    /// Only a call from an ordinary stage names one; a fan-out stage hands
    /// items to the workers its blueprint declares.
    pub agent: Option<String>,
    /// The work, one entry per worker.
    pub items: Vec<RequestedItem>,
    /// A per-call concurrency cap, when the caller asked for one. At least
    /// one: a call asking for none is refused.
    pub max_workers: Option<usize>,
}

/// Whether a tool call is the fan-out tool.
pub(crate) fn is_fan_out_tool(name: &str) -> bool {
    name == leviath_core::stage_tools::FAN_OUT_TOOL
}

/// Read a `fan_out` call's arguments.
///
/// Strict: the arguments came through a schema the provider enforced, so a
/// shape that does not fit is a real mistake and the model is told so rather
/// than guessed at. The refusal is an `[error]` tool result, which the model
/// corrects on its next turn like any other.
pub(crate) fn parse_fan_out_call(arguments: &serde_json::Value) -> Result<FanOutRequest, String> {
    let object = arguments
        .as_object()
        .ok_or_else(|| "fan_out arguments must be an object".to_string())?;
    let items = match object.get("items") {
        Some(serde_json::Value::Array(items)) => items,
        Some(_) => return Err("fan_out `items` must be an array".to_string()),
        None => return Err("fan_out requires an `items` array".to_string()),
    };
    let items: Vec<RequestedItem> = items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            serde_json::from_value::<ItemArgs>(item.clone())
                .map(|item| RequestedItem {
                    id: item.id,
                    worker: item.worker,
                    inputs: item.inputs,
                })
                .map_err(|e| format!("fan_out items[{i}] is not {{id, worker, inputs}}: {e}"))
        })
        .collect::<Result<_, _>>()?;
    let agent = object
        .get("agent")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .filter(|a| !a.trim().is_empty());
    let max_workers = object
        .get("max_workers")
        .and_then(serde_json::Value::as_u64)
        .map(|n| n as usize);
    if max_workers == Some(0) {
        return Err(
            "fan_out `max_workers` must be at least 1; leave it out to run every item at once"
                .to_string(),
        );
    }
    Ok(FanOutRequest {
        agent,
        items,
        max_workers,
    })
}

/// The last line of every refusal: the model is told that nothing ran, so it
/// knows a corrected call is a fresh start rather than a second batch.
const NOTHING_STARTED: &str = "Nothing was started.";

/// Settle a request into the fan-out it runs as and the items it hands out,
/// each with its worker, or refuse it with every problem at once.
///
/// In a fan-out stage (`stage` is its settings) the worker of each item is one
/// the stage declares: the one the item names when it declares several, its
/// only one otherwise. A call from an ordinary stage (`calling` is its name)
/// names the blueprint every item runs as its `agent`, which must be absolute
/// when it is a directory, and may not be inside `workdir`: the run's own files
/// are ones its model could have written, and a worker blueprint brings its
/// own seeds and MCP servers. Either way the result is one [`FanOutDef`], so the
/// cap, the failure policy and the report are the same code for both entry
/// points.
pub(crate) fn settle(
    request: &FanOutRequest,
    stage: Option<&FanOutDef>,
    calling: &StageName,
    workdir: Option<&Path>,
) -> Result<(FanOutDef, Vec<WorkItem>), String> {
    let (mut config, items) = match stage {
        Some(stage) => (stage.clone(), stage_items(request, stage)?),
        None => tool_call(request, calling, workdir)?,
    };
    if let Some(max_workers) = request.max_workers {
        config.max_workers = Some(clamp(max_workers));
    }
    Ok((config, items))
}

/// The items of a call from a fan-out stage, each with the declared worker it
/// goes to, or the refusal naming every item that names no such worker.
fn stage_items(request: &FanOutRequest, stage: &FanOutDef) -> Result<Vec<WorkItem>, String> {
    if let Some(sole) = stage.sole_worker() {
        return sole_worker_items(request, sole);
    }
    let mut problems = Vec::new();
    if request.agent.is_some() {
        problems.push(
            "\"agent\" is not used in this stage, which hands each item to the worker the item \
             names."
                .to_string(),
        );
    }
    let mut items = Vec::with_capacity(request.items.len());
    for (i, item) in request.items.iter().enumerate() {
        match item
            .worker
            .as_deref()
            .map(|name| (name, stage.worker(name)))
        {
            Some((_, Some(worker))) => items.push(WorkItem {
                id: item.id.clone(),
                worker: worker.name.clone(),
                inputs: item.inputs.clone(),
            }),
            Some((name, None)) => problems.push(format!(
                "items[{i}] names the worker \"{name}\", which this stage does not have."
            )),
            None => problems.push(format!(
                "items[{i}] has no \"worker\". This stage has more than one worker, so each item \
                 names the one that takes it."
            )),
        }
    }
    if problems.is_empty() {
        return Ok(items);
    }
    let menu: Vec<String> = stage
        .workers_available
        .iter()
        .map(|w| format!("- {}: {}", w.name, w.use_for))
        .collect();
    let agent = match request.agent.is_some() {
        true => ", leave \"agent\" out,",
        false => "",
    };
    Err(format!(
        "{}\nThis stage's workers are:\n{}\nSet each item's \"worker\" to one of those \
         names{agent} and call fan_out again. {NOTHING_STARTED}",
        refusal(&problems),
        menu.join("\n")
    ))
}

/// The items of a call from a fan-out stage with one worker, which takes
/// every item. Naming that worker is restating it; naming anything else is
/// refused.
fn sole_worker_items(
    request: &FanOutRequest,
    sole: &WorkerOption,
) -> Result<Vec<WorkItem>, String> {
    let named = |name: &str| name == sole.name.as_str();
    let mut problems = Vec::new();
    let wrong_agent = request.agent.as_deref().is_some_and(|a| !named(a.trim()));
    if wrong_agent {
        problems.push(format!(
            "\"agent\" is not used in this stage. It hands items only to the worker its \
             blueprint declares, {}.",
            sole.name
        ));
    }
    let mut wrong_items = false;
    for (i, item) in request.items.iter().enumerate() {
        if let Some(name) = item.worker.as_deref().filter(|name| !named(name)) {
            wrong_items = true;
            problems.push(format!(
                "items[{i}] names the worker \"{name}\", but this stage has one worker, {}, \
                 which takes every item.",
                sole.name
            ));
        }
    }
    let leave_out = match (wrong_agent, wrong_items) {
        (false, false) => {
            return Ok(request
                .items
                .iter()
                .map(|item| WorkItem {
                    id: item.id.clone(),
                    worker: sole.name.clone(),
                    inputs: item.inputs.clone(),
                })
                .collect());
        }
        (true, false) => "without \"agent\"",
        (false, true) => "without \"worker\" on its items",
        (true, true) => "without \"agent\" and without \"worker\" on its items",
    };
    Err(format!(
        "{}\nCall fan_out again {leave_out}. {NOTHING_STARTED}",
        refusal(&problems)
    ))
}

/// The fan-out a call from an ordinary stage runs: the blueprint its `agent`
/// names, for every item, under the engine's defaults.
fn tool_call(
    request: &FanOutRequest,
    calling: &StageName,
    workdir: Option<&Path>,
) -> Result<(FanOutDef, Vec<WorkItem>), String> {
    let mut problems = Vec::new();
    let worker = match request.agent.as_deref() {
        Some(agent) => named_worker(agent, workdir)
            .map_err(|why| problems.push(why))
            .ok(),
        None => {
            problems.push(
                "\"agent\" is missing. Outside a fan-out stage, \"agent\" names the installed \
                 blueprint every item runs."
                    .to_string(),
            );
            None
        }
    };
    for (i, item) in request.items.iter().enumerate() {
        if item.worker.is_some() {
            problems.push(format!(
                "items[{i}] names a worker, but only a fan-out stage declares workers. Name the \
                 blueprint every item runs as \"agent\", and leave \"worker\" out."
            ));
        }
    }
    match worker {
        Some(worker) if problems.is_empty() => {
            let items = request
                .items
                .iter()
                .map(|item| WorkItem {
                    id: item.id.clone(),
                    worker: worker.name.clone(),
                    inputs: item.inputs.clone(),
                })
                .collect();
            Ok((FanOutDef::with_worker(calling.clone(), worker), items))
        }
        _ => Err(format!("{} {NOTHING_STARTED}", refusal(&problems))),
    }
}

/// The refusal for a call whose items use a worker whose blueprint does not
/// load. `failed` holds each such worker and why. `agent` is the blueprint a
/// call from an ordinary stage named; `None` for a fan-out stage's call.
pub(crate) fn unloadable(
    config: &FanOutDef,
    items: &[WorkItem],
    failed: &[(WorkerName, String)],
    agent: Option<&str>,
) -> String {
    let sole = config.sole_worker().is_some();
    let problems: Vec<String> = failed
        .iter()
        .map(|(name, why)| match (agent, sole) {
            (Some(agent), _) => format!("the agent \"{agent}\" could not be loaded: {why}"),
            (None, true) => format!("this stage's worker \"{name}\" could not be loaded: {why}"),
            (None, false) => {
                let at: Vec<String> = items
                    .iter()
                    .enumerate()
                    .filter(|(_, item)| &item.worker == name)
                    .map(|(i, _)| format!("items[{i}]"))
                    .collect();
                format!(
                    "{} picked \"{name}\", but it could not be loaded: {why}",
                    at.join(", ")
                )
            }
        })
        .collect();
    let advice = match (agent, sole) {
        (Some(_), _) => "Name an installed blueprint as \"agent\". ",
        (None, true) => "",
        (None, false) => "Give those items another of this stage's workers, or leave them out. ",
    };
    format!("{}\n{advice}{NOTHING_STARTED}", refusal(&problems))
}

/// The first line of a refusal: its one problem, or each of several on a
/// line of its own.
fn refusal(problems: &[String]) -> String {
    match problems {
        [one] => format!("fan_out refused: {one}"),
        many => format!("fan_out refused:\n- {}", many.join("\n- ")),
    }
}

/// The worker a `fan_out` call's `agent` names. See [`settle`].
fn named_worker(agent: &str, workdir: Option<&Path>) -> Result<WorkerOption, String> {
    let worker = WorkerSource::named(agent).map_err(|e| {
        format!(
            "\"agent\" {e}. Name an installed blueprint, or give the absolute directory of one."
        )
    })?;
    if let (WorkerSource::BlueprintFile(path), Some(workdir)) = (&worker, workdir)
        && leviath_core::resolves_within(path.path(), workdir)
    {
        return Err(format!(
            "\"agent\" '{agent}' is inside this run's own working directory. Name an installed \
             blueprint, or one outside the workspace: an agent may not author the blueprint its \
             workers run."
        ));
    }
    Ok(WorkerOption::named_after(
        worker,
        crate::spec::graph::EVERY_ITEM,
    ))
}

/// A count as the graph stores it.
fn clamp(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The most workers a fan-out runs at once; `None` when it sets no limit.
pub(crate) fn worker_cap(config: &FanOutDef) -> Option<usize> {
    config.max_workers.map(|n| n as usize)
}

/// What one worker's inputs are checked against: the declarations of the
/// graph it runs, and whether that is its parent's own graph.
#[derive(Clone, Copy)]
pub(crate) struct WorkerDecls<'a> {
    /// The graph's declared inputs.
    pub decls: &'a [InputDecl],
    /// Whether the worker runs a stage of its parent's own graph, which is
    /// not asked for the graph's required inputs: it enters partway through a
    /// run whose caller already gave them.
    pub same_graph: bool,
}

/// Check every item's inputs against its worker's declarations, as the
/// worker's own spawn checks them: a name the graph does not declare, a
/// required input with no value and no default, and a value of the wrong
/// type. An item whose worker's declarations are not known (`decls_of` gives
/// `None`) is left to that worker's own start.
///
/// Reports every problem at once, each at its item's path
/// (`items[2].inputs.topic`).
pub(crate) fn check_items<'a>(
    items: &[WorkItem],
    decls_of: impl Fn(&WorkerName) -> Option<WorkerDecls<'a>>,
) -> Result<(), SpawnIssues> {
    let mut issues = SpawnIssues::new();
    for (i, item) in items.iter().enumerate() {
        let Some(worker) = decls_of(&item.worker) else {
            continue;
        };
        let decls = worker_decls(worker.decls, worker.same_graph);
        let at = SpecPath::root().field("items").index(i).field("inputs");
        check_inputs_at(&decls, &item.inputs, &at, &mut issues);
    }
    issues.into_result(())
}

/// The request and caller that start one worker of `parent`'s fan-out.
///
/// `source` is what the worker runs, already resolved from `worker`'s
/// [`WorkerSource`]. The worker inherits how unattended its parent is and the
/// tools it may call without asking, runs no spawn-time shell seeds (its parent already scoped the work, so every
/// worker re-running them is waste), and gets the parent's requested model
/// and output shape.
pub(crate) fn worker_request(
    parent: &RunSpec,
    worker: &WorkerOption,
    item: &WorkItem,
    source: SpawnSource,
    depth: usize,
) -> (SpawnRequest, Caller) {
    let request = SpawnRequest {
        inputs: item.inputs.clone(),
        model: parent.requested_model.clone(),
        output: parent.requested_output.clone(),
        workdir: Some(parent.placement.workdir.clone()),
        launch: LaunchRequest {
            unattended: parent.launch.unattended.clone(),
            allow: parent.launch.allow.clone(),
            seed_commands: false,
            ..LaunchRequest::default()
        },
        ..SpawnRequest::new(source)
    };
    let caller = Caller::Worker {
        parent: parent.run_id.clone(),
        policy: parent.launch.clone(),
        depth: u8::try_from(depth).unwrap_or(u8::MAX),
        stage: match &worker.runs {
            WorkerSource::Stage(stage) => Some(stage.clone()),
            WorkerSource::Blueprint(_)
            | WorkerSource::BlueprintFile(_)
            | WorkerSource::Query(_) => None,
        },
        item: item.id.clone(),
    };
    (request, caller)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::graph::WorkerFailure;
    use crate::spec::inputs::InputType;
    use crate::spec::names::{BlueprintRef, InputName};

    fn name(text: &str) -> WorkerName {
        WorkerName::new(text).unwrap()
    }

    fn stage_name(text: &str) -> StageName {
        StageName::new(text).unwrap()
    }

    fn option(worker: &str, runs: WorkerSource, use_for: &str) -> WorkerOption {
        WorkerOption {
            name: name(worker),
            runs,
            use_for: use_for.into(),
        }
    }

    /// A fan-out stage with one worker, `w`, a stage of the same graph.
    fn stage_def(max_workers: u32) -> FanOutDef {
        FanOutDef {
            stage: stage_name("split"),
            workers_available: vec![option(
                "w",
                WorkerSource::Stage(stage_name("w")),
                "Reviewing one area",
            )],
            merge_stage: Some(stage_name("merge")),
            max_workers: (max_workers > 0).then_some(max_workers),
            on_worker_failure: WorkerFailure::FailAll,
            split_prompt: "split".into(),
            results_region: None,
            max_items: Some(5),
            max_attempts: None,
        }
    }

    /// A fan-out stage with three workers of three kinds.
    fn several() -> FanOutDef {
        FanOutDef {
            workers_available: vec![
                option(
                    "code",
                    WorkerSource::Stage(stage_name("w")),
                    "Reviewing one area of the code",
                ),
                option(
                    "docs",
                    WorkerSource::Blueprint(BlueprintRef::parse("writer").unwrap()),
                    "Checking one docs page",
                ),
                option(
                    "research",
                    WorkerSource::Query("a researcher".into()),
                    "Looking a question up",
                ),
            ],
            ..stage_def(0)
        }
    }

    fn requested(id: &str, worker: Option<&str>) -> RequestedItem {
        RequestedItem {
            id: id.into(),
            worker: worker.map(str::to_string),
            inputs: BTreeMap::new(),
        }
    }

    fn request(
        agent: Option<&str>,
        items: Vec<RequestedItem>,
        max_workers: Option<usize>,
    ) -> FanOutRequest {
        FanOutRequest {
            agent: agent.map(str::to_string),
            items,
            max_workers,
        }
    }

    /// The name of the ordinary stage a tool call comes from.
    fn calling() -> StageName {
        stage_name("work")
    }

    fn decl(name: &str, ty: InputType) -> InputDecl {
        InputDecl {
            name: InputName::new(name).unwrap(),
            ty,
            required: true,
            default: None,
            description: None,
            binds: vec![],
        }
    }

    fn item(id: &str, inputs: &[(&str, RawInput)]) -> WorkItem {
        WorkItem {
            id: id.into(),
            worker: name("w"),
            inputs: inputs
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
        }
    }

    /// Typed inputs and a worker travel as the call wrote them. A `context`
    /// is not the worker's task text: it is an unknown key like any other,
    /// and an item with no id is refused at its index.
    #[test]
    fn items_carry_typed_inputs_and_nothing_else() {
        let request = parse_fan_out_call(&serde_json::json!({
            "items": [
                {"id": "a", "worker": "code", "inputs": {"topic": "rust", "depth": 2}},
                {"id": "c"}
            ]
        }))
        .unwrap();
        assert_eq!(request.items[0].inputs["depth"], RawInput::Int(2));
        assert_eq!(request.items[0].worker.as_deref(), Some("code"));
        assert!(request.items[1].inputs.is_empty());
        assert_eq!(request.items[1].worker, None);
        let err = parse_fan_out_call(&serde_json::json!({
            "items": [{"id": "x", "inputs": {}}, {"id": "b", "context": {"file": "a.rs"}}]
        }))
        .unwrap_err();
        assert!(err.contains("items[1]"), "{err}");
        assert!(err.contains("unknown field `context`"), "{err}");
        let err = parse_fan_out_call(&serde_json::json!({"items": [{"inputs": {}}]})).unwrap_err();
        assert!(err.contains("missing field `id`"), "{err}");
    }

    /// The schema `fan_out` is advertised with and the reader agree: every
    /// call the reader takes, the schema takes, and every call the schema
    /// refuses, the reader refuses too.
    #[test]
    fn the_advertised_schema_and_the_reader_agree() {
        use leviath_tools::validate::{ArgValidation, validate_tool_args};
        let tools =
            leviath_tools::BuiltinTools::new(leviath_tools::ToolContext::new(std::env::temp_dir()));
        let schema = tools
            .tool_defs()
            .into_iter()
            .find(|t| t.name == leviath_core::stage_tools::FAN_OUT_TOOL)
            .expect("fan_out is advertised")
            .parameters;
        for (call, readable) in [
            (serde_json::json!({"items": []}), true),
            (
                serde_json::json!({"agent": "a", "items": [{"id": "x"}]}),
                true,
            ),
            (
                serde_json::json!({"items": [{"id": "x", "inputs": {"task": "t", "n": 2}}], "max_workers": 3}),
                true,
            ),
            (serde_json::json!({"items": [{"inputs": {}}]}), false),
            (
                serde_json::json!({"items": [{"id": "x", "context": {}}]}),
                false,
            ),
            (serde_json::json!({"items": "all"}), false),
            (serde_json::json!({}), false),
        ] {
            let valid = validate_tool_args("fan_out", &schema, &call) == ArgValidation::Valid;
            assert_eq!(valid, readable, "the schema on {call}");
            assert_eq!(
                parse_fan_out_call(&call).is_ok(),
                readable,
                "the reader on {call}"
            );
        }
    }

    /// An `agent` given as a directory runs the blueprint there, unless it is
    /// inside the run's own workspace; a relative path names nothing.
    #[test]
    fn an_agent_named_by_its_directory_runs_from_there() {
        let work = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let outside = elsewhere.path().to_string_lossy().into_owned();
        let call = |agent: &str| request(Some(agent), vec![requested("a", None)], None);
        let (config, items) = settle(&call(&outside), None, &calling(), Some(work.path())).unwrap();
        assert_eq!(
            config.workers_available[0].runs,
            WorkerSource::BlueprintFile(
                crate::spec::names::BlueprintPath::new(outside.as_str()).unwrap()
            )
        );
        assert_eq!(items[0].worker, config.workers_available[0].name);
        let inside = work.path().join("x").to_string_lossy().into_owned();
        let err = settle(&call(&inside), None, &calling(), Some(work.path())).unwrap_err();
        assert!(err.contains("own working directory"), "{err}");
        // With no workspace to compare against, the directory is taken as named.
        assert!(settle(&call(&inside), None, &calling(), None).is_ok());
        let err = settle(&call("./x"), None, &calling(), None).unwrap_err();
        assert!(err.contains("absolute directory"), "{err}");
    }

    fn text() -> InputType {
        InputType::Text {
            multiline: false,
            min_len: None,
            max_len: None,
        }
    }

    /// A mistyped item is refused at its own path, every problem at once, and
    /// an input the worker does not declare names the ones it does. An item
    /// whose worker's inputs are not known is left to the worker's start.
    #[test]
    fn a_mistyped_item_is_refused_at_its_path() {
        let decls = vec![
            decl("topic", text()),
            decl(
                "depth",
                InputType::Int {
                    min: Some(1),
                    max: Some(3),
                },
            ),
        ];
        let mut unknown = item("u", &[("anything", RawInput::Bool(true))]);
        unknown.worker = name("elsewhere");
        let items = vec![
            item("a", &[("topic", RawInput::Text("rust".into()))]),
            item(
                "c",
                &[
                    ("topic", RawInput::Int(4)),
                    ("depth", RawInput::Int(9)),
                    ("colour", RawInput::Bool(true)),
                ],
            ),
            unknown,
        ];
        let decls_of = |worker: &WorkerName| {
            (worker.as_str() == "w").then_some(WorkerDecls {
                decls: &decls,
                same_graph: true,
            })
        };
        let issues = check_items(&items, decls_of).unwrap_err();
        let lines: Vec<String> = issues
            .iter()
            .map(|i| format!("{} {:?}", i.path, i.code))
            .collect();
        assert_eq!(
            lines,
            vec![
                "items[1].inputs.colour Unknown",
                "items[1].inputs.topic WrongType",
                "items[1].inputs.depth OutOfRange",
            ]
        );
        assert_eq!(issues.iter().next().unwrap().known, vec!["topic", "depth"]);
        assert!(check_items(&items[..1], decls_of).is_ok());
    }

    /// A worker running a blueprint of its own is checked as `spawn_agent`
    /// checks a child: a required input with no value and no default is
    /// missing, at its item's path, and a default fills one left out. A
    /// worker running a stage of its parent's graph is not asked for it.
    #[test]
    fn a_blueprint_workers_item_is_held_to_its_required_inputs() {
        let decls = vec![
            decl("task", text()),
            InputDecl {
                default: Some(crate::spec::inputs::InputValue::Int(1)),
                ..decl(
                    "count",
                    InputType::Int {
                        min: None,
                        max: None,
                    },
                )
            },
        ];
        let items = vec![
            item("x0", &[("task", RawInput::Text("go".into()))]),
            item("x1", &[("count", RawInput::Int(1))]),
            item("x2", &[]),
        ];
        let blueprint = |_: &WorkerName| {
            Some(WorkerDecls {
                decls: &decls,
                same_graph: false,
            })
        };
        let own_stage = |_: &WorkerName| {
            Some(WorkerDecls {
                decls: &decls,
                same_graph: true,
            })
        };
        let issues = check_items(&items, blueprint).unwrap_err();
        let lines: Vec<String> = issues
            .iter()
            .map(|i| format!("{} {:?}", i.path, i.code))
            .collect();
        assert_eq!(
            lines,
            vec![
                "items[1].inputs.task Missing",
                "items[2].inputs.task Missing"
            ]
        );
        assert!(check_items(&items, own_stage).is_ok());
    }

    /// A call from an ordinary stage runs the blueprint its `agent` names for
    /// every item, under the engine's defaults, in a fan-out of the stage it
    /// came from. Without an `agent`, or naming a worker on an item, it is
    /// refused.
    #[test]
    fn a_bare_call_names_its_blueprint_and_takes_the_defaults() {
        let call = request(
            Some("researcher"),
            vec![requested("a", None), requested("b", None)],
            None,
        );
        let (config, items) = settle(&call, None, &calling(), None).unwrap();
        assert_eq!(config.stage, calling());
        let worker = config.sole_worker().expect("one worker");
        assert_eq!(
            worker.runs,
            WorkerSource::Blueprint(BlueprintRef::parse("researcher").unwrap())
        );
        assert_eq!(worker.name.as_str(), "researcher");
        assert!(items.iter().all(|i| i.worker == worker.name));
        assert_eq!(config.max_workers, None);
        assert_eq!(config.on_worker_failure, WorkerFailure::Continue);
        assert_eq!(config.max_items, None);
        let err = settle(&request(None, vec![], None), None, &calling(), None).unwrap_err();
        assert_eq!(
            err,
            "fan_out refused: \"agent\" is missing. Outside a fan-out stage, \"agent\" names the \
             installed blueprint every item runs. Nothing was started."
        );
        let err = settle(
            &request(Some("x@nothex"), vec![], None),
            None,
            &calling(),
            None,
        )
        .unwrap_err();
        assert!(err.starts_with("fan_out refused: \"agent\" "), "{err}");
        let err = settle(
            &request(Some("researcher"), vec![requested("a", Some("code"))], None),
            None,
            &calling(),
            None,
        )
        .unwrap_err();
        assert_eq!(
            err,
            "fan_out refused: items[0] names a worker, but only a fan-out stage declares \
             workers. Name the blueprint every item runs as \"agent\", and leave \"worker\" \
             out. Nothing was started."
        );
        let err = settle(
            &request(None, vec![requested("a", Some("code"))], None),
            None,
            &calling(),
            None,
        )
        .unwrap_err();
        assert!(
            err.starts_with("fan_out refused:\n- \"agent\" is missing."),
            "{err}"
        );
    }

    /// A fan-out stage with one worker hands it every item, keeping the
    /// stage's settings under the call's own cap. Naming that worker is
    /// restating it.
    #[test]
    fn a_one_worker_stage_hands_it_every_item() {
        let stage = stage_def(3);
        let call = request(None, vec![requested("a", None)], None);
        let (config, items) = settle(&call, Some(&stage), &calling(), None).unwrap();
        assert_eq!(config, stage);
        assert_eq!(items[0].worker.as_str(), "w");
        let call = request(Some("w"), vec![requested("a", Some("w"))], Some(9));
        let (config, items) = settle(&call, Some(&stage), &calling(), None).unwrap();
        assert_eq!(config.max_workers, Some(9));
        assert_eq!(config.max_items, Some(5));
        assert_eq!(items[0].worker.as_str(), "w");
        assert_eq!(clamp(usize::MAX), u32::MAX);
        assert_eq!(worker_cap(&stage_def(0)), None);
        assert_eq!(worker_cap(&stage), Some(3));
    }

    /// A one-worker stage refuses an `agent` or an item `worker` that names
    /// anything else, saying which and what to send instead. This is what a
    /// reviewer run sent: `"agent": "review"`.
    #[test]
    fn a_one_worker_stage_refuses_any_other_name() {
        let stage = stage_def(0);
        let refused = |call: FanOutRequest| settle(&call, Some(&stage), &calling(), None);
        assert_eq!(
            refused(request(Some("review"), vec![requested("a", None)], None)).unwrap_err(),
            "fan_out refused: \"agent\" is not used in this stage. It hands items only to the \
             worker its blueprint declares, w.\nCall fan_out again without \"agent\". Nothing \
             was started."
        );
        assert_eq!(
            refused(request(None, vec![requested("a", Some("x"))], None)).unwrap_err(),
            "fan_out refused: items[0] names the worker \"x\", but this stage has one worker, \
             w, which takes every item.\nCall fan_out again without \"worker\" on its items. \
             Nothing was started."
        );
        assert_eq!(
            refused(request(
                Some("review"),
                vec![requested("a", Some("x"))],
                None
            ))
            .unwrap_err(),
            "fan_out refused:\n- \"agent\" is not used in this stage. It hands items only to \
             the worker its blueprint declares, w.\n- items[0] names the worker \"x\", but this \
             stage has one worker, w, which takes every item.\nCall fan_out again without \
             \"agent\" and without \"worker\" on its items. Nothing was started."
        );
    }

    /// A stage with several workers takes a mixed call, each item to the
    /// worker it names.
    #[test]
    fn several_workers_take_a_mixed_call() {
        let stage = several();
        let call = request(
            None,
            vec![
                requested("a", Some("code")),
                requested("b", Some("research")),
                requested("c", Some("docs")),
            ],
            None,
        );
        let (_, items) = settle(&call, Some(&stage), &calling(), None).unwrap();
        let picked: Vec<&str> = items.iter().map(|i| i.worker.as_str()).collect();
        assert_eq!(picked, ["code", "research", "docs"]);
    }

    /// A stage with several workers refuses an item naming a worker it does
    /// not have, an item naming none, and an `agent`, all at once, listing
    /// its workers and what each is for.
    #[test]
    fn several_workers_refuse_a_name_off_the_list() {
        let stage = several();
        let refused =
            |call: FanOutRequest| settle(&call, Some(&stage), &calling(), None).unwrap_err();
        let menu = "This stage's workers are:\n- code: Reviewing one area of the code\n- docs: \
                    Checking one docs page\n- research: Looking a question up";
        assert_eq!(
            refused(request(None, vec![requested("a", Some("reviewer"))], None)),
            format!(
                "fan_out refused: items[0] names the worker \"reviewer\", which this stage does \
                 not have.\n{menu}\nSet each item's \"worker\" to one of those names and call \
                 fan_out again. Nothing was started."
            )
        );
        assert_eq!(
            refused(request(
                Some("review"),
                vec![requested("a", Some("code")), requested("b", None)],
                None
            )),
            format!(
                "fan_out refused:\n- \"agent\" is not used in this stage, which hands each item \
                 to the worker the item names.\n- items[1] has no \"worker\". This stage has \
                 more than one worker, so each item names the one that takes it.\n{menu}\nSet \
                 each item's \"worker\" to one of those names, leave \"agent\" out, and call \
                 fan_out again. Nothing was started."
            )
        );
    }

    /// A worker whose blueprint does not load refuses the call, with advice
    /// that fits where the call came from.
    #[test]
    fn an_unloadable_worker_is_explained_where_the_call_came_from() {
        let why = "no blueprint named \"writer\" is installed".to_string();
        let items = vec![
            WorkItem {
                worker: name("docs"),
                ..item("a", &[])
            },
            WorkItem {
                worker: name("code"),
                ..item("b", &[])
            },
            WorkItem {
                worker: name("docs"),
                ..item("c", &[])
            },
        ];
        assert_eq!(
            unloadable(&several(), &items, &[(name("docs"), why.clone())], None),
            "fan_out refused: items[0], items[2] picked \"docs\", but it could not be loaded: \
             no blueprint named \"writer\" is installed\nGive those items another of this \
             stage's workers, or leave them out. Nothing was started."
        );
        assert_eq!(
            unloadable(&stage_def(0), &items, &[(name("w"), why.clone())], None),
            "fan_out refused: this stage's worker \"w\" could not be loaded: no blueprint named \
             \"writer\" is installed\nNothing was started."
        );
        assert_eq!(
            unloadable(&stage_def(0), &items, &[(name("w"), why)], Some("review")),
            "fan_out refused: the agent \"review\" could not be loaded: no blueprint named \
             \"writer\" is installed\nName an installed blueprint as \"agent\". Nothing was \
             started."
        );
    }

    /// A worker inherits its parent's unattended setting, allowed tools,
    /// model and output request, runs no shell seeds, and names the stage it
    /// enters when it runs the parent's own graph.
    #[test]
    fn a_worker_request_carries_what_its_parent_decided() {
        let mut parent = crate::spec::run_spec::tests::spec();
        parent.launch.allow = vec![crate::spec::names::ToolName::new("shell").unwrap()];
        let work = item("a", &[("topic", RawInput::Text("t".into()))]);
        let (req, caller) = worker_request(
            &parent,
            &stage_def(2).workers_available[0],
            &work,
            SpawnSource::Raw(Box::new(parent.graph.clone())),
            1,
        );
        assert_eq!(req.inputs, work.inputs);
        assert!(
            req.delivery.metadata.is_empty(),
            "the item is the host's, not a label"
        );
        assert_eq!(req.model, parent.requested_model);
        assert_eq!(req.launch.unattended, parent.launch.unattended);
        assert_eq!(req.launch.allow, parent.launch.allow);
        assert!(!req.launch.seed_commands);
        assert_eq!(
            req.workdir.as_deref(),
            Some(parent.placement.workdir.as_path())
        );
        assert_eq!(
            caller,
            Caller::Worker {
                parent: parent.run_id.clone(),
                policy: parent.launch.clone(),
                depth: 1,
                stage: Some(stage_name("w")),
                item: "a".to_string(),
            }
        );
        let (_, caller) = worker_request(
            &parent,
            &several().workers_available[2],
            &work,
            SpawnSource::Blueprint(BlueprintRef::parse("fixer").unwrap()),
            300,
        );
        assert_eq!(
            caller,
            Caller::Worker {
                parent: parent.run_id.clone(),
                policy: parent.launch.clone(),
                depth: u8::MAX,
                stage: None,
                item: "a".to_string(),
            }
        );
    }
}
