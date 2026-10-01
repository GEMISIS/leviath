//! Tests for the `Blueprint` object and the values it carries.

use std::sync::Arc;

use async_graphql::{EmptyMutation, EmptySubscription, Request, Schema};

use super::super::manifest::stage::StageMode;
use super::{Blueprint, BlueprintSource, HintSetting, RegionKind, ToolRescan};
use crate::commands::serve::core::blueprints::{BlueprintSource as CoreSource, digest_of};

/// A blueprint exercising the fields this module maps.
fn manifest() -> String {
    r#"[blueprint]
name = "coder"
version = "1.2.0"
description = "writes code"

[graph]
entry = "plan"
max_child_depth = 4
read_paths = ["~/designs"]
batch_tool_hint = false
tool_rescan = "before_dispatch"
stages = [
    { name = "plan", description = "decide what to do", tools = ["read_file", "@builtin"], required_tools = ["read_file"], max_iterations = 8, shell_hint = true, mode = { interactive_points = [{ name = "review", prompt = "Does this plan look right?" }] } },
    { name = "build", accepts_messages = false, allow_blocking_tools = true, require_output = true },
]
edges = [{ name = "build", from = "plan", to = "build", hint = "when the plan is settled" }]

[graph.layout]
total_budget_tokens = 3000
regions = [
    { name = "plan", kind = "pinned", budget = 2000, description = "the plan so far", required = true },
    { name = "notes", kind = { kind = "sliding_window", max_items = 10 }, budget = 1000 },
]
"#
    .to_string()
}

/// Build the object under test from an `agent.toml`.
fn blueprint(text: &str, source: CoreSource) -> Blueprint {
    Blueprint {
        parsed: super::super::manifest::parsed(text),
        digest: digest_of(text),
        source: source.into(),
    }
}

/// Ask the schema for a blueprint's fields.
async fn ask(text: &str, source: CoreSource, query: &str) -> serde_json::Value {
    let schema = Schema::build(
        BlueprintProbe {
            blueprint: blueprint(text, source),
        },
        EmptyMutation,
        EmptySubscription,
    )
    .finish();
    let answer = schema.execute(Request::new(query)).await;
    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
    serde_json::to_value(&answer.data).expect("data serializes")
}

/// A root handing out one blueprint, so a field test is one query.
struct BlueprintProbe {
    blueprint: Blueprint,
}

#[async_graphql::Object]
impl BlueprintProbe {
    /// The blueprint under test.
    async fn blueprint(&self) -> &Blueprint {
        &self.blueprint
    }
}

/// The id carries the digest, so two revisions of one name are two ids.
///
/// Without this, a client that caches by type and id merges a run's frozen
/// copy with whatever is installed now, and shows one run's blueprint under
/// another run.
#[tokio::test]
async fn the_id_tells_two_revisions_of_one_name_apart() {
    let one = ask(
        &manifest(),
        CoreSource::Snapshot,
        "{ blueprint { id name digest } }",
    )
    .await;
    let edited = manifest().replace("writes code", "writes better code");
    let two = ask(
        &edited,
        CoreSource::Snapshot,
        "{ blueprint { id name digest } }",
    )
    .await;

    assert_eq!(one["blueprint"]["name"], two["blueprint"]["name"]);
    assert_ne!(one["blueprint"]["id"], two["blueprint"]["id"]);
    assert_ne!(one["blueprint"]["digest"], two["blueprint"]["digest"]);
    let id = one["blueprint"]["id"].as_str().expect("an id");
    let digest = one["blueprint"]["digest"].as_str().expect("a digest");
    let short: String = digest.chars().take(12).collect();
    assert_eq!(id, format!("coder@{short}"));
}

