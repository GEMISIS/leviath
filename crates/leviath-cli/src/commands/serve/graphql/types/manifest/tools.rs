//! What a stage may call, what it may be handed, and where the answers land.
//!
//! The tool in each of these is a name rather than a `Tool`. A manifest names
//! tools no inventory can describe: an MCP server's, which this machine's
//! inventory deliberately leaves out, a group token such as `@builtin`, and any
//! tool an author wrote that is not installed here. `Query.tools` is what
//! describes the ones that are.

use std::sync::Arc;

use async_graphql::{Enum, Object, SimpleObject};
use leviath_graphql_derive::mirror;

use crate::commands::serve::core::blueprints::ParsedBlueprint;

use super::super::blueprint::Region;
use super::count;
use super::refs;

/// What a stage does with one tool.
#[mirror]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum ToolPermissionPolicy {
    /// Runs without asking.
    Allow,
    /// Parks the run until a person answers.
    Ask,
    /// Refused at dispatch.
    Deny,
}

impl ToolPermissionPolicy {
    /// The policy a manifest's word resolves to, exactly as the daemon resolves
    /// it.
    ///
    /// The manifest parser refuses any word but `allow`, `ask` and `deny`, so a
    /// parsed manifest only ever carries one of the three. The daemon's own
    /// resolution reads anything else as `ASK`, and this reads it the same way,
    /// so the schema cannot report a permission the dispatcher would not apply.
    pub(crate) fn of(word: &str) -> Self {
        match word.trim().to_ascii_lowercase().as_str() {
            "allow" => Self::Allow,
            "deny" => Self::Deny,
            _ => Self::Ask,
        }
    }
}

impl From<leviath_core::policy::ToolPolicy> for ToolPermissionPolicy {
    fn from(policy: leviath_core::policy::ToolPolicy) -> Self {
        use leviath_core::policy::ToolPolicy as Core;
        match policy {
            Core::Allow => Self::Allow,
            Core::Ask => Self::Ask,
            Core::Deny => Self::Deny,
        }
    }
}

/// One tool and what this level does with it.
#[mirror(list)]
#[derive(Debug, SimpleObject)]
pub(crate) struct ToolPermissionRule {
    /// The tool, by the name the manifest used. A name rather than a `Tool`: a
    /// rule may name an MCP server's tool, a group token, or one this machine
    /// does not have, and the rule still applies to the name it wrote.
    pub(crate) tool: String,
    /// What happens when it is called.
    pub(crate) policy: ToolPermissionPolicy,
}

impl ToolPermissionRule {
    /// Read a permission table, sorted by tool name, so two reads of one
    /// blueprint list the rules in the same order.
    pub(crate) fn from_table(
        table: &std::collections::BTreeMap<
            leviath_runtime::spec::names::ToolName,
            leviath_core::policy::ToolPolicy,
        >,
    ) -> Vec<ToolPermissionRule> {
        table
            .iter()
            .map(|(tool, policy)| ToolPermissionRule {
                tool: tool.to_string(),
                policy: ToolPermissionPolicy::from(*policy),
            })
            .collect()
    }
}

/// The resolver state behind the `ToolRouteOverride` type.
pub(crate) struct ToolRouteOverride {
    /// The blueprint the region name resolves in.
    blueprint: Arc<ParsedBlueprint>,
    /// The tool this override is keyed by.
    tool: String,
    /// The region name it was written with.
    region: String,
}

/// Where one tool's results go, in place of the stage's default region.
#[mirror(list)]
#[Object]
impl ToolRouteOverride {
    /// The tool, by the name the manifest used. A name rather than a `Tool`: an
    /// override may name an MCP server's tool or one this machine does not have.
    async fn tool(&self) -> &str {
        &self.tool
    }

    /// The region its results are written to.
    ///
    /// Null where no layout in this blueprint declares that name, which is an
    /// override sending results nowhere the stage can read: `lev validate`
    /// refuses it and the daemon will not spawn it. `regionName` carries the name
    /// either way.
    async fn region(&self) -> Option<Region> {
        refs::region(&self.blueprint, &self.region)
    }

    /// The region name the override was written with, verbatim.
    async fn region_name(&self) -> &str {
        &self.region
    }
}

/// A per-tool ceiling on one result, in place of the stage's own.
///
/// One number for a whole stage cannot fit a stage that both greps, where the
/// answer is small and wanted whole, and reads files, where it can be enormous.
#[mirror(list)]
#[derive(Debug, SimpleObject)]
pub(crate) struct ToolTokenCeiling {
    /// The tool, by the name the manifest used. A name rather than a `Tool`: a
    /// ceiling may name an MCP server's tool or one this machine does not have.
    pub(crate) tool: String,
    /// The most tokens one of its results may take.
    pub(crate) max_result_tokens: i32,
}

