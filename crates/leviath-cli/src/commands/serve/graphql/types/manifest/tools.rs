//! What a stage may call, what it may be handed, and where the answers land.

use async_graphql::{Enum, SimpleObject};

use super::count;

/// What a stage does with one tool.
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
    /// Read the manifest's own word, or nothing for a word that is neither.
    ///
    /// A spelling the daemon does not recognise is left out rather than
    /// defaulted: `ALLOW` on a typo would report a permission nobody granted.
    fn parse(word: &str) -> Option<Self> {
        match word.trim().to_ascii_lowercase().as_str() {
            "allow" => Some(Self::Allow),
            "ask" => Some(Self::Ask),
            "deny" => Some(Self::Deny),
            _ => None,
        }
    }
}

/// One tool and what this level does with it.
#[derive(Debug, SimpleObject)]
pub(crate) struct ToolPermissionRule {
    /// The tool, by the name the manifest used.
    pub(crate) tool: String,
    /// What happens when it is called. Null when the manifest's word is one the
    /// daemon does not recognise, which is a rule that grants nothing.
    pub(crate) policy: Option<ToolPermissionPolicy>,
}

impl ToolPermissionRule {
    /// Read a permission table, in a stable order.
    ///
    /// Sorted by tool name because the manifest's own table is a hash map: an
    /// unsorted list would reorder between two reads of one blueprint, and a
    /// client diffing two answers would see changes that are not there.
    pub(crate) fn from_table(
        table: &std::collections::HashMap<String, String>,
    ) -> Vec<ToolPermissionRule> {
        let mut rules: Vec<ToolPermissionRule> = table
            .iter()
            .map(|(tool, policy)| ToolPermissionRule {
                tool: tool.clone(),
                policy: ToolPermissionPolicy::parse(policy),
            })
            .collect();
        rules.sort_by(|a, b| a.tool.cmp(&b.tool));
        rules
    }
}

/// Where one tool's results go, in place of the stage's default region.
#[derive(Debug, SimpleObject)]
pub(crate) struct ToolRouteOverride {
    /// The tool, by canonical name.
    pub(crate) tool: String,
    /// The region its results are written to, by name.
    pub(crate) region: String,
}

/// A per-tool ceiling on one result, in place of the stage's own.
///
/// One number for a whole stage cannot fit a stage that both greps, where the
/// answer is small and wanted whole, and reads files, where it can be enormous.
#[derive(Debug, SimpleObject)]
pub(crate) struct ToolTokenCeiling {
    /// The tool, by canonical name.
    pub(crate) tool: String,
    /// The most tokens one of its results may take.
    pub(crate) max_result_tokens: i32,
}

/// Where a stage's tool results land in its context.
#[derive(Debug, SimpleObject)]
pub(crate) struct ToolRouting {
    /// The region results go to when no override names another.
    pub(crate) default_region: String,
    /// Tools whose results go somewhere else.
    pub(crate) overrides: Vec<ToolRouteOverride>,
    /// Whether a tool's result stays in the region it was routed to, rather
    /// than going to `scratch` where the stage can drop it.
    pub(crate) keep_results: bool,
    /// The most tokens any one result may take, before truncation.
    pub(crate) max_result_tokens: Option<i32>,
    /// Tools with a ceiling of their own.
    pub(crate) max_result_tokens_per_tool: Vec<ToolTokenCeiling>,
}

impl From<&leviath_core::blueprint::ToolResultRouting> for ToolRouting {
    fn from(routing: &leviath_core::blueprint::ToolResultRouting) -> Self {
        let mut overrides: Vec<ToolRouteOverride> = routing
            .tool_overrides
            .iter()
            .map(|(tool, region)| ToolRouteOverride {
                tool: tool.clone(),
                region: region.clone(),
            })
            .collect();
        overrides.sort_by(|a, b| a.tool.cmp(&b.tool));
        let mut ceilings: Vec<ToolTokenCeiling> = routing
            .tool_max_result_tokens
            .iter()
            .map(|(tool, tokens)| ToolTokenCeiling {
                tool: tool.clone(),
                max_result_tokens: count(*tokens),
            })
            .collect();
        ceilings.sort_by(|a, b| a.tool.cmp(&b.tool));
        Self {
            default_region: routing.default_region.clone(),
            overrides,
            keep_results: routing.keep_results,
            max_result_tokens: routing.max_result_tokens.map(count),
            max_result_tokens_per_tool: ceilings,
        }
    }
}

/// Where the parts a stage produces are written, by mime pattern.
#[derive(Debug, SimpleObject)]
pub(crate) struct OutputRoute {
    /// The mime pattern this rule matches: `image/png`, `image/*` or `*/*`. The
    /// most specific match wins.
    pub(crate) pattern: String,
    /// The region matching parts are written to, by name.
    pub(crate) region: String,
}

/// What one tool may be handed at this stage.
///
/// A stored part outside a tool's list is out of that tool's reach here. A tool
/// absent from the table has no limit beyond what it takes itself, and inline
/// text is never hidden by one.
#[derive(Debug, SimpleObject)]
pub(crate) struct ToolAcceptRule {
    /// The tool, by the name the manifest used.
    pub(crate) tool: String,
    /// The mime patterns it may be handed.
    pub(crate) patterns: Vec<String>,
}