/// Where a blueprint came from is part of the answer: "what ran" and "what is
/// installed" are different questions.
#[tokio::test]
async fn the_source_says_which_file_this_came_from() {
    let snapshot = ask(
        &manifest(),
        CoreSource::Snapshot,
        "{ blueprint { source } }",
    )
    .await;
    assert_eq!(snapshot["blueprint"]["source"], "SNAPSHOT");
    let installed = ask(
        &manifest(),
        CoreSource::Installed,
        "{ blueprint { source } }",
    )
    .await;
    assert_eq!(installed["blueprint"]["source"], "INSTALLED");
    assert_eq!(
        BlueprintSource::from(CoreSource::Snapshot),
        BlueprintSource::Snapshot
    );
}

/// The blueprint-level fields, as a client reads them.
#[tokio::test]
async fn a_blueprint_carries_its_agent_block() {
    let json = ask(
        &manifest(),
        CoreSource::Snapshot,
        "{ blueprint { name version description maxChildDepth toolRescan readPaths
                       entryStage { name mode } } }",
    )
    .await;
    let bp = &json["blueprint"];
    assert_eq!(bp["name"], "coder");
    assert_eq!(bp["version"], "1.2.0");
    assert_eq!(bp["description"], "writes code");
    assert_eq!(bp["maxChildDepth"], 4);
    // The setting decides when a run looks for tools again, not whether it may
    // install one, so it is named for that.
    assert_eq!(bp["toolRescan"], "RESCAN_BEFORE_DISPATCH");
    assert_eq!(bp["readPaths"][0], "~/designs");
    // `entry_stage` names a stage rather than taking the first declared one.
    assert_eq!(bp["entryStage"]["name"], "plan");
    assert_eq!(bp["entryStage"]["mode"], "INTERACTIVE_POINTS");
}

/// A blueprint naming no entry stage starts at the first one declared.
#[tokio::test]
async fn an_unnamed_entry_stage_is_the_first_declared() {
    let text = manifest().replace("entry = \"plan\"\n", "");
    let json = ask(
        &text,
        CoreSource::Snapshot,
        "{ blueprint { entryStage { name } } }",
    )
    .await;
    assert_eq!(json["blueprint"]["entryStage"]["name"], "plan");
}