/// The resolver state behind the `ToolRouting` type.
pub(crate) struct ToolRouting {
    /// The blueprint the region names resolve in.
    blueprint: Arc<ParsedBlueprint>,
    /// The routing block as the stage wrote it.
    routing: leviath_runtime::spec::graph::ToolRoutingDef,
}

/// Where a stage's tool results land in its context.
#[mirror]
#[Object]
impl ToolRouting {
    /// The region results go to when no override names another.
    ///
    /// Null where no layout in this blueprint declares that name, which is a
    /// stage whose results land nowhere it can read. `defaultRegionName` carries
    /// the name either way.
    async fn default_region(&self) -> Option<Region> {
        refs::region(&self.blueprint, self.routing.default_region.as_str())
    }

    /// The default region's name, verbatim.
    async fn default_region_name(&self) -> &str {
        self.routing.default_region.as_str()
    }

    /// Tools whose results go somewhere else, sorted by tool name so two reads
    /// of one blueprint cannot disagree about the order.
    async fn overrides(&self) -> Vec<ToolRouteOverride> {
        self.routing
            .tool_regions
            .iter()
            .map(|(tool, region)| ToolRouteOverride {
                blueprint: Arc::clone(&self.blueprint),
                tool: tool.to_string(),
                region: region.to_string(),
            })
            .collect()
    }

    /// Whether a tool's result stays in the region it was routed to, rather
    /// than going to `scratch` where the stage can drop it.
    async fn keep_results(&self) -> bool {
        self.routing.keep_results
    }

    /// The most tokens any one result may take, before truncation.
    async fn max_result_tokens(&self) -> Option<i32> {
        self.routing.max_result_tokens.map(count)
    }

    /// Tools with a ceiling of their own, sorted by tool name.
    async fn max_result_tokens_per_tool(&self) -> Vec<ToolTokenCeiling> {
        self.routing
            .tool_max_result_tokens
            .iter()
            .map(|(tool, tokens)| ToolTokenCeiling {
                tool: tool.to_string(),
                max_result_tokens: count(*tokens),
            })
            .collect()
    }
}

impl ToolRouting {
    /// Describe one stage's routing block against the blueprint that holds it.
    pub(crate) fn of(
        blueprint: &Arc<ParsedBlueprint>,
        routing: &leviath_runtime::spec::graph::ToolRoutingDef,
    ) -> Self {
        Self {
            blueprint: Arc::clone(blueprint),
            routing: routing.clone(),
        }
    }
}

/// The resolver state behind the `OutputRoute` type.
pub(crate) struct OutputRoute {
    /// The blueprint the region name resolves in.
    blueprint: Arc<ParsedBlueprint>,
    /// The mime pattern this rule matches.
    pattern: String,
    /// The region name it was written with.
    region: String,
}

/// Where the parts a stage produces are written, by mime pattern.
#[mirror(list)]
#[Object]
impl OutputRoute {
    /// The mime pattern this rule matches: `image/png`, `image/*` or `*/*`. The
    /// most specific match wins.
    async fn pattern(&self) -> &str {
        &self.pattern
    }

    /// The region matching parts are written to.
    ///
    /// Null where no layout in this blueprint declares that name, which is a
    /// route whose parts would land nowhere: `lev validate` refuses it and the
    /// daemon will not spawn it. `regionName` carries the name either way.
    async fn region(&self) -> Option<Region> {
        refs::region(&self.blueprint, &self.region)
    }

    /// The region name the route was written with, verbatim.
    async fn region_name(&self) -> &str {
        &self.region
    }
}

impl OutputRoute {
    /// Describe one output route against the blueprint that holds it.
    pub(crate) fn of(blueprint: &Arc<ParsedBlueprint>, pattern: &str, region: &str) -> Self {
        Self {
            blueprint: Arc::clone(blueprint),
            pattern: pattern.to_string(),
            region: region.to_string(),
        }
    }
}

/// What one tool may be handed at this stage.
///
/// A stored part outside a tool's list is out of that tool's reach here. A tool
/// absent from the table has no limit beyond what it takes itself, and inline
/// text is never hidden by one.
#[mirror(list)]
#[derive(Debug, SimpleObject)]
pub(crate) struct ToolAcceptRule {
    /// The tool, by the name the manifest used. A name rather than a `Tool`: a
    /// rule may name an MCP server's tool or one this machine does not have.
    pub(crate) tool: String,
    /// The mime patterns it may be handed.
    pub(crate) patterns: Vec<String>,
}

#[cfg(test)]
#[path = "tools_tests.rs"]
mod tests;
