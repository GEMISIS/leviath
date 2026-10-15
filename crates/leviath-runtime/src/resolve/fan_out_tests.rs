use super::*;
use crate::spec::run_spec::ToolSource;
use crate::test_graph::graph_of;

/// A graph with a fan-out stage of three workers, one of one worker, and an
/// ordinary stage, each granted `fan_out`.
fn graph() -> RunGraph {
    graph_of(
        r#"
        layout = { total_budget_tokens = 1000, regions = [] }

        [[stages]]
        name = "split"
        tools = ["fan_out"]
        [stages.mode.fan_out]
        stage = "split"
        workers_available = [
            { name = "code", stage = "area", use_for = "Reviewing one area of the code" },
            { name = "docs", blueprint = "writer", use_for = "Checking one docs page." },
            { name = "research", query = "a researcher", use_for = "Looking a question up" },
        ]

        [[stages]]
        name = "one"
        tools = ["fan_out"]
        [stages.mode.fan_out]
        stage = "one"
        workers_available = [{ name = "area", stage = "area", use_for = "Reviewing one area" }]

        [[stages]]
        name = "area"
        allow_as_worker = true

        [[stages]]
        name = "plain"
        tools = ["fan_out"]
        "#,
    )
}

/// The builtin `fan_out` tool, as a stage's plan carries it.
fn fan_out_tool() -> ToolDef {
    let tools =
        leviath_tools::BuiltinTools::new(leviath_tools::ToolContext::new(std::env::temp_dir()));
    let builtin = tools
        .tool_defs()
        .into_iter()
        .find(|t| t.name == FAN_OUT_TOOL)
        .expect("fan_out is a builtin");
    ToolDef {
        name: crate::spec::names::ToolName::new(FAN_OUT_TOOL).unwrap(),
        description: builtin.description,
        schema: JsonDoc::new(builtin.parameters),
        source: ToolSource::Builtin,
    }
}

/// One plan per stage of [`graph`], each with the `fan_out` tool.
fn plans() -> Vec<StagePlan> {
    let template = crate::spec::run_spec::tests::spec().stages[0].clone();
    ["split", "one", "area", "plain"]
        .into_iter()
        .map(|name| StagePlan {
            stage: StageName::new(name).unwrap(),
            tools: vec![fan_out_tool()],
            ..template.clone()
        })
        .collect()
}

fn schema_of(plan: &StagePlan) -> serde_json::Value {
    plan.tools[0].schema.value().clone()
}

/// A stage with several workers offers no `agent`, and each item names one
/// of the declared workers, described by what it is for and the inputs it
/// takes when they are known.
#[test]
fn several_workers_make_a_closed_worker_field_on_each_item() {
    let mut plans = plans();
    let inputs: WorkerInputs = [(
        (
            StageName::new("split").unwrap(),
            WorkerName::new("code").unwrap(),
        ),
        vec!["task".to_string(), "diff".to_string()],
    )]
    .into_iter()
    .collect();
    shape(&mut plans, &graph(), &inputs);
    let schema = schema_of(&plans[0]);
    assert!(schema["properties"].get("agent").is_none(), "{schema}");
    let item = &schema["properties"]["items"]["items"];
    assert_eq!(
        item["properties"]["worker"]["enum"],
        serde_json::json!(["code", "docs", "research"])
    );
    assert_eq!(item["required"], serde_json::json!(["id", "worker"]));
    assert_eq!(
        item["properties"]["worker"]["description"],
        "Which of this stage's workers takes this item:\n\
         - code: Reviewing one area of the code. Inputs: task, diff\n\
         - docs: Checking one docs page.\n\
         - research: Looking a question up."
    );
}

/// A stage with one worker offers neither field: every item goes to it. An
/// ordinary stage keeps `agent`, and a stage that is not granted the tool is
/// left alone.
#[test]
fn one_worker_offers_nothing_to_name_and_an_ordinary_stage_keeps_agent() {
    let mut plans = plans();
    plans[2].tools.clear();
    shape(&mut plans, &graph(), &WorkerInputs::new());
    let one = schema_of(&plans[1]);
    assert!(one["properties"].get("agent").is_none(), "{one}");
    let item = &one["properties"]["items"]["items"];
    assert!(item["properties"].get("worker").is_none(), "{one}");
    assert_eq!(item["required"], serde_json::json!(["id"]));
    let plain = schema_of(&plans[3]);
    assert!(plain["properties"].get("agent").is_some(), "{plain}");
    assert!(plans[2].tools.is_empty());
}

/// The shaped schema takes the calls the reader takes from a stage with
/// several workers, and refuses a worker it does not list.
#[test]
fn the_shaped_schema_takes_a_mixed_call_and_refuses_an_unknown_worker() {
    use leviath_tools::validate::{ArgValidation, validate_tool_args};
    let mut plans = plans();
    shape(&mut plans, &graph(), &WorkerInputs::new());
    let schema = schema_of(&plans[0]);
    let mixed = serde_json::json!({"items": [
        {"id": "a", "worker": "code", "inputs": {"task": "t"}},
        {"id": "b", "worker": "research", "inputs": {"task": "q"}}
    ]});
    assert_eq!(
        validate_tool_args("fan_out", &schema, &mixed),
        ArgValidation::Valid
    );
    let unknown = serde_json::json!({"items": [{"id": "a", "worker": "reviewer"}]});
    assert_ne!(
        validate_tool_args("fan_out", &schema, &unknown),
        ArgValidation::Valid
    );
}