/// Every value the setting can take reads back, and a blueprint that says
/// nothing.
#[tokio::test]
async fn every_rescan_setting_reads_back() {
    let cases = [
        (r#"tool_rescan = "at_spawn""#, "AT_SPAWN_ONLY"),
        (r#"tool_rescan = "after_writes""#, "RESCAN_AFTER_WRITES"),
        (
            r#"tool_rescan = "before_dispatch""#,
            "RESCAN_BEFORE_DISPATCH",
        ),
        // And a blueprint that mentions none of it is fixed at spawn.
        ("", "AT_SPAWN_ONLY"),
    ];
    for (line, expected) in cases {
        let text = manifest().replace(r#"tool_rescan = "before_dispatch""#, line);
        let json = ask(&text, CoreSource::Snapshot, "{ blueprint { toolRescan } }").await;
        assert_eq!(json["blueprint"]["toolRescan"], expected, "{line}");
    }
    assert_eq!(ToolRescan::AtSpawnOnly, ToolRescan::AtSpawnOnly);
}

/// The three states of a prompt hint, and what each one means.
///
/// `OMIT` leaves the paragraph out. It does not tell the model to avoid the
/// behaviour, and nothing in the schema should read as though it did.
#[tokio::test]
async fn prompt_guidance_has_three_states() {
    let json = ask(
        &manifest(),
        CoreSource::Snapshot,
        "{ blueprint { toolGuidance { batchIndependentCalls shellForMultiStepWork }
                       stages { name toolGuidance { batchIndependentCalls shellForMultiStepWork } } } }",
    )
    .await;
    let bp = &json["blueprint"]["toolGuidance"];
    assert_eq!(bp["batchIndependentCalls"], "OMIT", "declared false");
    assert_eq!(bp["shellForMultiStepWork"], "INHERIT", "not declared");
    let plan = &json["blueprint"]["stages"][0]["toolGuidance"];
    assert_eq!(plan["shellForMultiStepWork"], "INCLUDE", "declared true");
    assert_eq!(
        plan["batchIndependentCalls"], "INHERIT",
        "stage says nothing"
    );

    assert_eq!(HintSetting::from(None), HintSetting::Inherit);
    assert_eq!(HintSetting::from(Some(true)), HintSetting::Include);
    assert_eq!(HintSetting::from(Some(false)), HintSetting::Omit);
}

/// The stage fields, including the ones whose names had to change to say what
/// they do.
#[tokio::test]
async fn a_stage_carries_its_own_block() {
    let json = ask(
        &manifest(),
        CoreSource::Snapshot,
        "{ blueprint { stages { name mode description availableTools requiredTools
                                maxIterations acceptsMessages declaresBlockingTools
                                outputRequirement { reasks }
                                transitions { target { name } targetName hint } } } }",
    )
    .await;
    let stages = json["blueprint"]["stages"].as_array().expect("stages");
    assert_eq!(stages.len(), 2, "declaration order");
    let plan = &stages[0];
    assert_eq!(plan["name"], "plan");
    assert_eq!(plan["mode"], "INTERACTIVE_POINTS");
    assert_eq!(plan["description"], "decide what to do");
    assert_eq!(plan["availableTools"][1], "@builtin");
    assert_eq!(plan["requiredTools"][0], "read_file");
    assert_eq!(plan["maxIterations"], 8);
    assert_eq!(plan["transitions"][0]["target"]["name"], "build");
    assert_eq!(plan["transitions"][0]["targetName"], "build");
    assert_eq!(plan["transitions"][0]["hint"], "when the plan is settled");
    // Not required: the stage may leave without submitting.
    assert!(plan["outputRequirement"].is_null());

    let build = &stages[1];
    assert_eq!(build["mode"], "AUTONOMOUS");
    assert_eq!(build["acceptsMessages"], false);
    // A lint acknowledgement, which is all it ever was.
    assert_eq!(build["declaresBlockingTools"], true);
    // Required, and the bound on being asked again is part of the answer.
    assert_eq!(build["outputRequirement"]["reasks"], 3);
    assert!(
        build["transitions"].as_array().map(Vec::is_empty) == Some(true),
        "terminal"
    );
}

/// The stage fields a run's limits come from, and the edge ordering.
///
/// A stage with two edges is what makes the sort observable: they are listed
/// by target, whatever order the blueprint declares them in.
#[tokio::test]
async fn a_stage_carries_its_limits_and_orders_its_edges() {
    let text = r#"[blueprint]
name = "router"
version = "1.0.0"

[graph]
stages = [
    { name = "pick", max_revisits = 2, requires_children = true, allow_complete = true, allow_as_worker = true, connectors = ["github"] },
    { name = "alpha", allow_complete = true },
    { name = "zeta", allow_complete = true },
]
edges = [
    { name = "zeta", from = "pick", to = "zeta", hint = "last alphabetically" },
    { name = "alpha", from = "pick", to = "alpha" },
]
layout = { total_budget_tokens = 1000, regions = [] }
"#;
    let json = ask(
        text,
        CoreSource::Installed,
        "{ blueprint { stages { name maxRevisits requiresChildren allowComplete allowAsWorker
                                availableConnectors transitions { target { name } hint } } } }",
    )
    .await;
    let pick = &json["blueprint"]["stages"][0];
    assert_eq!(pick["name"], "pick");
    assert_eq!(pick["maxRevisits"], 2);
    assert_eq!(pick["requiresChildren"], true);
    assert_eq!(pick["allowComplete"], true);
    assert_eq!(pick["allowAsWorker"], true);
    assert_eq!(pick["availableConnectors"][0], "github");
    // Sorted by target, whatever order the blueprint listed them in.
    assert_eq!(pick["transitions"][0]["target"]["name"], "alpha");
    assert!(pick["transitions"][0]["hint"].is_null());
    assert_eq!(pick["transitions"][1]["target"]["name"], "zeta");

    // A stage naming no revisit bound says so with a null rather than a zero.
    let alpha = &json["blueprint"]["stages"][1];
    assert!(alpha["maxRevisits"].is_null());
    assert_eq!(alpha["requiresChildren"], false);
}

