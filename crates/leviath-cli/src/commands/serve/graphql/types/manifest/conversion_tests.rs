//! The arms one blueprint cannot take.
//!
//! Every enum here is a translation of one the graph owns, and a blueprint
//! exercises a few arms at a time: a region is sliding or compacting, never
//! both, and a sandbox is one kind. These go at the conversions directly, so
//! each arm is asserted once rather than a blueprint being contorted into
//! reaching it.
//!
//! The point is not the mapping, which is obvious, but that it is total: a
//! daemon that gains a state has to name it here, and an arm that quietly
//! reported the wrong word would read as the feature not working.

use std::sync::Arc;

use super::super::blueprint::{Blueprint, BlueprintSource, HintSetting};
use super::interaction::{InteractionPointStyle, UnattendedPolicy};
use super::model::MaxOutputTokens;
use super::output::ValidatorErrorPolicy;
use super::region::{
    RegionAdmission, RegionEviction, RegionStrategy, RegionVolatility, SeedRefresh,
};
use super::runtime::{
    BlueprintSecurity, NudgePolicy, SandboxKind, SandboxUnavailable, TaintTracking,
    WorkerFailurePolicy,
};
use super::tools::{ToolPermissionPolicy, ToolPermissionRule};
use super::transition::{MappingTransform, TransitionCondition, TransitionTransform};

/// A root handing out one parsed blueprint, so the types that resolve a name
/// against their blueprint can be asked for through the schema that serves them.
struct Probe {
    /// The blueprint under test.
    blueprint: Arc<crate::commands::serve::core::blueprints::ParsedBlueprint>,
}

#[async_graphql::Object]
impl Probe {
    /// The blueprint under test.
    async fn blueprint(&self) -> Blueprint {
        Blueprint {
            parsed: Arc::clone(&self.blueprint),
            digest: "0".repeat(64),
            source: BlueprintSource::Installed,
        }
    }
}

/// Ask the schema about one blueprint.
async fn ask(manifest: &str, query: &str) -> serde_json::Value {
    use async_graphql::{EmptyMutation, EmptySubscription, Request, Schema};

    let schema = Schema::build(
        Probe {
            blueprint: super::parsed(manifest),
        },
        EmptyMutation,
        EmptySubscription,
    )
    .data(crate::commands::serve::testutil::state_with_agent_paths(
        Vec::new(),
    ))
    .finish();
    let answer = schema.execute(Request::new(query)).await;
    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
    serde_json::to_value(&answer.data).expect("data serializes")
}

/// Every condition an edge can carry has its own value.
#[test]
fn every_transition_condition_is_translated() {
    use leviath_runtime::spec::graph::EdgeCondition as Core;

    let cases = [
        (Core::Always, TransitionCondition::Always),
        (Core::LlmChoice, TransitionCondition::LlmChoice),
        (Core::Error, TransitionCondition::Error),
        (Core::MaxIterations, TransitionCondition::MaxIterations),
        (Core::Stuck, TransitionCondition::Stuck),
        (Core::DeadEnd, TransitionCondition::DeadEnd),
    ];
    for (core, expected) in cases {
        assert_eq!(TransitionCondition::from(&core), expected, "{core:?}");
    }
}

/// And every transform, including the two that carry nothing.
#[test]
fn every_edge_transform_is_translated() {
    use leviath_runtime::spec::graph::EdgeCarry as Core;

    assert_eq!(
        TransitionTransform::from(&Core::Direct),
        TransitionTransform::Direct
    );
    assert_eq!(
        TransitionTransform::from(&Core::Clear),
        TransitionTransform::Clear
    );
    assert_eq!(
        TransitionTransform::from(&Core::Compact { prompt: None }),
        TransitionTransform::Compact
    );
    assert_eq!(
        TransitionTransform::from(&Core::Custom {
            carry: Vec::new(),
            compact: Vec::new(),
            clear: Vec::new(),
            compact_prompt: None,
        }),
        TransitionTransform::Custom
    );
}

