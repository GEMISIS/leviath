//! Tests for the manifest in detail.
//!
//! One manifest that uses the awkward corners, asked through the schema, so what
//! is asserted is the answer a client reads rather than a struct field. The
//! corners matter more than the plain fields: a fan-out block on a stage that
//! is not a fan-out, a gate naming a region, a seed that runs tools.

use std::sync::Arc;

use async_graphql::{EmptyMutation, EmptySubscription, Request, Schema};

use super::stage::Stage;
use crate::commands::serve::testutil::state_with_agent_paths;

/// A manifest that exercises the detail types.
pub(super) fn manifest() -> String {
    r#"
[agent]
name = "detailed"
version = "2.0.0"
description = "uses the awkward corners"
entry_stage = "plan"

[security]
taint_tracking = true

[sandbox]
kind = "container"
image = "python:3.12"
network = false
mounts = ["./data:/data"]

[agent.nudge]
max = 5

[compaction]
provider = "anthropic"
model = "claude-haiku-4-5"
max_summary_tokens = 800
temperature = 0.1

[context.file_tracking]
region = "files"
track_reads = true
track_writes = false

[repetition_detection]
max_repeat_calls = 4

[safe_commands]
tools = ["read_file"]
shell = ["cargo test"]

[agent.output]
format = "json"
instructions = "one object per finding"
validator = "checks/output.rhai"
on_validator_error = "accept"

[[agent.output.artifacts]]
name = "report"
type = "text/markdown"
required = true
description = "the write-up"

[[dependencies]]
name = "blender"
kind = "binary"
command = "blender"
remedy = "install Blender and put it on PATH"

[[dependencies]]
name = "docs"
kind = "mcp_server"
server = "docs"
env = ["DOCS_TOKEN"]

[dependencies.install]
command = "npm i -g docs-mcp"

[dependencies.install.server]
transport = "stdio"
command = "docs-mcp"
args = ["--stdio"]

[dependencies.install.server.env]
DOCS_TOKEN = "${DOCS_TOKEN}"

[mime_types."model/gltf+json"]
family = "model"
text = true
extensions = ["gltf"]
tokens = { fixed = 500 }

[context.regions.plan]
kind = "pinned"
budget = "20%"
min_tokens = 500
max_tokens = 4000
description = "the plan"
required = true
required_message = "{region} has to say something first"
volatility = "rewritten"
admission = "reject"
accepts = ["text/*"]
seed = { literal = "start here" }

[context.regions.notes]
kind = "sliding_window"
max_tokens = 1000
max_items = 20
strategy = "bulk"
overflow = 3

[context.regions.facts]
kind = "hashmap"
max_tokens = 1000
max_entries = 50

[context.regions.files]
kind = "hashmap"
max_tokens = 800
max_entries = 20

[context.regions.env]
kind = "temporary"
max_tokens = 400
seed = { tools = [{ name = "which_command", args = { command = "git" } }], refresh = "each_stage" }

[stages.plan]
mode = "interactive_points"
description = "decide"
available_tools = ["read_file", "shell"]
max_iterations = 8
transition_prompt = "pick the next step"
tool_permissions = { shell = "ask", read_file = "allow" }
tool_accepts = { spawn_agent = ["image/*"] }
output_routing = { "image/*" = "notes" }

[stages.plan.context]
hide = ["facts"]
reset = ["env"]

[stages.plan.model]
models = [{ provider = "anthropic", model = "claude-sonnet-5" }]
allow_user_default = false
parameters = { temperature = 0.2, max_output_tokens = "40%", top_p = 0.9 }
request_timeout_secs = 300

[stages.plan.tool_routing]
default_region = "notes"
keep_results = false
max_result_tokens = 4000
overrides = { shell = "env" }
max_result_tokens_per_tool = { read_file = 2000 }

[stages.plan.hooks]
on_stage_enter = "hooks/enter.rhai"

[[stages.plan.interaction_points]]
name = "review"
prompt = "Does this look right?"
style = "multiple_choice"
options = ["Approve", "Revise"]
unattended = "ask"
document_region = "plan"
directives = { Revise = "ask what to change" }

[stages.plan.transitions.build]
condition = "llm_choice"
hint = "when the plan is settled"
transform = "custom"

[stages.plan.transitions.build.transform_config]
carry = ["plan"]
clear = ["env"]
compact_prompt = "keep the decisions"

[stages.plan.transitions.build.gate]
require_modifications = true
require_regions = ["plan"]
require_region_updated = "plan"
max_attempts = 2
message = "write the plan first"

[stages.plan.transitions.stuck_out]
condition = "stuck"
stuck_after_iterations = 12

[stages.stuck_out]
mode = "autonomous"

[stages.build]
mode = "fan_out"
description = "split the work"
worker_stage = "worker"
merge_stage = "merge"
split_prompt = "one item per file"
max_workers = 4
max_items = 20
on_worker_failure = "fail_all"
results_region = "notes"

[stages.worker]
mode = "autonomous"
allow_as_worker = true

[stages.merge]
mode = "output"
"#
    .to_string()
}