/// Regions carry what they hold and how they behave when full.
#[tokio::test]
async fn regions_carry_their_kind_and_ceiling() {
    let json = ask(
        &manifest(),
        CoreSource::Snapshot,
        "{ blueprint { regions { name kind maxTokens description required describeInPrompt } } }",
    )
    .await;
    let regions = json["blueprint"]["regions"].as_array().expect("regions");
    let plan = regions
        .iter()
        .find(|r| r["name"] == "plan")
        .expect("the plan region");
    assert_eq!(plan["kind"], "PINNED");
    assert_eq!(plan["maxTokens"], 2000);
    assert_eq!(plan["description"], "the plan so far");
    assert_eq!(plan["required"], true);
    assert_eq!(plan["describeInPrompt"], false);
    let notes = regions
        .iter()
        .find(|r| r["name"] == "notes")
        .expect("the notes region");
    assert_eq!(notes["kind"], "SLIDING_WINDOW");
    assert!(notes["description"].is_null());
}

/// Every region kind a graph can declare has exactly one schema value.
#[test]
fn every_region_kind_maps_to_one_value() {
    use leviath_runtime::spec::graph::{CodeRef, Eviction, RegionKind as Kind};
    let cases = [
        (Kind::Pinned, RegionKind::Pinned),
        (Kind::Temporary, RegionKind::Temporary),
        (Kind::Clearable, RegionKind::Clearable),
        (
            Kind::SlidingWindow {
                max_items: 10,
                eviction: Eviction::PerItem,
            },
            RegionKind::SlidingWindow,
        ),
        (
            Kind::Compacting {
                threshold_tokens: Some(100),
            },
            RegionKind::Compacting,
        ),
        (
            Kind::CompactHistory { source: None },
            RegionKind::CompactHistory,
        ),
        (Kind::Keyed { max_entries: None }, RegionKind::Keyed),
        (Kind::Checklist, RegionKind::Checklist),
        (
            Kind::Custom {
                code: CodeRef::File("s.rhai".to_string()),
                pinned: false,
            },
            RegionKind::Custom,
        ),
    ];
    for (kind, expected) in cases {
        assert_eq!(RegionKind::from(&kind), expected, "{kind:?}");
    }
}

/// Every stage mode maps to one schema value.
#[test]
fn every_stage_mode_maps_to_one_value() {
    use leviath_runtime::spec::graph::{FanOutDef, StageMode as Mode};
    use leviath_runtime::spec::names::StageName;
    assert_eq!(StageMode::from(&Mode::Autonomous), StageMode::Autonomous);
    assert_eq!(StageMode::from(&Mode::Interactive), StageMode::Interactive);
    assert_eq!(StageMode::from(&Mode::Output), StageMode::Output);
    assert_eq!(
        StageMode::from(&Mode::InteractivePoints(Vec::new())),
        StageMode::InteractivePoints
    );
    let fanned = Mode::FanOut(FanOutDef::same_graph(StageName::new("work").unwrap()));
    assert_eq!(StageMode::from(&fanned), StageMode::FanOut);
}

/// Every kind a snapshot can carry reads back, including the two spellings
/// older builds wrote and the one nothing here knows.
///
/// The spellings matter because those files are still on disk: a reader that
/// only knew the current words would answer null for a `sliding_window` region
/// written months ago, which looks like a region with no behaviour at all.
#[test]
fn a_snapshots_region_kind_reads_back_under_either_spelling() {
    use super::RegionKind;
    let cases = [
        ("pinned", RegionKind::Pinned),
        ("temporary", RegionKind::Temporary),
        ("clearable", RegionKind::Clearable),
        ("sliding_window", RegionKind::SlidingWindow),
        ("sliding", RegionKind::SlidingWindow),
        ("compacting", RegionKind::Compacting),
        ("compact_history", RegionKind::CompactHistory),
        ("history", RegionKind::CompactHistory),
        ("keyed", RegionKind::Keyed),
        ("checklist", RegionKind::Checklist),
        ("custom", RegionKind::Custom),
    ];
    for (word, kind) in cases {
        assert_eq!(RegionKind::from_snapshot(word), Some(kind), "{word}");
    }
    // A run written by a newer build. Null beside a region that is plainly
    // there beats refusing the whole window.
    assert_eq!(RegionKind::from_snapshot("something-else"), None);
    assert_eq!(RegionKind::from_snapshot(""), None);
}