/// The graph of a blueprint with one gate that sets every requirement, and
/// one region a later edit would have removed, with `stages` as its stages.
///
/// `stray` is named by the gate and declared nowhere, which is the case the
/// resolved fields and their `…Name` twins exist to tell apart.
fn gated_with(stages: &str) -> String {
    format!(
        r#"[blueprint]
name = "gated"
version = "1.0.0"
description = "one gate, every requirement"

[graph]
stages = {stages}

[[graph.edges]]
name = "build"
from = "plan"
to = "build"

[graph.edges.gate]
require_modifications = true
region = "plan"
tools = ["write_note"]
require_regions = ["plan", "notes", "stray"]
require_region_updated = "plan"
require_no_open_items = "todo"
require_region_entries = {{ region = "views", at_least = 4 }}
message = "write it first"
max_attempts = 2

[graph.layout]
total_budget_tokens = 4000
regions = [
    {{ name = "plan", kind = "pinned", budget = 1000 }},
    {{ name = "notes", kind = "temporary", budget = 1000 }},
    {{ name = "views", kind = "temporary", budget = 1000 }},
    {{ name = "todo", kind = "checklist", budget = 1000 }},
]
"#
    )
}

/// [`gated_with`] over two plain stages.
fn gated_manifest() -> String {
    gated_with(r#"[{ name = "plan", max_revisits = 2 }, { name = "build" }]"#)
}

/// A gate carries every requirement it was given, including the ones a manifest
/// rarely sets together, with each region resolved to the region it names.
#[tokio::test]
async fn a_gate_carries_every_requirement() {
    let json = ask(
        &gated_manifest(),
        r#"{ blueprint { stages { name transitions {
               targetName
               gate {
                 requireModifications
                 region { name } regionName
                 tools
                 requireRegions { name } requireRegionNames
                 requireRegionUpdated { name } requireRegionUpdatedName
                 requireNoOpenItems { name } requireNoOpenItemsName
                 requireRegionEntries { region { name } regionName atLeast }
                 message maxAttempts
               }
             } } } }"#,
    )
    .await;
    let gate = &json["blueprint"]["stages"][0]["transitions"][0]["gate"];
    assert_eq!(gate["requireModifications"], true);
    assert_eq!(gate["region"]["name"], "plan");
    assert_eq!(gate["regionName"], "plan");
    assert_eq!(gate["tools"][0], "write_note");
    let required: Vec<&str> = gate["requireRegions"]
        .as_array()
        .expect("a list")
        .iter()
        .map(|region| region["name"].as_str().expect("a name"))
        .collect();
    assert_eq!(
        required,
        vec!["plan", "notes"],
        "only the regions a layout declares"
    );
    assert_eq!(
        gate["requireRegionNames"],
        serde_json::json!(["plan", "notes", "stray"]),
        "and every name the gate wrote, so the stray one is still readable"
    );
    assert_eq!(gate["requireRegionUpdated"]["name"], "plan");
    assert_eq!(gate["requireRegionUpdatedName"], "plan");
    assert_eq!(gate["requireNoOpenItems"]["name"], "todo");
    assert_eq!(gate["requireNoOpenItemsName"], "todo");
    assert_eq!(gate["requireRegionEntries"]["region"]["name"], "views");
    assert_eq!(gate["requireRegionEntries"]["regionName"], "views");
    assert_eq!(gate["requireRegionEntries"]["atLeast"], 4);
    assert_eq!(gate["message"], "write it first");
    assert_eq!(gate["maxAttempts"], 2);
}

