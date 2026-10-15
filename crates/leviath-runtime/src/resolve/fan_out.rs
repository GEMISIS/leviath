//! A fan-out stage's `fan_out` tool, shaped to the workers its blueprint
//! declares.
//!
//! Everywhere else the tool is the same: an ordinary stage names the blueprint
//! every item runs as `agent`. A fan-out stage hands items only to the
//! workers it declares, so its copy of the tool has no `agent`. When it
//! declares more than one, each item carries a required `worker`, limited to
//! their names and described by what each is for and the inputs it takes. The
//! schema steers the model; the runtime refuses any other name, whatever a
//! provider lets through.

use std::collections::BTreeMap;

use leviath_core::JsonDoc;
use leviath_core::stage_tools::FAN_OUT_TOOL;

use crate::spec::graph::{FanOutDef, RunGraph, StageMode};
use crate::spec::names::{StageName, WorkerName};
use crate::spec::run_spec::{StagePlan, ToolDef};

/// The inputs each declared worker takes, by its stage and its name, as
/// resolving found them. A worker not here takes inputs this resolve could
/// not read, and its line in the description says nothing about them.
pub(super) type WorkerInputs = BTreeMap<(StageName, WorkerName), Vec<String>>;

/// Shape the `fan_out` tool of each fan-out stage in `plans` to the workers
/// its stage in `graph` declares.
pub(super) fn shape(plans: &mut [StagePlan], graph: &RunGraph, inputs: &WorkerInputs) {
    for plan in plans.iter_mut() {
        let Some(StageMode::FanOut(fan)) = graph.stage(plan.stage.as_str()).map(|s| &s.mode) else {
            continue;
        };
        for tool in plan
            .tools
            .iter_mut()
            .filter(|t| t.name.as_str() == FAN_OUT_TOOL)
        {
            shape_tool(tool, fan, &plan.stage, inputs);
        }
    }
}

/// `tool` without `agent`, and with several workers, a required `worker` on
/// each item.
fn shape_tool(tool: &mut ToolDef, fan: &FanOutDef, stage: &StageName, inputs: &WorkerInputs) {
    let mut schema = tool.schema.value().clone();
    if let Some(properties) = schema["properties"].as_object_mut() {
        properties.remove("agent");
    }
    if fan.sole_worker().is_none() {
        let item = &mut schema["properties"]["items"]["items"];
        item["properties"]["worker"] = serde_json::json!({
            "type": "string",
            "enum": fan.workers_available.iter().map(|w| w.name.as_str()).collect::<Vec<_>>(),
            "description": menu(fan, stage, inputs),
        });
        if let Some(required) = item["required"].as_array_mut() {
            required.push("worker".into());
        }
    }
    tool.schema = JsonDoc::new(schema);
}

/// What the model reads about each worker: when to pick it, and the inputs it
/// takes when they are known.
fn menu(fan: &FanOutDef, stage: &StageName, inputs: &WorkerInputs) -> String {
    let lines: Vec<String> = fan
        .workers_available
        .iter()
        .map(|w| {
            let takes = inputs
                .get(&(stage.clone(), w.name.clone()))
                .map(|names| format!(" Inputs: {}", names.join(", ")))
                .unwrap_or_default();
            format!("- {}: {}.{takes}", w.name, w.use_for.trim_end_matches('.'))
        })
        .collect();
    format!(
        "Which of this stage's workers takes this item:\n{}",
        lines.join("\n")
    )
}

#[cfg(test)]
#[path = "fan_out_tests.rs"]
mod tests;
