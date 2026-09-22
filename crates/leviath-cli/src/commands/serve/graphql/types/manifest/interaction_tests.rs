//! Tests for the checkpoints a stage raises, and for the mirrors of those
//! types.

use std::sync::Arc;

use async_graphql::{EmptyMutation, EmptySubscription, Request, Schema};

use super::{DirectiveEntry, InteractionPoint, InteractionPointStyle, UnattendedPolicy};
use crate::commands::serve::graphql::filter::testkit::{exercise, exercise_enum, exercise_list};

/// A manifest with one checkpoint that exercises every field: options,
/// directives, abort and edit options, and a document region that resolves.
fn manifest() -> &'static str {
    r#"
[agent]
name = "checked"
version = "1.0.0"
description = "one checkpoint"

[context.regions.plan]
kind = "pinned"
max_tokens = 1000

[stages.review]
mode = "interactive_points"

[[stages.review.interaction_points]]
name = "decide"
prompt = "which way?"
required = true
unattended = "ask"
style = "multiple_choice"
options = ["ship", "hold"]
document_region = "plan"
directives = { ship = "go to the next stage", hold = "ask again later" }
abort_options = ["cancel"]
edit_options = ["ship"]
"#
}

/// The one checkpoint `manifest` declares, resolved against its blueprint.
fn point() -> InteractionPoint {
    let parsed = leviath_core::manifest::parse_manifest(manifest()).expect("the manifest parses");
    let blueprint = Arc::new(parsed);
    let core_point = match &blueprint.stages[0].mode {
        leviath_core::blueprint::StageMode::InteractivePoints { points } => points[0].clone(),
        other => panic!("the stage declares interaction points, got {other:?}"),
    };
    InteractionPoint::of(&blueprint, &core_point)
}

/// A root handing out one checkpoint, so a field test is one query.
struct Probe {
    /// The checkpoint under test.
    point: InteractionPoint,
}

#[async_graphql::Object]
impl Probe {
    /// The checkpoint under test.
    async fn point(&self) -> &InteractionPoint {
        &self.point
    }
}

/// Ask the schema about one checkpoint, built from manifest text.
async fn ask(text: &str, query: &str) -> serde_json::Value {
    let parsed = leviath_core::manifest::parse_manifest(text).expect("the manifest parses");
    let blueprint = Arc::new(parsed);
    let core_point = match &blueprint.stages[0].mode {
        leviath_core::blueprint::StageMode::InteractivePoints { points } => points[0].clone(),
        other => panic!("the stage declares interaction points, got {other:?}"),
    };
    let schema = Schema::build(
        Probe {
            point: InteractionPoint::of(&blueprint, &core_point),
        },
        EmptyMutation,
        EmptySubscription,
    )
    .finish();
    let answer = schema.execute(Request::new(query)).await;
    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
    serde_json::to_value(&answer.data).expect("data serializes")
}

/// Every field of a checkpoint that names a region a layout declares.
#[tokio::test]
async fn a_checkpoint_carries_its_fields_and_its_document_region() {
    let json = ask(
        manifest(),
        "{ point { name prompt required unattended style options abortOptions editOptions
                   directives { option instruction }
                   documentRegion { name } documentRegionName } }",
    )
    .await;
    let point = &json["point"];
    assert_eq!(point["name"], "decide");
    assert_eq!(point["prompt"], "which way?");
    assert_eq!(point["required"], true);
    assert_eq!(point["unattended"], "ASK");
    assert_eq!(point["style"], "MULTIPLE_CHOICE");
    assert_eq!(point["options"], serde_json::json!(["ship", "hold"]));
    assert_eq!(point["abortOptions"], serde_json::json!(["cancel"]));
    assert_eq!(point["editOptions"], serde_json::json!(["ship"]));
    // The manifest holds directives in a map, so a fixed order is what lets two
    // reads of one blueprint agree.
    assert_eq!(
        point["directives"],
        serde_json::json!([
            { "option": "hold", "instruction": "ask again later" },
            { "option": "ship", "instruction": "go to the next stage" },
        ])
    );
    assert_eq!(point["documentRegion"]["name"], "plan");
    assert_eq!(point["documentRegionName"], "plan");
}

/// A checkpoint naming a region no layout declares answers null for the region
/// and keeps the name, which is the pair a client branches on.
#[tokio::test]
async fn a_checkpoint_naming_an_undeclared_region_keeps_the_name() {
    let text = manifest().replace("document_region = \"plan\"", "document_region = \"stray\"");
    let json = ask(
        &text,
        "{ point { documentRegion { name } documentRegionName } }",
    )
    .await;
    let point = &json["point"];
    assert!(
        point["documentRegion"].is_null(),
        "nothing declares it: {point}"
    );
    assert_eq!(point["documentRegionName"], "stray");
}

/// A checkpoint naming no document region answers null for both halves.
#[tokio::test]
async fn a_checkpoint_naming_no_document_region_answers_null_for_both_halves() {
    let text = manifest().replace("document_region = \"plan\"\n", "");
    let json = ask(
        &text,
        "{ point { documentRegion { name } documentRegionName } }",
    )
    .await;
    let point = &json["point"];
    assert!(point["documentRegion"].is_null());
    assert!(point["documentRegionName"].is_null());
}

/// Every function `#[mirror]` wrote for this file's types runs at least once.
#[tokio::test]
async fn every_mirrored_function_runs() {
    exercise_enum(&[UnattendedPolicy::AutoApprove, UnattendedPolicy::Ask]).await;
    exercise_enum(&[
        InteractionPointStyle::FreeText,
        InteractionPointStyle::MultipleChoice,
        InteractionPointStyle::Confirm,
    ])
    .await;

    let directives = vec![DirectiveEntry {
        option: "ship".to_string(),
        instruction: "go to the next stage".to_string(),
    }];
    exercise(&directives).await;
    exercise_list(&directives).await;

    let value = point();
    exercise(std::slice::from_ref(&value)).await;
    exercise_list(&[value]).await;
}