/// Ask the schema about the manifest, with a server behind it so `effective`
/// has a config to resolve against.
async fn ask(query: &str) -> serde_json::Value {
    let parsed = leviath_core::manifest::parse_manifest(&manifest()).expect("the manifest parses");
    let schema = Schema::build(
        StageProbe {
            blueprint: Arc::new(parsed),
        },
        EmptyMutation,
        EmptySubscription,
    )
    .data(state_with_agent_paths(Vec::new()))
    .finish();
    let answer = schema.execute(Request::new(query)).await;
    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
    serde_json::to_value(&answer.data).expect("data serializes")
}

/// A root handing out stages by name, so one field test is one query.
struct StageProbe {
    blueprint: Arc<leviath_core::Blueprint>,
}

#[async_graphql::Object]
impl StageProbe {
    /// The stage under test.
    async fn stage(&self, name: String) -> Option<Stage> {
        let at = self.blueprint.stages.iter().position(|s| s.name == name)?;
        Some(Stage {
            blueprint: Arc::clone(&self.blueprint),
            at,
        })
    }
}

/// A stage's model block comes back whole, with the relative output cap typed
/// rather than flattened to a string.
#[tokio::test]
async fn a_stage_carries_its_model_block() {
    let json = ask(r#"{ stage(name: "plan") { model {
             models { provider model }
             allowUserDefault
             requestTimeoutSecs
             parameters {
               temperature
               providerParams
               maxOutputTokens {
                 __typename
                 ... on MaxTokensContextPercent { percent }
               }
             }
           } } }"#)
    .await;
    let model = &json["stage"]["model"];
    assert_eq!(model["models"][0]["provider"], "anthropic");
    assert_eq!(model["models"][0]["model"], "claude-sonnet-5");
    assert_eq!(model["allowUserDefault"], false);
    assert_eq!(model["requestTimeoutSecs"], 300);
    assert_eq!(model["parameters"]["temperature"], 0.2);
    assert_eq!(
        model["parameters"]["maxOutputTokens"]["__typename"],
        "MaxTokensContextPercent"
    );
    assert_eq!(model["parameters"]["maxOutputTokens"]["percent"], 40.0);
    // A parameter this schema has no field for is kept rather than dropped:
    // it is a parameter some provider understands, and losing it would make
    // the answer a worse copy of the manifest than the manifest.
    assert_eq!(model["parameters"]["providerParams"]["top_p"], 0.9);
}

