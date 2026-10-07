//! What the editor's choosers offer: the mime types a type field lists,
//! and the tools a stage may be granted (the groups, the MCP servers as
//! connectors, every tool this install has, and each server's tools as it
//! answers).

use super::{McpCatalog, McpServerTools};
use crate::blueprint_edit::ManifestDoc;

/// The mime types the choosers offer: every family, then every type the
/// registry knows, read the way the daemon reads it (the compiled defaults,
/// the config's rows, `mime_types.toml`) with the blueprint's own rows on
/// top, the way its runs read it.
pub(super) fn mime_type_options(config_path: &std::path::Path, doc: &ManifestDoc) -> Vec<String> {
    let mut registry = crate::config::Config::load_from_path_public(config_path)
        .ok()
        .and_then(|c| c.mime_registry().ok())
        .unwrap_or_else(leviath_core::mime::MimeRegistry::builtin);
    for key in crate::blueprint_edit::mime_type_keys(doc) {
        let _ = registry.layer(
            &toml::Table::from_iter([(key, toml::Value::Table(toml::Table::new()))]),
            "blueprint",
        );
    }
    let mut out: Vec<String> = [
        "*/*",
        "text/*",
        "image/*",
        "audio/*",
        "video/*",
        "application/*",
        "model/*",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let mut keys: Vec<String> = registry
        .keys()
        .into_iter()
        .map(|(key, _)| key)
        .filter(|key| !out.contains(key))
        .collect();
    keys.sort();
    out.extend(keys);
    out
}

/// One row of the tools chooser: a name the stage may write, and what it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::commands::dashboard) struct ToolChoice {
    /// The `available_tools` entry (a tool name or a group token), or the
    /// server's name for a connector.
    pub(in crate::commands::dashboard) name: String,
    /// Where it comes from, or what the group reaches, in a few words.
    pub(in crate::commands::dashboard) detail: String,
    /// An MCP server whose whole tool set the stage may use: written to
    /// `available_connectors`, not `available_tools`.
    pub(in crate::commands::dashboard) connector: bool,
}

/// The tool groups a stage's `tools` can name, as the file spells them, with
/// what each covers, in the order the chooser lists them.
pub(super) const TOOL_GROUPS: [(&str, &str); 5] = [
    (
        "@all",
        "every tool this install has: built in, sub-agent, scripts, and MCP",
    ),
    ("@builtin", "every tool compiled into Leviath"),
    (
        "@subagent",
        "the sub-agent tools: spawn, validate, check, wait for, message and kill a child, and read the spawn schema, a blueprint and a run's history",
    ),
    (
        "@scripts",
        "every Rhai script tool, the agent's own and the global ones",
    ),
    ("@mcp", "every tool every connected MCP server advertises"),
];

/// Everything the tools chooser offers for the agent at `dir`: the group
/// tokens first, since "all the built-ins" is the usual answer, then each
/// MCP server as a connector (every tool it advertises, now or later), then
/// the tools this install has (built in, scripts under the agent and the
/// global directory, and each MCP server's tools by their `server__tool`
/// name once the server has answered), then whatever the agent names
/// that was not found.
pub(super) fn tool_choices(
    dir: &std::path::Path,
    name: &str,
    doc: &ManifestDoc,
    mcp: &McpCatalog,
) -> Vec<ToolChoice> {
    let mut choices: Vec<ToolChoice> = TOOL_GROUPS
        .iter()
        .map(|(token, detail)| ToolChoice {
            name: token.to_string(),
            detail: detail.to_string(),
            connector: false,
        })
        .collect();
    for (server, state) in mcp {
        let detail = match state {
            McpServerTools::Pending => {
                "MCP server: every tool it advertises; asking it for the list".to_string()
            }
            McpServerTools::Listed(tools) => format!(
                "MCP server: every tool it advertises, {} now",
                match tools.len() {
                    1 => "1 tool".to_string(),
                    n => format!("{n} tools"),
                }
            ),
            McpServerTools::Failed(why) => {
                format!("MCP server: every tool it advertises; could not list them: {why}")
            }
        };
        choices.push(ToolChoice {
            name: server.clone(),
            detail,
            connector: true,
        });
    }
    let inventory = crate::tool_inventory::ToolInventory::discover(Some(dir), Some(name));
    let mut named: Vec<ToolChoice> = inventory
        .tools
        .iter()
        .map(|t| ToolChoice {
            name: t.name.clone(),
            detail: t.source.describe().to_string(),
            connector: false,
        })
        .collect();
    for (server, state) in mcp {
        let McpServerTools::Listed(tools) = state else {
            continue;
        };
        for tool in tools {
            named.push(ToolChoice {
                name: mcp_tool_name(server, tool),
                detail: format!("{server}'s tool, over MCP"),
                connector: false,
            });
        }
    }
    for tool in doc.known_tools() {
        if !tool.starts_with('@') && !named.iter().any(|t| t.name == tool) {
            named.push(ToolChoice {
                name: tool,
                detail: "named by this agent, not found on this install".to_string(),
                connector: false,
            });
        }
    }
    named.sort_by(|a, b| a.name.cmp(&b.name));
    named.dedup_by(|a, b| a.name == b.name);
    choices.extend(named);
    choices
}

/// The name a stage writes for one of an MCP server's tools: the name the
/// daemon advertises it under, `<server>__<tool>` with every character a
/// provider refuses replaced.
pub(super) fn mcp_tool_name(server: &str, tool: &str) -> String {
    leviath_core::mcp_names::advertised_name(server, tool)
}