/// Every function `#[mirror]` wrote for this file's types runs at least once.
///
/// The mirrors are straight lines of delegation, so running each of them once
/// is enough to measure all of them. One test per file rather than per query:
/// what a query happens to select is not what the mirror is made of.
#[tokio::test]
async fn every_mirrored_function_runs() {
    use crate::commands::serve::graphql::filter::testkit::{
        exercise, exercise_enum, exercise_list, exercise_order,
    };

    let one = blueprint(&manifest(), CoreSource::Installed);
    let regions: Vec<super::Region> = (0..one.parsed.graph.layout.regions.len())
        .map(|at| super::Region {
            blueprint: Arc::clone(&one.parsed),
            stage: None,
            at,
        })
        .collect();

    exercise(std::slice::from_ref(&one)).await;
    exercise_order(&one, super::BlueprintOrderField::ALL);
    exercise(&regions).await;
    exercise_list(&regions).await;
    exercise(&[super::ToolUseGuidance {
        batch_independent_calls: HintSetting::Include,
        shell_for_multi_step_work: HintSetting::Inherit,
    }])
    .await;
    exercise_enum(&[BlueprintSource::Installed, BlueprintSource::Snapshot]).await;
    exercise_enum(&[ToolRescan::AtSpawnOnly, ToolRescan::RescanBeforeDispatch]).await;
    exercise_enum(&[HintSetting::Inherit, HintSetting::Omit]).await;
    exercise_enum(&[RegionKind::Pinned, RegionKind::SlidingWindow]).await;
}

/// A blueprint's inputs are typed, each with where its value goes: the
/// `task` region takes the task, and a region seeded from the caller takes an
/// input of that name.
#[tokio::test]
async fn a_blueprint_lists_the_inputs_a_spawn_takes() {
    let text = r#"[blueprint]
name = "writer"
version = "1.0.0"

[graph]
stages = [{ name = "main", system_prompt = "work" }]
layout = { total_budget_tokens = 1000, regions = [
    { name = "task", kind = "pinned", budget = 400 },
    { name = "brief", kind = "pinned", budget = 400 },
] }
inputs = [
    { name = "task", type = "text", required = true, binds = [{ region = "task" }] },
    { name = "brief_text", type = { kind = "text", multiline = true }, required = true, description = "what to write", binds = [{ region = "brief" }] },
]
"#;
    let answer = ask(
        text,
        CoreSource::Installed,
        "{ blueprint { inputs { name required description
            type { __typename ... on TextInputTypeOutput { multiline summary } }
            binds { kind region template stage } default { __typename } } } }",
    )
    .await;
    let inputs = answer["blueprint"]["inputs"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let named = |name: &str| {
        inputs
            .iter()
            .find(|input| input["name"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("no input {name}: {inputs:?}"))
    };
    let brief = named("brief_text");
    assert_eq!(brief["required"], true);
    assert_eq!(brief["description"], "what to write");
    assert_eq!(brief["type"]["__typename"], "TextInputTypeOutput");
    assert_eq!(brief["type"]["multiline"], true);
    assert_eq!(brief["binds"][0]["kind"], "REGION");
    assert_eq!(brief["binds"][0]["region"], "brief");
    assert_eq!(brief["default"], serde_json::Value::Null);
    assert_eq!(named("task")["binds"][0]["region"], "task");
}