/// Tool routing, permissions and the accept table all come back, each in a
/// stable order.
#[tokio::test]
async fn a_stage_carries_what_its_tools_may_do() {
    let json = ask(r#"{ stage(name: "plan") {
             toolPermissions { tool policy }
             toolAccepts { tool patterns }
             outputRouting { pattern region { name } regionName }
             toolRouting {
               defaultRegion { name } defaultRegionName keepResults maxResultTokens
               overrides { tool region { name } regionName }
               maxResultTokensPerTool { tool maxResultTokens }
             }
           } }"#)
    .await;
    let stage = &json["stage"];
    // Sorted by tool, because the manifest's own table is a hash map and an
    // unsorted answer would reorder between two reads of one blueprint.
    assert_eq!(stage["toolPermissions"][0]["tool"], "read_file");
    assert_eq!(stage["toolPermissions"][0]["policy"], "ALLOW");
    assert_eq!(stage["toolPermissions"][1]["tool"], "shell");
    assert_eq!(stage["toolPermissions"][1]["policy"], "ASK");
    assert_eq!(stage["toolAccepts"][0]["tool"], "spawn_agent");
    assert_eq!(stage["toolAccepts"][0]["patterns"][0], "image/*");
    assert_eq!(stage["outputRouting"][0]["pattern"], "image/*");
    assert_eq!(stage["outputRouting"][0]["region"]["name"], "notes");
    assert_eq!(stage["outputRouting"][0]["regionName"], "notes");
    let routing = &stage["toolRouting"];
    assert_eq!(routing["defaultRegion"]["name"], "notes");
    assert_eq!(routing["defaultRegionName"], "notes");
    assert_eq!(routing["keepResults"], false);
    assert_eq!(routing["maxResultTokens"], 4000);
    assert_eq!(routing["overrides"][0]["tool"], "shell");
    assert_eq!(routing["maxResultTokensPerTool"][0]["tool"], "read_file");
    assert_eq!(
        routing["maxResultTokensPerTool"][0]["maxResultTokens"],
        2000
    );
}

/// An edge carries its condition, its transform and its gate, and a stuck edge
/// carries what arms it.
///
/// A `STUCK` edge with no thresholds could never fire, so a client that shows
/// the condition and not the numbers shows an edge that looks unconditional.
#[tokio::test]
async fn an_edge_carries_its_condition_transform_and_gate() {
    let json = ask(r#"{ stage(name: "plan") { transitions {
             target { name } targetName condition transform
             transformConfig { carry { name } carryNames compact { name } compactNames
                              clear { name } clearNames compactPrompt }
             gate {
               requireModifications requireRegions { name } requireRegionNames
               requireRegionUpdated { name } requireRegionUpdatedName
               maxAttempts message
             }
             stuck { afterIterations afterMinutes }
           } } }"#)
    .await;
    let edges = json["stage"]["transitions"].as_array().expect("edges");
    assert_eq!(edges.len(), 2);
    // Sorted by target: `build` before `stuck_out`.
    let build = &edges[0];
    assert_eq!(build["target"]["name"], "build");
    assert_eq!(build["targetName"], "build");
    assert_eq!(build["condition"], "LLM_CHOICE");
    assert_eq!(build["transform"], "CUSTOM");
    assert_eq!(build["transformConfig"]["carry"][0]["name"], "plan");
    assert_eq!(build["transformConfig"]["carryNames"][0], "plan");
    assert_eq!(build["transformConfig"]["clear"][0]["name"], "env");
    assert_eq!(build["transformConfig"]["clearNames"][0], "env");
    assert_eq!(
        build["transformConfig"]["compactPrompt"],
        "keep the decisions"
    );
    assert_eq!(build["gate"]["requireModifications"], true);
    assert_eq!(build["gate"]["requireRegions"][0]["name"], "plan");
    assert_eq!(build["gate"]["requireRegionNames"][0], "plan");
    assert_eq!(build["gate"]["requireRegionUpdated"]["name"], "plan");
    assert_eq!(build["gate"]["requireRegionUpdatedName"], "plan");
    assert_eq!(build["gate"]["maxAttempts"], 2);
    assert!(build["stuck"].is_null(), "not a stuck edge");

    let stuck = &edges[1];
    assert_eq!(stuck["condition"], "STUCK");
    assert_eq!(stuck["stuck"]["afterIterations"], 12);
    assert!(stuck["stuck"]["afterMinutes"].is_null());
    assert!(stuck["gate"].is_null(), "no gate on this one");
}

