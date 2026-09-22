//! Tests for how a run leaves one stage for the next, and the mirrors of
//! those types.

use std::sync::Arc;

use leviath_core::Blueprint as CoreBlueprint;
use leviath_core::blueprint::{
    EdgeTransform, RegionCount, TransitionCondition as CoreCondition, TransitionEdge as CoreEdge,
    TransitionGate as CoreGate,
};

use super::{
    ContextTransform, MappingTransform, RegionEntryRequirement, RegionMapping, StuckThresholds,
    TransformConfig, TransformRegions, TransitionCondition, TransitionEdge, TransitionGate,
    TransitionTransform,
};
use crate::commands::serve::graphql::filter::testkit::{exercise, exercise_enum, exercise_list};

/// A blueprint with the regions and stages these types resolve names against.
fn blueprint() -> Arc<CoreBlueprint> {
    let text = "[agent]\n\
                name = \"router\"\n\
                \n\
                [context.regions.plan]\n\
                kind = \"pinned\"\n\
                max_tokens = 100\n\
                \n\
                [context.regions.env]\n\
                kind = \"temporary\"\n\
                max_tokens = 100\n\
                \n\
                [stages.plan]\n\
                mode = \"autonomous\"\n\
                \n\
                [stages.build]\n\
                mode = \"autonomous\"\n";
    Arc::new(leviath_core::manifest::parse_manifest(text).expect("the manifest parses"))
}

/// One edge, custom-transformed and gated, as a manifest would write it.
fn custom_edge() -> CoreEdge {
    CoreEdge {
        target: "build".to_string(),
        condition: CoreCondition::LlmChoice,
        hint: Some("when the plan is settled".to_string()),
        transform: EdgeTransform::Custom {
            carry: vec!["plan".to_string()],
            compact: vec![],
            clear: vec!["env".to_string()],
            compact_prompt: None,
        },
        gate: None,
        stuck: None,
    }
}

/// Every function `#[mirror]` wrote for this file's types runs at least once.
///
/// The mirrors are straight lines of delegation, so running each of them once
/// is enough to measure all of them.
#[tokio::test]
async fn every_mirrored_function_runs() {
    let bp = blueprint();

    exercise_enum(&[TransitionCondition::Always, TransitionCondition::Stuck]).await;
    exercise_enum(&[TransitionTransform::Direct, TransitionTransform::Custom]).await;
    exercise_enum(&[
        MappingTransform::Direct,
        MappingTransform::Summarize,
        MappingTransform::Extract,
    ])
    .await;

    exercise(&[TransformConfig {
        blueprint: Arc::clone(&bp),
        named: TransformRegions {
            carry: vec!["plan".to_string()],
            compact: vec![],
            clear: vec!["env".to_string()],
        },
        compact_prompt: Some("keep the decisions".to_string()),
    }])
    .await;

    exercise(&[RegionEntryRequirement {
        blueprint: Arc::clone(&bp),
        region: "plan".to_string(),
        at_least: 3,
    }])
    .await;

    exercise(&[StuckThresholds {
        after_iterations: Some(12),
        after_minutes: None,
        after_same_file_edits: None,
        after_tool_calls: None,
    }])
    .await;

    exercise(&[TransitionGate {
        blueprint: Arc::clone(&bp),
        gate: CoreGate {
            require_modifications: true,
            region: Some("plan".to_string()),
            tools: vec!["shell".to_string()],
            require_region_updated: Some("plan".to_string()),
            require_regions: vec!["plan".to_string()],
            require_no_open_items: Some("plan".to_string()),
            require_region_entries: Some(RegionCount {
                region: "plan".to_string(),
                at_least: 2,
            }),
            message: Some("write the plan first".to_string()),
            max_attempts: Some(2),
        },
    }])
    .await;

    exercise(&[TransitionEdge::of(&bp, "build", &custom_edge())]).await;
    exercise_list(&[TransitionEdge::of(
        &bp,
        "build",
        &CoreEdge {
            target: "build".to_string(),
            condition: CoreCondition::Always,
            hint: None,
            transform: EdgeTransform::Direct,
            gate: None,
            stuck: None,
        },
    )])
    .await;

    exercise(&[RegionMapping {
        from_region: "plan".to_string(),
        to_region: "notes".to_string(),
        transform: Some(MappingTransform::Summarize),
        fields: vec!["summary".to_string()],
    }])
    .await;
    exercise_list(&[RegionMapping {
        from_region: "plan".to_string(),
        to_region: "notes".to_string(),
        transform: None,
        fields: vec![],
    }])
    .await;

    exercise(&[ContextTransform {
        from_blueprint: "coder".to_string(),
        to_blueprint: "reviewer".to_string(),
        mappings: vec![RegionMapping {
            from_region: "plan".to_string(),
            to_region: "notes".to_string(),
            transform: Some(MappingTransform::Direct),
            fields: vec![],
        }],
    }])
    .await;
    exercise_list(&[ContextTransform {
        from_blueprint: "coder".to_string(),
        to_blueprint: "reviewer".to_string(),
        mappings: vec![],
    }])
    .await;
}

/// A root handing out one edge, so a field test is one query.
struct EdgeProbe {
    edge: TransitionEdge,
}

#[async_graphql::Object]
impl EdgeProbe {
    /// The edge under test.
    async fn edge(&self) -> &TransitionEdge {
        &self.edge
    }
}

/// An edge naming a stage no blueprint declares resolves to nothing, and the
/// name it wrote is still served beside it.
#[tokio::test]
async fn a_dangling_target_resolves_to_nothing() {
    use async_graphql::{EmptyMutation, EmptySubscription, Request, Schema};

    let bp = blueprint();
    let edge = TransitionEdge::of(
        &bp,
        "nowhere",
        &CoreEdge {
            target: "nowhere".to_string(),
            condition: CoreCondition::Always,
            hint: None,
            transform: EdgeTransform::Direct,
            gate: None,
            stuck: None,
        },
    );
    let schema = Schema::build(EdgeProbe { edge }, EmptyMutation, EmptySubscription).finish();
    let answer = schema
        .execute(Request::new("{ edge { target { name } targetName } }"))
        .await;
    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
    let json = serde_json::to_value(&answer.data).expect("data serializes");
    assert!(json["edge"]["target"].is_null());
    assert_eq!(json["edge"]["targetName"], "nowhere");
}