/// A gate that asks for nothing answers null for each region, and null for each
/// name beside it: the pair is what tells "asks for none" from "names one that
/// is not declared".
#[tokio::test]
async fn a_gate_that_names_no_region_answers_null_for_both_halves() {
    let manifest = gated_manifest()
        .replace("region = \"plan\"\n", "")
        .replace("require_region_updated = \"plan\"\n", "")
        .replace("require_no_open_items = \"todo\"\n", "")
        .replace(
            "require_region_entries = { region = \"views\", at_least = 4 }\n",
            "",
        );
    let json = ask(
        &manifest,
        r#"{ blueprint { stages { transitions { gate {
               region { name } regionName
               requireRegionUpdated { name } requireRegionUpdatedName
               requireNoOpenItems { name } requireNoOpenItemsName
               requireRegionEntries { regionName }
             } } } } }"#,
    )
    .await;
    let gate = &json["blueprint"]["stages"][0]["transitions"][0]["gate"];
    for field in [
        "region",
        "regionName",
        "requireRegionUpdated",
        "requireRegionUpdatedName",
        "requireNoOpenItems",
        "requireNoOpenItemsName",
        "requireRegionEntries",
    ] {
        assert!(gate[field].is_null(), "{field}: {gate}");
    }
}

/// A gate naming a region no layout declares answers null for the region and the
/// name for the twin, which is the difference a client branches on.
#[tokio::test]
async fn a_gate_naming_an_undeclared_region_keeps_the_name() {
    let manifest = gated_manifest().replace("region = \"plan\"", "region = \"stray\"");
    let json = ask(
        &manifest,
        "{ blueprint { stages { transitions { gate { region { name } regionName } } } } }",
    )
    .await;
    let gate = &json["blueprint"]["stages"][0]["transitions"][0]["gate"];
    assert!(gate["region"].is_null(), "nothing declares it: {gate}");
    assert_eq!(gate["regionName"], "stray", "and the name is still there");
}

/// A name a stage declares in its own layout resolves, and says which stage.
///
/// A region another stage set up exists, which is why the lookup walks every
/// stage's own layout after the blueprint's. `declaredByStage` is how a client
/// tells that from a region the blueprint declares run-wide.
#[tokio::test]
async fn a_region_only_a_stage_declares_still_resolves() {
    // `plan` declares a layout of its own that does not hold `stray`, so the
    // walk has to pass over a stage that has one before it reaches the stage
    // that declares it.
    let manifest = gated_with(
        r#"[
    { name = "plan", layout = { total_budget_tokens = 100, regions = [{ name = "local", kind = "temporary", budget = 100 }] } },
    { name = "build", layout = { total_budget_tokens = 200, regions = [{ name = "stray", kind = "temporary", budget = 200 }] } },
]"#,
    );
    let json = ask(
        &manifest,
        "{ blueprint { regions { name } stages { transitions { gate {
             requireRegions { name declaredByStage { name } } } } } } }",
    )
    .await;
    let run_wide: Vec<&str> = json["blueprint"]["regions"]
        .as_array()
        .expect("a list")
        .iter()
        .map(|region| region["name"].as_str().expect("a name"))
        .collect();
    assert!(
        !run_wide.contains(&"stray"),
        "a stage's own region is not in the run-wide layout: {run_wide:?}"
    );
    let required = &json["blueprint"]["stages"][0]["transitions"][0]["gate"]["requireRegions"];
    let stray = required
        .as_array()
        .expect("a list")
        .iter()
        .find(|region| region["name"] == "stray")
        .expect("the gate's third region resolves now");
    assert_eq!(stray["declaredByStage"]["name"], "build");
}

/// An `entry` naming a stage the graph does not declare answers null, with the
/// name it wrote beside it.
#[tokio::test]
async fn an_entry_stage_that_is_not_declared_answers_null_and_keeps_its_name() {
    let manifest = gated_manifest().replace("[graph]\n", "[graph]\nentry = \"nope\"\n");
    let json = ask(
        &manifest,
        "{ blueprint { entryStage { name } entryStageName } }",
    )
    .await;
    assert!(
        json["blueprint"]["entryStage"].is_null(),
        "no stage is declared under that name: {}",
        json["blueprint"]
    );
    assert_eq!(json["blueprint"]["entryStageName"], "nope");
}