/// Checkpoints come back with their options and what each one does.
#[tokio::test]
async fn a_stage_carries_its_checkpoints() {
    let json = ask(r#"{ stage(name: "plan") { interactionPoints {
             name prompt required style options unattended
             documentRegion { name } documentRegionName
             directives { option instruction }
           } } }"#)
    .await;
    let point = &json["stage"]["interactionPoints"][0];
    assert_eq!(point["name"], "review");
    assert_eq!(point["style"], "MULTIPLE_CHOICE");
    assert_eq!(point["unattended"], "ASK");
    assert_eq!(point["documentRegion"]["name"], "plan");
    assert_eq!(point["documentRegionName"], "plan");
    assert_eq!(point["options"][1], "Revise");
    assert_eq!(point["directives"][0]["option"], "Revise");
    assert_eq!(point["directives"][0]["instruction"], "ask what to change");
}

/// Fan-out is on the stage that fans out, and null on the stages that do not.
///
/// A block of defaults on an autonomous stage would read as a fan-out nobody
/// wrote, which is the shape a flattened stage type cannot avoid.
#[tokio::test]
async fn fan_out_is_null_on_a_stage_that_does_not_fan_out() {
    let json = ask(r#"{
             build: stage(name: "build") { mode fanOut {
               workerStage { name } workerStageName
               mergeStage { name } mergeStageName
               splitPrompt maxWorkers maxItems
               onWorkerFailure resultsRegion { name } resultsRegionName
             } }
             plan: stage(name: "plan") { fanOut { splitPrompt } interactionPoints { name } }
             worker: stage(name: "worker") { fanOut { splitPrompt } interactionPoints { name } }
           }"#)
    .await;
    let fan = &json["build"]["fanOut"];
    assert_eq!(json["build"]["mode"], "FAN_OUT");
    assert_eq!(fan["workerStage"]["name"], "worker");
    assert_eq!(fan["workerStageName"], "worker");
    assert_eq!(fan["mergeStage"]["name"], "merge");
    assert_eq!(fan["maxWorkers"], 4);
    assert_eq!(fan["maxItems"], 20);
    assert_eq!(fan["onWorkerFailure"], "FAIL_ALL");
    assert_eq!(fan["resultsRegion"]["name"], "notes");
    assert_eq!(fan["resultsRegionName"], "notes");
    assert!(json["plan"]["fanOut"].is_null());
    assert!(json["worker"]["fanOut"].is_null());
    // And the mirror image: checkpoints belong to the stage that raises them.
    assert_eq!(
        json["worker"]["interactionPoints"].as_array().map(Vec::len),
        Some(0)
    );
}

/// The declared settings and the resolved ones are different answers, and both
/// are available.
#[tokio::test]
async fn declared_and_effective_settings_are_both_answered() {
    let json = ask(r#"{ stage(name: "plan") {
             nudge { policy max text }
             sandbox { kind }
             security { taintTracking }
             hooks { onStageEnter onStageExit }
             effective {
               includesBatchHint shellHintEligible tracksTaint
               nudge { nudges max text }
               sandbox { kind image allowNetwork mounts keepWarm onUnavailable }
             }
           } }"#)
    .await;
    let stage = &json["stage"];
    // The stage declares none of these, so every declared field is null: what
    // the author wrote and what the daemon resolved are different questions.
    assert!(stage["nudge"].is_null(), "the stage declares no nudge");
    assert!(stage["sandbox"].is_null(), "nor a sandbox");
    assert!(stage["security"].is_null(), "nor a security block");
    assert_eq!(stage["hooks"]["onStageEnter"], "hooks/enter.rhai");
    assert!(stage["hooks"]["onStageExit"].is_null());

    let effective = &stage["effective"];
    // The blueprint's own blocks are what these resolve from.
    assert_eq!(effective["sandbox"]["kind"], "CONTAINER");
    assert_eq!(effective["sandbox"]["image"], "python:3.12");
    assert_eq!(effective["sandbox"]["allowNetwork"], false);
    assert_eq!(effective["tracksTaint"], true);
    assert_eq!(effective["nudge"]["max"], 5, "from the agent block");
    // A stage with checkpoints is not nudged: its text is its work product.
    assert_eq!(effective["nudge"]["nudges"], false);
    assert!(
        effective["nudge"]["text"]
            .as_str()
            .is_some_and(|text| !text.is_empty()),
        "the default nudge text is resolved, not left null"
    );
}

