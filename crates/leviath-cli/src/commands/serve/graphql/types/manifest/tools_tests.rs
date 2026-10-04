//! Tests for what a stage may call, what it may be handed, and the mirrors of
//! those types.

use std::sync::Arc;

use crate::commands::serve::core::blueprints::ParsedBlueprint;

use super::{
    OutputRoute, ToolAcceptRule, ToolPermissionPolicy, ToolPermissionRule, ToolRouteOverride,
    ToolRouting, ToolTokenCeiling,
};
use crate::commands::serve::graphql::filter::testkit::{exercise, exercise_enum, exercise_list};

/// A blueprint with the regions these types resolve names against.
fn blueprint() -> Arc<ParsedBlueprint> {
    super::super::parsed(
        r#"[blueprint]
name = "router"
version = "1.0.0"

[graph]
stages = [{ name = "plan" }]
layout = { total_budget_tokens = 200, regions = [{ name = "notes", kind = "temporary", budget = 100 }, { name = "scratch", kind = "temporary", budget = 100 }] }
"#,
    )
}

/// One permission rule, as `from_table` would build it.
fn permission_rule() -> ToolPermissionRule {
    ToolPermissionRule {
        tool: "shell".to_string(),
        policy: ToolPermissionPolicy::Ask,
    }
}

/// One ceiling, as `max_result_tokens_per_tool` would build it.
fn ceiling() -> ToolTokenCeiling {
    ToolTokenCeiling {
        tool: "read_file".to_string(),
        max_result_tokens: 2000,
    }
}

/// One override, resolving to a declared region.
fn route_override(blueprint: &Arc<ParsedBlueprint>) -> ToolRouteOverride {
    ToolRouteOverride {
        blueprint: Arc::clone(blueprint),
        tool: "shell".to_string(),
        region: "notes".to_string(),
    }
}

/// One accept rule, as `tool_accepts` would build it.
fn accept_rule() -> ToolAcceptRule {
    ToolAcceptRule {
        tool: "spawn_agent".to_string(),
        patterns: vec!["image/*".to_string()],
    }
}

/// One routing block with an override and a per-tool ceiling, so both of
/// `ToolRouting`'s list fields carry something.
fn tool_result_routing() -> leviath_runtime::spec::graph::ToolRoutingDef {
    use leviath_runtime::spec::names::{RegionName, ToolName};
    leviath_runtime::spec::graph::ToolRoutingDef {
        default_region: RegionName::new("notes").unwrap(),
        tool_regions: [(
            ToolName::new("shell").unwrap(),
            RegionName::new("notes").unwrap(),
        )]
        .into(),
        keep_results: true,
        max_result_tokens: Some(4000),
        tool_max_result_tokens: [(ToolName::new("read_file").unwrap(), 2000)].into(),
    }
}

/// Every function `#[mirror]` wrote for this file's types runs at least once.
///
/// The mirrors are straight lines of delegation, so running each of them once
/// is enough to measure all of them.
#[tokio::test]
async fn every_mirrored_function_runs() {
    let bp = blueprint();

    exercise_enum(&[ToolPermissionPolicy::Allow, ToolPermissionPolicy::Deny]).await;

    exercise(&[permission_rule()]).await;
    exercise_list(&[permission_rule()]).await;

    exercise(&[ceiling()]).await;
    exercise_list(&[ceiling()]).await;

    exercise(&[route_override(&bp)]).await;
    exercise_list(&[route_override(&bp)]).await;

    exercise(&[accept_rule()]).await;
    exercise_list(&[accept_rule()]).await;

    let routing = ToolRouting::of(&bp, &tool_result_routing());
    exercise(&[routing]).await;

    let route = OutputRoute::of(&bp, "image/*", "notes");
    exercise(&[route]).await;
    exercise_list(&[OutputRoute::of(&bp, "image/*", "notes")]).await;
}

/// A root handing out one override, so a field test is one query.
struct RouteProbe {
    route: ToolRouteOverride,
}

#[async_graphql::Object]
impl RouteProbe {
    /// The override under test.
    async fn route(&self) -> &ToolRouteOverride {
        &self.route
    }
}

/// A dangling region name resolves to nothing, and the name is still served
/// beside it.
#[tokio::test]
async fn a_dangling_region_name_resolves_to_nothing() {
    use async_graphql::{EmptyMutation, EmptySubscription, Request, Schema};

    let bp = blueprint();
    let dangling = ToolRouteOverride {
        blueprint: Arc::clone(&bp),
        tool: "shell".to_string(),
        region: "nowhere".to_string(),
    };
    let schema = Schema::build(
        RouteProbe { route: dangling },
        EmptyMutation,
        EmptySubscription,
    )
    .finish();
    let answer = schema
        .execute(Request::new("{ route { region { name } regionName } }"))
        .await;
    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
    let json = serde_json::to_value(&answer.data).expect("data serializes");
    assert!(json["route"]["region"].is_null());
    assert_eq!(json["route"]["regionName"], "nowhere");
}