/// A handoff's mappings carry each content transform, and `EXTRACT` carries the
/// fields it keeps.
#[test]
fn every_mapping_transform_is_translated() {
    use leviath_runtime::spec::graph::{ContentTransform, ContextTransformDef, RegionMappingDef};
    use leviath_runtime::spec::names::{BlueprintName, RegionName};

    let mapping = |name: &str, transform: ContentTransform| RegionMappingDef {
        from: RegionName::new(name).unwrap(),
        to: RegionName::new(name).unwrap(),
        transform,
    };
    let transform = ContextTransformDef {
        from: BlueprintName::new("a").unwrap(),
        to: BlueprintName::new("b").unwrap(),
        mappings: vec![
            mapping("one", ContentTransform::Direct),
            mapping("two", ContentTransform::Summarize),
            mapping(
                "three",
                ContentTransform::Extract(vec!["title".to_string()]),
            ),
        ],
    };
    let mapped = super::transition::ContextTransform::from(&transform);
    assert_eq!(mapped.from_blueprint, "a");
    assert_eq!(mapped.mappings[0].transform, Some(MappingTransform::Direct));
    assert_eq!(
        mapped.mappings[1].transform,
        Some(MappingTransform::Summarize)
    );
    assert_eq!(
        mapped.mappings[2].transform,
        Some(MappingTransform::Extract)
    );
    assert_eq!(mapped.mappings[2].fields, vec!["title".to_string()]);
    // The fields belong to the extract and nowhere else: a client reading them
    // off a direct mapping would be reading a list nobody wrote.
    assert!(mapped.mappings[0].fields.is_empty());
}

/// A checkpoint's two enums, both arms each.
#[test]
fn a_checkpoints_words_are_translated() {
    use leviath_runtime::spec::graph::{AnswerStyle as InteractionStyle, UnattendedPoint as Core};

    assert_eq!(
        UnattendedPolicy::from(Core::AutoApprove),
        UnattendedPolicy::AutoApprove
    );
    assert_eq!(UnattendedPolicy::from(Core::Ask), UnattendedPolicy::Ask);
    assert_eq!(
        InteractionPointStyle::from(&InteractionStyle::FreeText),
        InteractionPointStyle::FreeText
    );
    assert_eq!(
        InteractionPointStyle::from(&InteractionStyle::MultipleChoice),
        InteractionPointStyle::MultipleChoice
    );
    assert_eq!(
        InteractionPointStyle::from(&InteractionStyle::Confirm),
        InteractionPointStyle::Confirm
    );
}

/// A blueprint with two checkpoints: one bare, one with directives and a
/// document region.
fn checkpointed_manifest() -> &'static str {
    r#"[blueprint]
name = "checked"
version = "1.0.0"
description = "two checkpoints"

[graph]
layout = { total_budget_tokens = 1000, regions = [{ name = "plan", kind = "pinned", budget = 1000 }] }

[[graph.stages]]
name = "review"

[[graph.stages.mode.interactive_points]]
name = "review"
prompt = "ok?"
style = "confirm"

[[graph.stages.mode.interactive_points]]
name = "decide"
prompt = "which way?"
style = "multiple_choice"
options = ["ship", "hold"]
document_region = "plan"
directives = { ship = "go to the next stage", hold = "ask again later", abort = "stop the run" }
"#
}

/// A checkpoint with no directives carries none, rather than an empty entry, and
/// names no document region.
#[tokio::test]
async fn a_checkpoint_with_no_directives_carries_none() {
    let json = ask(
        checkpointed_manifest(),
        r#"{ blueprint { stages { interactionPoints {
               name prompt required unattended style options abortOptions editOptions
               directives { option instruction }
               documentRegion { name } documentRegionName
             } } } }"#,
    )
    .await;
    let bare = &json["blueprint"]["stages"][0]["interactionPoints"][0];
    assert_eq!(bare["name"], "review");
    assert_eq!(bare["prompt"], "ok?");
    assert_eq!(bare["style"], "CONFIRM");
    assert_eq!(bare["directives"], serde_json::json!([]));
    assert!(bare["documentRegion"].is_null());
    assert!(bare["documentRegionName"].is_null());

    let full = &json["blueprint"]["stages"][0]["interactionPoints"][1];
    assert_eq!(full["documentRegion"]["name"], "plan");
    assert_eq!(full["documentRegionName"], "plan");
    assert_eq!(full["options"], serde_json::json!(["ship", "hold"]));
}

