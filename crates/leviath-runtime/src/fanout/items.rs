//! What a fan-out hands each worker, and the request that starts one.
//!
//! A `fan_out` call lists work items. Each carries typed inputs for the graph
//! its worker runs, and a same-graph worker's items are checked against that
//! graph's declared inputs before any worker starts, so a mistyped item is
//! refused with the path of the value that did not fit. A worker running a
//! blueprint of its own has its inputs checked when that blueprint is resolved.
//!
//! Kept apart from [`super`], which starts, tracks and merges workers: this
//! is the shape of the work and the request each worker is spawned from.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::spec::env::Caller;
use crate::spec::graph::{FanOutDef, WorkerFailure, WorkerSource};
use crate::spec::inputs::{CheckCtx, InputDecl, RawInput};
use crate::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
use crate::spec::launch::LaunchRequest;
use crate::spec::request::{SpawnRequest, SpawnSource};
use crate::spec::run_spec::RunSpec;
use std::path::Path;

/// The input a worker's work item fills when a graph declares none of its
/// own: the conventional `task` text.
pub(crate) const TASK_INPUT: &str = "task";

/// The most workers at once for a `fan_out` called outside a fan-out stage,
/// which has no `max_workers` of its own: the same default a stage gets.
const CALLED_MAX_WORKERS: u32 = crate::spec::graph::stage::DEFAULT_MAX_WORKERS;

/// The label a worker's request carries its work item's id under, so the
/// host can name the worker after the item it runs.
pub const WORK_ITEM_LABEL: &str = "fan_out_item";

/// One unit of work produced by a fan-out call.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct WorkItem {
    /// Stable id (used to label the worker in the consolidated report).
    #[serde(default)]
    pub id: String,
    /// The worker's inputs, by the name its graph declares them under.
    #[serde(default)]
    pub inputs: BTreeMap<String, RawInput>,
}

/// A work item as a `fan_out` call writes it: an id, and the worker's inputs
/// by name. Nothing else is accepted, so a misspelled key is refused rather
/// than dropped.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ItemArgs {
    id: String,
    #[serde(default)]
    inputs: BTreeMap<String, RawInput>,
}

/// A `fan_out` call the dispatcher has read but not yet started.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FanOutRequest {
    /// The blueprint to run for every item, when the caller named one: an
    /// installed name, or the absolute directory of one that is not installed.
    /// `None` inside a fan-out stage, whose graph names the worker instead.
    pub agent: Option<String>,
    /// The work, one entry per worker.
    pub items: Vec<WorkItem>,
    /// A per-call concurrency cap, when the caller asked for one.
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
    let items: Vec<WorkItem> = items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            serde_json::from_value::<ItemArgs>(item.clone())
                .map(|item| WorkItem {
                    id: item.id,
                    inputs: item.inputs,
                })
                .map_err(|e| format!("fan_out items[{i}] is not {{id, inputs}}: {e}"))
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
    Ok(FanOutRequest {
        agent,
        items,
        max_workers,
    })
}

/// Turn a request into the fan-out it runs as.
///
/// A fan-out stage's own settings are the starting point when there are any;
/// a call from an ordinary stage has none, so it takes the engine defaults and
/// must name its worker in the call. Either way the result is one
/// [`FanOutDef`], so the cap, the failure policy and the report are the same
/// code for both entry points.
///
/// An `agent` the call names by its directory must be absolute, and may not
/// be inside `workdir`: the run's own files are ones its model could have
/// written, and a worker blueprint brings its own seeds and MCP servers.
pub(crate) fn config_for(
    request: &FanOutRequest,
    stage: Option<&FanOutDef>,
    workdir: Option<&Path>,
) -> Result<FanOutDef, String> {
    // A named agent wins over the graph's worker: an ordinary stage has no
    // worker to inherit, and a fan-out stage that names one in the call meant it.
    let named = match &request.agent {
        Some(agent) => Some(named_worker(agent, workdir)?),
        None => None,
    };
    let mut config = match (stage, named) {
        (Some(stage), named) => FanOutDef {
            worker: named.unwrap_or_else(|| stage.worker.clone()),
            ..stage.clone()
        },
        (None, Some(worker)) => FanOutDef {
            worker,
            merge_stage: None,
            max_workers: CALLED_MAX_WORKERS,
            on_worker_failure: WorkerFailure::Continue,
            split_prompt: String::new(),
            results_region: None,
            max_items: None,
            max_attempts: None,
        },
        (None, None) => {
            return Err(
                "fan_out needs an `agent` to run for each item when it is called outside a \
                 fan_out stage"
                    .to_string(),
            );
        }
    };
    if let Some(max_workers) = request.max_workers {
        config.max_workers = clamp(max_workers);
    }
    Ok(config)
}

