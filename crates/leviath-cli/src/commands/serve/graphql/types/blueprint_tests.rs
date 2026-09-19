//! Tests for the `Blueprint` object and the values it carries.

use std::sync::Arc;

use async_graphql::{EmptyMutation, EmptySubscription, Request, Schema};

use super::{Blueprint, BlueprintSource, HintSetting, RegionKind, StageMode, ToolDiscovery};
use crate::commands::serve::core::blueprints::{BlueprintSource as CoreSource, digest_of};

/// A manifest exercising the fields this module maps.
fn manifest() -> String {
    "[agent]\n\
     name = \"coder\"\n\
     version = \"1.2.0\"\n\
     description = \"writes code\"\n\
     entry_stage = \"plan\"\n\
     max_child_depth = 4\n\
     dynamic_tools = true\n\
     batch_tool_hint = false\n\
     \n\
     [read_paths]\n\
     allow = [\"~/designs\"]\n\
     \n\
     [context.regions.plan]\n\
     kind = \"pinned\"\n\
     max_tokens = 2000\n\
     description = \"the plan so far\"\n\
     required = true\n\
     \n\
     [context.regions.notes]\n\
     kind = \"sliding_window\"\n\
     max_tokens = 1000\n\
     \n\
     [stages.plan]\n\
     mode = \"interactive_points\"\n\
     description = \"decide what to do\"\n\
     available_tools = [\"read_file\", \"@builtin\"]\n\
     required_tools = [\"read_file\"]\n\
     max_iterations = 8\n\
     shell_hint = true\n\
     \n\
     [stages.plan.interaction_points.review]\n\
     prompt = \"Does this plan look right?\"\n\
     \n\
     [stages.plan.transitions.build]\n\
     hint = \"when the plan is settled\"\n\
     \n\
     [stages.build]\n\
     mode = \"autonomous\"\n\
     require_output = true\n\
     allow_blocking_tools = true\n\
     accepts_messages = false\n\
     "
    .to_string()
}

/// Build the object under test from manifest text.
fn blueprint(text: &str, source: CoreSource) -> Blueprint {
    let parsed = leviath_core::manifest::parse_manifest(text).expect("the manifest parses");
    Blueprint {
        parsed: Arc::new(parsed),
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
        "{ blueprint { name version description maxChildDepth toolDiscovery readPaths
                       entryStage { name mode } } }",
    )
    .await;
    let bp = &json["blueprint"];
    assert_eq!(bp["name"], "coder");
    assert_eq!(bp["version"], "1.2.0");
    assert_eq!(bp["description"], "writes code");
    assert_eq!(bp["maxChildDepth"], 4);
    // `dynamic_tools = true` decides when a run sees new tools, so it is named
    // for that rather than for installing them.
    assert_eq!(bp["toolDiscovery"], "RESCAN_AFTER_WRITES");
    assert_eq!(bp["readPaths"][0], "~/designs");
    // `entry_stage` names a stage rather than taking the first declared one.
    assert_eq!(bp["entryStage"]["name"], "plan");
    assert_eq!(bp["entryStage"]["mode"], "INTERACTIVE_POINTS");
}

/// A blueprint naming no entry stage starts at the first one declared.
#[tokio::test]
async fn an_unnamed_entry_stage_is_the_first_declared() {
    let text = manifest().replace("entry_stage = \"plan\"\n", "");
    let json = ask(
        &text,
        CoreSource::Snapshot,
        "{ blueprint { entryStage { name } } }",
    )
    .await;
    assert_eq!(json["blueprint"]["entryStage"]["name"], "plan");
}

/// A tool set fixed at spawn says so, rather than reading as a missing field.
#[tokio::test]
async fn a_blueprint_without_live_discovery_says_at_spawn_only() {
    let text = manifest().replace("dynamic_tools = true", "dynamic_tools = false");
    let json = ask(
        &text,
        CoreSource::Snapshot,
        "{ blueprint { toolDiscovery } }",
    )
    .await;
    assert_eq!(json["blueprint"]["toolDiscovery"], "AT_SPAWN_ONLY");
    assert_eq!(ToolDiscovery::AtSpawnOnly, ToolDiscovery::AtSpawnOnly);
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
                                transitions { target hint } } } }",
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
    assert_eq!(plan["transitions"][0]["target"], "build");
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

/// Every region kind the daemon recognises has exactly one schema value, and
/// the manifest's two spellings of one kind land on one of them.
#[test]
fn every_region_kind_maps_to_one_value() {
    use leviath_core::region::RegionKind as Core;
    let cases = [
        (Core::Pinned, RegionKind::Pinned),
        (Core::Temporary, RegionKind::Temporary),
        (Core::Clearable, RegionKind::Clearable),
        (
            Core::SlidingWindow {
                max_items: 10,
                eviction_strategy: Default::default(),
            },
            RegionKind::SlidingWindow,
        ),
        (
            Core::Compacting {
                threshold_tokens: 100,
            },
            RegionKind::Compacting,
        ),
        (
            Core::CompactHistory {
                source_region: "notes".to_string(),
            },
            RegionKind::CompactHistory,
        ),
        (Core::HashMap { max_entries: None }, RegionKind::Hashmap),
        (Core::Checklist, RegionKind::Checklist),
        (
            Core::Custom {
                script: "s.rhai".to_string(),
                persistent: false,
            },
            RegionKind::Custom,
        ),
    ];
    for (core, expected) in cases {
        assert_eq!(RegionKind::from(&core), expected, "{core:?}");
    }
}

/// Every stage mode maps to one schema value.
#[test]
fn every_stage_mode_maps_to_one_value() {
    use leviath_core::blueprint::StageMode as Core;
    assert_eq!(StageMode::from(&Core::Autonomous), StageMode::Autonomous);
    assert_eq!(StageMode::from(&Core::Interactive), StageMode::Interactive);
    assert_eq!(StageMode::from(&Core::Output), StageMode::Output);
    assert_eq!(
        StageMode::from(&Core::InteractivePoints { points: Vec::new() }),
        StageMode::InteractivePoints
    );
    // Fan-out carries a config with no default, so this one comes from a
    // manifest: the mapping is what is under test, not the config's shape.
    let fanned = leviath_core::manifest::parse_manifest(
        "[agent]\nname = \"f\"\n\n\
         [stages.split]\nmode = \"fan_out\"\nworker_stage = \"work\"\n\
         split_prompt = \"one item per line\"\n\n\
         [stages.work]\nmode = \"autonomous\"\nallow_as_worker = true\n",
    )
    .expect("the manifest parses");
    assert_eq!(StageMode::from(&fanned.stages[0].mode), StageMode::FanOut);
}