/// Every region policy word, and the numbers each eviction strategy carries.
#[test]
fn every_region_policy_is_translated() {
    use leviath_core::region::{Admission, Volatility};
    use leviath_runtime::spec::graph::Eviction;

    assert_eq!(
        RegionVolatility::from(Volatility::Stable),
        RegionVolatility::Stable
    );
    assert_eq!(
        RegionVolatility::from(Volatility::Grows),
        RegionVolatility::Grows
    );
    assert_eq!(
        RegionVolatility::from(Volatility::Rewritten),
        RegionVolatility::Rewritten
    );
    assert_eq!(
        RegionAdmission::from(Admission::Evict),
        RegionAdmission::Evict
    );
    assert_eq!(
        RegionAdmission::from(Admission::Reject),
        RegionAdmission::Reject
    );

    let per_item = RegionEviction::from(Eviction::PerItem);
    assert_eq!(per_item.strategy, RegionStrategy::PerItem);
    assert!(per_item.overflow.is_none() && per_item.compact_count.is_none());
    let bulk = RegionEviction::from(Eviction::Bulk(3));
    assert_eq!(bulk.strategy, RegionStrategy::Bulk);
    assert_eq!(bulk.overflow, Some(3));
    let compacting = RegionEviction::from(Eviction::Compact(4));
    assert_eq!(compacting.strategy, RegionStrategy::Compact);
    assert_eq!(compacting.compact_count, Some(4));

    assert_eq!(
        SeedRefresh::from(leviath_runtime::spec::graph::SeedRefresh::Once),
        SeedRefresh::Once
    );
    assert_eq!(
        SeedRefresh::from(leviath_runtime::spec::graph::SeedRefresh::EachStage),
        SeedRefresh::EachStage
    );
}

/// The settings that cascade, each state named for its effect rather than for
/// on and off.
#[test]
fn every_cascading_setting_is_translated() {
    assert_eq!(HintSetting::from(None), HintSetting::Inherit);
    assert_eq!(HintSetting::from(Some(true)), HintSetting::Include);
    assert_eq!(HintSetting::from(Some(false)), HintSetting::Omit);

    assert_eq!(NudgePolicy::from(None), NudgePolicy::Inherit);
    assert_eq!(NudgePolicy::from(Some(true)), NudgePolicy::Nudge);
    assert_eq!(NudgePolicy::from(Some(false)), NudgePolicy::NeverNudge);

    // A graph can ask for tracking and cannot ask for less, so `false` is
    // inheritance rather than a refusal, and leaving it unset says nothing.
    let tracked = BlueprintSecurity::of(Some(true)).expect("declared");
    assert_eq!(tracked.taint_tracking, TaintTracking::Track);
    let silent = BlueprintSecurity::of(Some(false)).expect("declared");
    assert_eq!(silent.taint_tracking, TaintTracking::Inherit);
    assert!(BlueprintSecurity::of(None).is_none());
}

/// Every sandbox kind and both answers to a sandbox that cannot be built.
#[test]
fn every_sandbox_word_is_translated() {
    use leviath_core::sandbox::{OnUnavailable, SandboxKind as Core};

    assert_eq!(SandboxKind::from(Core::None), SandboxKind::None);
    assert_eq!(SandboxKind::from(Core::Namespace), SandboxKind::Namespace);
    assert_eq!(SandboxKind::from(Core::Container), SandboxKind::Container);
    assert_eq!(
        SandboxUnavailable::from(OnUnavailable::Error),
        SandboxUnavailable::Error
    );
    assert_eq!(
        SandboxUnavailable::from(OnUnavailable::Warn),
        SandboxUnavailable::Warn
    );
}

