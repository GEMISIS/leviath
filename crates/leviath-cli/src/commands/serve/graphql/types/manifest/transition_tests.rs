//! Tests for how a run leaves one stage for the next, and the mirrors of
//! those types.

use std::sync::Arc;

use leviath_runtime::spec::graph::{
    EdgeCarry, EdgeCondition as CoreCondition, EdgeDef as CoreEdge, GateDef as CoreGate,
    RegionCount,
};
use leviath_runtime::spec::names::{EdgeName, RegionName, StageName, ToolName};

use crate::commands::serve::core::blueprints::ParsedBlueprint;

use super::{
    ContextTransform, MappingTransform, RegionEntryRequirement, RegionMapping, StuckThresholds,
    TransformConfig, TransformRegions, TransitionCondition, TransitionEdge, TransitionGate,
    TransitionTransform,
};
use crate::commands::serve::graphql::filter::testkit::{exercise, exercise_enum, exercise_list};

/// A blueprint with the regions and stages these types resolve names against.
fn blueprint() -> Arc<ParsedBlueprint> {
    super::super::parsed(
        r#"[blueprint]
name = "router"
version = "1.0.0"

[graph]
stages = [{ name = "plan" }, { name = "build" }]
layout = { total_budget_tokens = 200, regions = [{ name = "plan", kind = "pinned", budget = 100 }, { name = "env", kind = "temporary", budget = 100 }] }
"#,
    )
}

/// An edge from `plan` to `to`, taken when it is `when`, carrying `carry`.
fn edge(to: &str, when: CoreCondition, carry: EdgeCarry) -> CoreEdge {
    CoreEdge {
        name: EdgeName::new(to).unwrap(),
        from: StageName::new("plan").unwrap(),
        to: StageName::new(to).unwrap(),
        when,
        hint: None,
        carry,
        gate: None,
        stuck: None,
    }
}

/// One edge, custom-transformed, as a blueprint would write it.
fn custom_edge() -> CoreEdge {
    CoreEdge {
        hint: Some("when the plan is settled".to_string()),
        ..edge(
            "build",
            CoreCondition::LlmChoice,
            EdgeCarry::Custom {
                carry: vec![RegionName::new("plan").unwrap()],
                compact: vec![],
                clear: vec![RegionName::new("env").unwrap()],
                compact_prompt: None,
            },
        )
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
            region: Some(RegionName::new("plan").unwrap()),
            tools: vec![ToolName::new("shell").unwrap()],
            require_region_updated: Some(RegionName::new("plan").unwrap()),
            require_regions: vec![RegionName::new("plan").unwrap()],
            require_no_open_items: Some(RegionName::new("plan").unwrap()),
            require_region_entries: Some(RegionCount {
                region: RegionName::new("plan").unwrap(),
                at_least: 2,
            }),
            message: Some("write the plan first".to_string()),
            max_attempts: Some(2),
        },
    }])
    .await;

    exercise(&[TransitionEdge::of(&bp, &custom_edge())]).await;
    exercise_list(&[TransitionEdge::of(
        &bp,
        &edge("build", CoreCondition::Always, EdgeCarry::Direct),
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
    let dangling = TransitionEdge::of(
        &bp,
        &edge("nowhere", CoreCondition::Always, EdgeCarry::Direct),
    );
    let schema = Schema::build(
        EdgeProbe { edge: dangling },
        EmptyMutation,
        EmptySubscription,
    )
    .finish();
    let answer = schema
        .execute(Request::new("{ edge { target { name } targetName } }"))
        .await;
    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
    let json = serde_json::to_value(&answer.data).expect("data serializes");
    assert!(json["edge"]["target"].is_null());
    assert_eq!(json["edge"]["targetName"], "nowhere");
}