/// The worker a `fan_out` call's `agent` names. See [`config_for`].
fn named_worker(agent: &str, workdir: Option<&Path>) -> Result<WorkerSource, String> {
    let worker = WorkerSource::named(agent).map_err(|e| {
        format!(
            "fan_out `agent`: {e}. Name an installed blueprint, or give the absolute \
             directory of one"
        )
    })?;
    if let (WorkerSource::BlueprintFile(path), Some(workdir)) = (&worker, workdir)
        && leviath_core::resolves_within(path.path(), workdir)
    {
        return Err(format!(
            "fan_out `agent`: '{agent}' is inside this run's own working directory. Name an \
             installed blueprint, or one outside the workspace: an agent may not author the \
             blueprint its workers run"
        ));
    }
    Ok(worker)
}

/// A count as the graph stores it.
fn clamp(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The most workers a fan-out runs at once; `None` when it sets no limit
/// (`max_workers = 0`).
pub(crate) fn worker_cap(config: &FanOutDef) -> Option<usize> {
    (config.max_workers > 0).then_some(config.max_workers as usize)
}

/// Check every item's inputs against the worker graph's declarations.
///
/// Reports every problem at once, each at its item's path
/// (`items[2].inputs.topic`): a value of the wrong type, and a name the graph
/// does not declare. The conventional `task` text is always accepted, as the
/// work item's own description. A required input is not demanded here: a
/// same-graph worker enters partway through a run whose caller already
/// supplied it.
pub(crate) fn check_items(decls: &[InputDecl], items: &[WorkItem]) -> Result<(), SpawnIssues> {
    let mut issues = SpawnIssues::new();
    let cx = CheckCtx::default();
    for (i, item) in items.iter().enumerate() {
        let at = SpecPath::root().field("items").index(i).field("inputs");
        for (name, raw) in &item.inputs {
            let path = at.key(name);
            match decls.iter().find(|d| d.name.as_str() == name) {
                Some(decl) => {
                    let _ = decl.ty.check(raw, &path, &cx, &mut issues);
                }
                None if name == TASK_INPUT => {}
                None => issues.push(
                    SpawnIssue::new(
                        path,
                        IssueCode::Unknown,
                        "the worker declares no such input",
                    )
                    .known(decls.iter().map(|d| d.name.as_str()).chain([TASK_INPUT])),
                ),
            }
        }
    }
    issues.into_result(())
}

/// The request and caller that start one worker of `parent`'s fan-out.
///
/// `source` is what the worker runs, already resolved from the fan-out's
/// [`WorkerSource`]. The worker inherits how unattended its parent is and the
/// tools it may call without asking, runs no spawn-time shell seeds (its parent already scoped the work, so every
/// worker re-running them is waste), and gets the parent's requested model
/// and output shape.
pub(crate) fn worker_request(
    parent: &RunSpec,
    config: &FanOutDef,
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
        delivery: crate::spec::launch::Delivery {
            callback: None,
            metadata: BTreeMap::from([(WORK_ITEM_LABEL.to_string(), item.id.clone())]),
        },
        ..SpawnRequest::new(source)
    };
    let caller = Caller::Worker {
        parent: parent.run_id.clone(),
        policy: parent.launch.clone(),
        depth: u8::try_from(depth).unwrap_or(u8::MAX),
        stage: match &config.worker {
            WorkerSource::Stage(stage) => Some(stage.clone()),
            WorkerSource::Blueprint(_)
            | WorkerSource::BlueprintFile(_)
            | WorkerSource::Query(_) => None,
        },
    };
    (request, caller)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::inputs::InputType;
    use crate::spec::names::{BlueprintRef, InputName, StageName};

    fn stage_def(max_workers: u32) -> FanOutDef {
        FanOutDef {
            worker: WorkerSource::Stage(StageName::new("w").unwrap()),
            merge_stage: Some(StageName::new("merge").unwrap()),
            max_workers,
            on_worker_failure: WorkerFailure::FailAll,
            split_prompt: "split".into(),
            results_region: None,
            max_items: Some(5),
            max_attempts: None,
        }
    }

    fn request(agent: Option<&str>, max_workers: Option<usize>) -> FanOutRequest {
        FanOutRequest {
            agent: agent.map(str::to_string),
            items: vec![],
            max_workers,
        }
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
            inputs: inputs
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
        }
    }

    /// Typed inputs travel as the call wrote them. A `context` is no longer
    /// read as the worker's task text: it is an unknown key like any other,
    /// and an item with no id is refused at its index.
    #[test]
    fn items_carry_typed_inputs_and_nothing_else() {
        let request = parse_fan_out_call(&serde_json::json!({
            "items": [
                {"id": "a", "inputs": {"topic": "rust", "depth": 2}},
                {"id": "c"}
            ]
        }))
        .unwrap();
        assert_eq!(request.items[0].inputs["depth"], RawInput::Int(2));
        assert!(request.items[1].inputs.is_empty());
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
        let config = config_for(&request(Some(&outside), None), None, Some(work.path())).unwrap();
        assert_eq!(
            config.worker,
            WorkerSource::BlueprintFile(
                crate::spec::names::BlueprintPath::new(outside.as_str()).unwrap()
            )
        );
        let inside = work.path().join("x").to_string_lossy().into_owned();
        let err = config_for(&request(Some(&inside), None), None, Some(work.path())).unwrap_err();
        assert!(err.contains("own working directory"), "{err}");
        // With no workspace to compare against, the directory is taken as named.
        assert!(config_for(&request(Some(&inside), None), None, None).is_ok());
        let err = config_for(&request(Some("./x"), None), None, None).unwrap_err();
        assert!(err.contains("absolute directory"), "{err}");
    }

    /// A mistyped item is refused at its own path, every problem at once, and
    /// an input the worker does not declare names the ones it does.
    #[test]
    fn a_mistyped_item_is_refused_at_its_path() {
        let decls = vec![
            decl(
                "topic",
                InputType::Text {
                    multiline: false,
                    min_len: None,
                    max_len: None,
                },
            ),
            decl(
                "depth",
                InputType::Int {
                    min: Some(1),
                    max: Some(3),
                },
            ),
        ];
        let items = vec![
            item("a", &[("topic", RawInput::Text("rust".into()))]),
            item("b", &[("task", RawInput::Text("free text".into()))]),
            item(
                "c",
                &[
                    ("topic", RawInput::Int(4)),
                    ("depth", RawInput::Int(9)),
                    ("colour", RawInput::Bool(true)),
                ],
            ),
        ];
        let issues = check_items(&decls, &items).unwrap_err();
        let lines: Vec<String> = issues
            .iter()
            .map(|i| format!("{} {:?}", i.path, i.code))
            .collect();
        assert_eq!(
            lines,
            vec![
                "items[2].inputs.colour Unknown",
                "items[2].inputs.depth OutOfRange",
                "items[2].inputs.topic WrongType",
            ]
        );
        assert_eq!(
            issues.iter().next().unwrap().known,
            vec!["topic", "depth", "task"]
        );
        assert!(check_items(&decls, &items[..2]).is_ok());
    }

    #[test]
    fn a_bare_call_names_its_worker_and_takes_the_defaults() {
        let config = config_for(&request(Some("researcher"), None), None, None).unwrap();
        assert_eq!(
            config.worker,
            WorkerSource::Blueprint(BlueprintRef::parse("researcher").unwrap())
        );
        assert_eq!(config.max_workers, CALLED_MAX_WORKERS);
        assert_eq!(config.on_worker_failure, WorkerFailure::Continue);
        assert_eq!(config.max_items, None);
        let err = config_for(&request(None, None), None, None).unwrap_err();
        assert!(err.contains("needs an `agent`"), "{err}");
        let err = config_for(&request(Some("x@nothex"), None), None, None).unwrap_err();
        assert!(err.contains("fan_out `agent`"), "{err}");
    }

    #[test]
    fn a_stage_call_keeps_the_stages_settings_under_the_calls_own() {
        let stage = stage_def(3);
        let config = config_for(&request(None, None), Some(&stage), None).unwrap();
        assert_eq!(config, stage);
        let config = config_for(&request(Some("other"), Some(9)), Some(&stage), None).unwrap();
        assert_eq!(
            config.worker,
            WorkerSource::Blueprint(BlueprintRef::parse("other").unwrap())
        );
        assert_eq!(config.max_workers, 9);
        assert_eq!(config.max_items, Some(5));
        assert_eq!(clamp(usize::MAX), u32::MAX);
        assert_eq!(worker_cap(&stage_def(0)), None);
        assert_eq!(worker_cap(&stage), Some(3));
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
            &stage_def(2),
            &work,
            SpawnSource::Raw(Box::new(parent.graph.clone())),
            1,
        );
        assert_eq!(req.inputs, work.inputs);
        assert_eq!(req.delivery.metadata[WORK_ITEM_LABEL], "a");
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
                stage: Some(StageName::new("w").unwrap()),
            }
        );
        let mut named = stage_def(2);
        named.worker = WorkerSource::Query("fixer".into());
        let (_, caller) = worker_request(
            &parent,
            &named,
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
            }
        );
    }
}