/// Both answers to a worker that failed, and both to a validator that refused.
#[test]
fn the_failure_policies_are_translated() {
    use leviath_core::output::OnValidatorError;
    use leviath_runtime::spec::graph::WorkerFailure as Core;

    assert_eq!(
        WorkerFailurePolicy::from(&Core::Continue),
        WorkerFailurePolicy::Continue
    );
    assert_eq!(
        WorkerFailurePolicy::from(&Core::FailAll),
        WorkerFailurePolicy::FailAll
    );
    assert_eq!(
        ValidatorErrorPolicy::from(OnValidatorError::Reject),
        ValidatorErrorPolicy::Reject
    );
    assert_eq!(
        ValidatorErrorPolicy::from(OnValidatorError::Accept),
        ValidatorErrorPolicy::Accept
    );
}

/// Each shape an output cap can take is its own type, with its number.
#[test]
fn every_output_cap_shape_is_translated() {
    use leviath_runtime::spec::graph::OutputCap;
    use leviath_runtime::spec::names::RegionName;

    match MaxOutputTokens::from(&OutputCap::Tokens(8_000)) {
        MaxOutputTokens::Count(count) => assert_eq!(count.tokens, 8_000),
        other => panic!("a token count is a count: {other:?}"),
    }
    match MaxOutputTokens::from(&OutputCap::WindowPercent(0.4)) {
        MaxOutputTokens::ContextPercent(share) => assert!((share.percent - 40.0).abs() < 1e-9),
        other => panic!("a window share is a share: {other:?}"),
    }
    match MaxOutputTokens::from(&OutputCap::RegionPercent {
        percent: 1.0,
        region: RegionName::new("claims").unwrap(),
    }) {
        MaxOutputTokens::RegionPercent(share) => {
            assert!((share.percent - 100.0).abs() < 1e-9);
            assert_eq!(share.region, "claims");
        }
        other => panic!("a region share names its region: {other:?}"),
    }
}

/// A permission table comes back sorted, and every rule carries its policy.
#[test]
fn a_permission_table_is_sorted_and_carries_each_policy() {
    use leviath_core::policy::ToolPolicy;
    use leviath_runtime::spec::names::ToolName;

    let table = std::collections::BTreeMap::from([
        (ToolName::new("shell").unwrap(), ToolPolicy::Deny),
        (ToolName::new("read_file").unwrap(), ToolPolicy::Allow),
        (ToolName::new("ask_user_text").unwrap(), ToolPolicy::Ask),
    ]);
    let rules = ToolPermissionRule::from_table(&table);
    let tools: Vec<&str> = rules.iter().map(|rule| rule.tool.as_str()).collect();
    assert_eq!(tools, ["ask_user_text", "read_file", "shell"]);
    assert_eq!(rules[0].policy, ToolPermissionPolicy::Ask);
    assert_eq!(rules[1].policy, ToolPermissionPolicy::Allow);
    assert_eq!(rules[2].policy, ToolPermissionPolicy::Deny);
}

/// A policy written as a word reads the way the daemon reads it: anything but
/// `allow` and `deny` is a prompt.
#[test]
fn a_policy_word_resolves_as_the_daemon_does() {
    assert_eq!(
        ToolPermissionPolicy::of(" Allow "),
        ToolPermissionPolicy::Allow
    );
    assert_eq!(ToolPermissionPolicy::of("deny"), ToolPermissionPolicy::Deny);
    assert_eq!(ToolPermissionPolicy::of("ask"), ToolPermissionPolicy::Ask);
    assert_eq!(
        ToolPermissionPolicy::of("sideways"),
        ToolPermissionPolicy::Ask
    );
}

