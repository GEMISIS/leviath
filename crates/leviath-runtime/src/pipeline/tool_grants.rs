//! A stage's tool grants: which of the tools a run could offer a stage's
//! `available_tools`, groups and connectors select, and which of those an
//! unattended run takes away.
//!
//! Split from the stage's model resolution (`resolve`) because the two answer
//! different questions about a stage: what it runs on, and what it may call.

use crate::spec::graph::ToolGroup;
use leviath_providers::Tool;

/// Which MCP server advertises each tool: advertised name -> server name.
///
/// Built where the servers are registered, because that is the only place the
/// mapping exists. A [`Tool`] carries a name, a description and a schema, and
/// although `leviath-mcp` always advertises exactly `<server>__<tool>`, the
/// server half cannot be read back out of it: a server name may itself contain
/// `_`, so `a__b__c` is either `a`'s `b__c` or `a__b`'s `c`. There is no string
/// pattern that reliably answers "does this tool belong to tracker". A grant
/// names the server and this table answers for it. It is also how the filter
/// tells an MCP def from a script def, which nothing on the [`Tool`] itself
/// records.
pub type ToolOwners = std::collections::HashMap<String, String>;

/// Where a run-time tool def came from, which is what a group grant selects on.
///
/// Decided by name, because a [`Tool`] carries no provenance: the built-in and
/// sub-agent names are fixed lists, an MCP name is in `owners`, and script
/// discovery drops any script whose name one of those already owns, so what
/// is left can only be a script.
pub fn tool_source(name: &str, owners: &ToolOwners) -> ToolGroup {
    if leviath_tools::is_builtin_tool(name) {
        ToolGroup::Builtin
    } else if leviath_tools::is_subagent_tool(name) {
        ToolGroup::Subagent
    } else if owners.contains_key(name) {
        ToolGroup::Mcp
    } else {
        ToolGroup::Scripts
    }
}

/// Every tool a run could offer, and which MCP server each came from.
///
/// The two travel together because a connector grant needs both: the defs to
/// filter, and the ownership to know what a server's name covers. Passed as one
/// value rather than two adjacent `&`s: two references of different types
/// next to each other is exactly the pair a reader transposes.
#[derive(Clone, Copy)]
pub struct ToolCatalog<'a> {
    /// Every tool definition available to the run.
    pub defs: &'a [Tool],
    /// Which MCP server advertises each of them. Empty for a run with no MCP
    /// servers, which makes every connector grant resolve to nothing.
    pub owners: &'a ToolOwners,
}

/// A stage's Layer-1 grant list: the names it asked for, plus every tool
/// belonging to a server it named.
///
/// Connector grants are resolved here rather than at parse time because the
/// answer only exists once the servers are connected - which is the whole point
/// of naming a server instead of its tools. A connector that resolves to
/// nothing (server not installed, or not connected this run) contributes
/// nothing, exactly as an `available_tools` name matching nothing does.
///
/// Order is `available_tools` first, then each connector's tools in the order
/// the servers were named, and the result is de-duplicated: a tool named
/// individually *and* covered by a connector is granted once.
pub fn expand_connector_grants(
    available: &[String],
    connectors: &[String],
    owners: &ToolOwners,
) -> Vec<String> {
    if connectors.is_empty() {
        return available.to_vec();
    }
    let mut granted: Vec<String> = available.to_vec();
    for server in connectors {
        // Sorted so a stage's advertised tool order does not depend on hash
        // iteration - the model sees this list, and a set that reshuffles
        // between runs is a difference nobody can explain.
        let mut owned: Vec<&String> = owners
            .iter()
            .filter(|(_, owner)| *owner == server)
            .map(|(tool, _)| tool)
            .collect();
        owned.sort();
        for tool in owned {
            if !granted.contains(tool) {
                granted.push(tool.clone());
            }
        }
    }
    granted
}