/// The nudge default flips for a stage with no checkpoints, which is the one
/// place the resolution depends on the stage's own shape.
#[tokio::test]
async fn an_ordinary_stage_is_nudged_where_a_reviewed_one_is_not() {
    let json = ask(r#"{
             plan: stage(name: "plan") { effective { nudge { nudges } } }
             worker: stage(name: "worker") { effective { nudge { nudges } } }
           }"#)
    .await;
    assert_eq!(json["plan"]["effective"]["nudge"]["nudges"], false);
    assert_eq!(json["worker"]["effective"]["nudge"]["nudges"], true);
}

/// The context block says which regions a stage adds, hides and empties.
#[tokio::test]
async fn a_stage_says_what_it_does_to_the_context() {
    let json = ask(
        r#"{ stage(name: "plan") { context { regions { name } hide { name } hideNames
         reset { name } resetNames } input { accepts asText } } }"#,
    )
    .await;
    let context = &json["stage"]["context"];
    assert_eq!(context["hide"][0]["name"], "facts");
    assert_eq!(context["hideNames"][0], "facts");
    assert_eq!(context["reset"][0]["name"], "env");
    assert_eq!(context["resetNames"][0], "env");
    assert_eq!(
        json["stage"]["input"]["accepts"].as_array().map(Vec::len),
        Some(0),
        "the stage says nothing, so its regions decide"
    );
}

/// A stage that declares regions of its own says which they are.
///
/// A stage-local region is part of the layout that stage runs with and of no
/// other, so a client rendering one stage's context needs the names from the
/// stage rather than from the blueprint.
#[tokio::test]
async fn a_stage_can_declare_regions_of_its_own() {
    let text = r#"
[agent]
name = "local-regions"
version = "1.0.0"
description = "a stage with its own regions"

[context.regions.shared]
kind = "pinned"
max_tokens = 100

[stages.only]
mode = "autonomous"

[stages.only.context.regions.scratch]
kind = "temporary"
max_tokens = 50

[stages.only.context]
hide = ["shared"]
"#;
    let parsed = leviath_core::manifest::parse_manifest(text).expect("the manifest parses");
    let schema = Schema::build(
        StageProbe {
            blueprint: Arc::new(parsed),
        },
        EmptyMutation,
        EmptySubscription,
    )
    .data(state_with_agent_paths(Vec::new()))
    .finish();
    let answer = schema
        .execute(Request::new(
            r#"{ stage(name: "only") { context { regions { name declaredByStage { name } }
           hide { name } hideNames reset { name } resetNames } } }"#,
        ))
        .await;
    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
    let json = serde_json::to_value(&answer.data).expect("data serializes");
    let context = &json["stage"]["context"];
    assert_eq!(context["regions"][0]["name"], "scratch");
    assert_eq!(
        context["regions"][0]["declaredByStage"]["name"], "only",
        "a region a stage declares says which stage declared it"
    );
    assert_eq!(context["hideNames"][0], "shared");
    assert_eq!(context["reset"].as_array().map(Vec::len), Some(0));
}