/// Tool routing carries its overrides and ceilings, each sorted by tool, with
/// every region resolved to the region it names.
#[tokio::test]
async fn tool_routing_carries_its_tables() {
    let manifest = r#"[blueprint]
name = "routed"
version = "1.0.0"
description = "one routing block"

[graph]
layout = { total_budget_tokens = 2000, regions = [{ name = "files", kind = { kind = "keyed" }, budget = 1000 }, { name = "logs", kind = "temporary", budget = 1000 }] }

[[graph.stages]]
name = "work"

[graph.stages.tool_routing]
default_region = "logs"
keep_results = true
max_result_tokens = 4000
tool_regions = { shell = "logs", read_file = "files", grep = "gone" }
tool_max_result_tokens = { shell = 500, read_file = 2000 }
"#;
    let json = ask(
        manifest,
        r#"{ blueprint { stages { toolRouting {
               defaultRegion { name } defaultRegionName
               keepResults maxResultTokens
               overrides { tool region { name } regionName }
               maxResultTokensPerTool { tool maxResultTokens }
             } } } }"#,
    )
    .await;
    let routing = &json["blueprint"]["stages"][0]["toolRouting"];
    assert_eq!(routing["defaultRegion"]["name"], "logs");
    assert_eq!(routing["defaultRegionName"], "logs");
    assert_eq!(routing["keepResults"], true);
    assert_eq!(routing["maxResultTokens"], 4000);

    let overrides = routing["overrides"].as_array().expect("a list");
    let tools: Vec<&str> = overrides
        .iter()
        .map(|entry| entry["tool"].as_str().expect("a tool"))
        .collect();
    assert_eq!(tools, ["grep", "read_file", "shell"], "by tool, always");
    assert!(
        overrides[0]["region"].is_null(),
        "nothing declares 'gone': {}",
        overrides[0]
    );
    assert_eq!(overrides[0]["regionName"], "gone", "and the name is kept");
    assert_eq!(overrides[1]["region"]["name"], "files");

    let ceilings: Vec<&str> = routing["maxResultTokensPerTool"]
        .as_array()
        .expect("a list")
        .iter()
        .map(|entry| entry["tool"].as_str().expect("a tool"))
        .collect();
    assert_eq!(ceilings, ["read_file", "shell"]);
}

/// A shipped MCP server's transport is read where it is a word the daemon knows,
/// and left out where it is not.
#[test]
fn a_server_templates_transport_is_read_or_left_out() {
    use super::dependency::{McpServerTemplate, McpTransport};

    let template = |transport: Option<&str>| leviath_runtime::spec::graph::McpServerTemplate {
        transport: transport.map(str::to_string),
        command: Some("docs-mcp".to_string()),
        url: None,
        args: Vec::new(),
        headers: std::collections::BTreeMap::new(),
        env: std::collections::BTreeMap::new(),
    };
    assert_eq!(
        McpServerTemplate::from(&template(Some("stdio"))).transport,
        Some(McpTransport::Stdio)
    );
    assert_eq!(
        McpServerTemplate::from(&template(Some("http"))).transport,
        Some(McpTransport::Http)
    );
    // A word neither end knows is left out rather than guessed: the installer
    // infers the transport from the command or the URL anyway.
    assert!(
        McpServerTemplate::from(&template(Some("carrier-pigeon")))
            .transport
            .is_none()
    );
    assert!(McpServerTemplate::from(&template(None)).transport.is_none());
}

/// A checkpoint's directives come back in a fixed order, by option, so two
/// reads of one blueprint agree.
#[tokio::test]
async fn a_checkpoints_directives_are_ordered() {
    let json = ask(
        checkpointed_manifest(),
        "{ blueprint { stages { interactionPoints { directives { option } } } } }",
    )
    .await;
    let directives = &json["blueprint"]["stages"][0]["interactionPoints"][1]["directives"];
    let options: Vec<&str> = directives
        .as_array()
        .expect("a list")
        .iter()
        .map(|entry| entry["option"].as_str().expect("an option"))
        .collect();
    assert_eq!(options, vec!["abort", "hold", "ship"], "by option, always");
}