/// Filter the catalog's tool defs down to those a stage's `available_tools`
/// grants: the names it lists (alias-resolved), plus every def whose source a
/// listed group covers. Shared by spawn-time stage resolution and the mid-run
/// tool-service refresh so both apply Layer-1 identically.
///
/// Connector grants are expanded into `available` by
/// [`expand_connector_grants`] before this sees it. Group grants are *not*
/// pre-expanded, on purpose: a `dynamic_tools` agent that grants `@scripts`
/// installs a tool mid-run and expects the next refresh to offer it, which
/// only works if the group is still a group when the refresh filters.
///
/// The stage-control tools (`submit_output`, `fan_out`) are never granted by
/// a group; the stage mode writes them into the list by name.
pub(crate) fn filter_tools_by_available(
    catalog: ToolCatalog<'_>,
    available: &[String],
) -> Vec<Tool> {
    if available.is_empty() {
        return Vec::new();
    }
    let all = catalog.defs;
    let groups = ToolGroup::named_in(available);
    let wanted: std::collections::HashSet<&str> = available
        .iter()
        .filter(|n| !ToolGroup::is_token(n))
        .map(|n| leviath_tools::canonical_tool_name(n))
        .collect();
    // Names that match nothing get one more chance, as a server-qualified MCP
    // tool. Computed once rather than per candidate, and only from the names
    // that actually missed - so a stage whose grants all resolve does no extra
    // work.
    let unmatched: Vec<&str> = wanted
        .iter()
        .copied()
        .filter(|n| !all.iter().any(|t| t.name == *n))
        .collect();
    let recovered = recover_unqualified(all, &unmatched);
    let grouped = |name: &str| {
        !groups.is_empty()
            && !leviath_tools::STAGE_CONTROL_TOOLS.contains(&name)
            && groups
                .iter()
                .any(|g| g.covers(tool_source(name, catalog.owners)))
    };
    all.iter()
        .filter(|t| {
            wanted.contains(t.name.as_str())
                || recovered.contains(t.name.as_str())
                || grouped(&t.name)
        })
        .cloned()
        .collect()
}

/// MCP tools named without their server, matched to the qualified name when
/// exactly one server offers them.
///
/// MCP tools are advertised as `<server>__<tool>`, so a blueprint written
/// against bare names says `create_issue` where the tool is
/// `github__create_issue`. Left alone that grant matches nothing and the tool
/// is silently not offered.
///
/// Three things keep this from doing harm:
///
/// - It only ever sees names that matched **nothing**. A built-in `read_file`
///   matches itself, so a server that also offers `read_file` can never capture
///   the grant.
/// - It requires exactly one candidate. Two servers offering `create_issue`
///   leave the name unresolved, because it genuinely is ambiguous and the
///   manifest has to say which. A silent "whichever registered first" is what
///   qualified names exist to prevent.
/// - It matches on the qualified shape, so `create_issue` finds
///   `github__create_issue` and not some unrelated tool ending in those
///   characters.
fn recover_unqualified<'a>(
    all: &'a [Tool],
    unmatched: &[&str],
) -> std::collections::HashSet<&'a str> {
    let mut recovered = std::collections::HashSet::new();
    for name in unmatched {
        let suffix = format!("__{name}");
        let mut candidates = all
            .iter()
            .filter(|t| t.name.ends_with(&suffix) && t.name.len() > suffix.len());
        let (Some(only), None) = (candidates.next(), candidates.next()) else {
            continue;
        };
        tracing::debug!(
            wrote = %name,
            resolved = %only.name,
            "a stage names an MCP tool without its server; resolved because exactly one offers it"
        );
        recovered.insert(only.name.as_str());
    }
    recovered
}

/// The stage's Layer-1 tool set for a run that may have nobody watching.
///
/// Same filter as `filter_tools_by_available`, then - for an unattended run -
/// minus every tool whose only outcome is a prompt for a person
/// ([`BLOCKING_INTERACTION_TOOLS`](crate::dynamic_interaction::BLOCKING_INTERACTION_TOOLS)),
/// unless the stage named it in `required_tools`.
///
/// Dropping the definition rather than auto-answering the call is what makes the
/// difference visible to the model: it never sees the tool, so it decides for
/// itself instead of spending a round trip to be told nobody is there. A call
/// that arrives anyway (a model repeating itself from context) meets the ordinary
/// unoffered-tool refusal.
pub fn filter_tools_for_stage(
    catalog: ToolCatalog<'_>,
    available: &[String],
    required: &[String],
    unattended: bool,
) -> Vec<Tool> {
    let mut tools = filter_tools_by_available(catalog, available);
    if unattended {
        tools.retain(|t| {
            !crate::dynamic_interaction::BLOCKING_INTERACTION_TOOLS.contains(&t.name.as_str())
                || required
                    .iter()
                    .any(|n| leviath_tools::canonical_tool_name(n) == t.name)
        });
    }
    tools
}
