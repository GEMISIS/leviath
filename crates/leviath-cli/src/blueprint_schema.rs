//! The blueprint schema written for this machine, `<data dir>/agent.schema.json`.
//!
//! The published schema (`docs/schema/blueprint.schema.json`) describes the
//! format. This one is the same schema with the names this machine knows
//! added as completions: its tool names, the MCP tools a server has listed,
//! and its provider names. An editor pointed at it suggests them while still
//! taking any other name, because the loader takes names it cannot check
//! offline (an MCP tool of a server not reached yet, a provider a script adds).
//!
//! MCP tool names are known only once a server has been reached, so
//! `lev mcp test` records the names it listed under `x-leviath-mcp-tools`
//! in the file, and every later write keeps them for the servers still
//! configured.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::Config;

/// The schema's file name inside the data directory.
pub(crate) const FILE_NAME: &str = "agent.schema.json";

/// Where the file records each MCP server's listed tools, by server.
const MCP_KEY: &str = "x-leviath-mcp-tools";

/// A server and the tool names it just listed.
pub(crate) type Listed<'a> = (&'a str, &'a [String]);

/// The schema for the data directory `data_dir` and `config`, with `listed`
/// recorded as that server's tools.
pub(crate) fn render(
    data_dir: &Path,
    config: &Config,
    listed: Option<Listed<'_>>,
) -> serde_json::Value {
    let mut mcp = remembered(&data_dir.join(FILE_NAME));
    mcp.retain(|server, _| config.mcp_servers.iter().any(|s| &s.name == server));
    if let Some((server, tools)) = listed {
        mcp.insert(server.to_string(), tools.to_vec());
    }
    let mut tools: Vec<String> =
        crate::tool_inventory::ToolInventory::discover_in(None, None, Some(data_dir.join("tools")))
            .names()
            .into_iter()
            .collect();
    tools.extend(mcp.iter().flat_map(|(server, names)| {
        names
            .iter()
            .map(|tool| leviath_core::mcp_names::advertised_name(server, tool))
    }));
    let mut providers: Vec<String> = leviath_providers::compiled::COMPILED
        .iter()
        .map(ToString::to_string)
        .collect();
    providers.extend(config.model_providers.keys().cloned());
    providers.extend(script_providers(&data_dir.join("providers")));
    let mut schema =
        leviath_blueprint::schema_with(&leviath_blueprint::SchemaHints { tools, providers });
    schema[MCP_KEY] = serde_json::json!(mcp);
    schema
}

/// Write [`render`] to `<data_dir>/agent.schema.json`, and say where.
pub(crate) fn write(
    data_dir: &Path,
    config: &Config,
    listed: Option<Listed<'_>>,
) -> std::io::Result<PathBuf> {
    let schema = render(data_dir, config, listed);
    let text = serde_json::to_string_pretty(&schema).expect("a schema is plain JSON") + "\n";
    let path = data_dir.join(FILE_NAME);
    std::fs::create_dir_all(data_dir).and_then(|()| std::fs::write(&path, text))?;
    Ok(path)
}

/// [`write`], for a command whose own work is done: a failure is a warning
/// on stderr, never the command's failure.
pub(crate) fn refresh(data_dir: Option<&Path>, config: &Config, listed: Option<Listed<'_>>) {
    if let Some(dir) = data_dir
        && let Err(e) = write(dir, config, listed)
    {
        eprintln!(
            "Warning: could not update {}: {e}",
            dir.join(FILE_NAME).display()
        );
    }
}

/// The MCP tools an earlier write recorded. A missing or unreadable file
/// records none.
fn remembered(path: &Path) -> BTreeMap<String, Vec<String>> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|schema| serde_json::from_value(schema[MCP_KEY].clone()).ok())
        .unwrap_or_default()
}

/// The provider names the `.rhai` scripts in `dir` answer to: each file's stem.
fn script_providers(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|e| e == "rhai"))
        .filter_map(|path| path.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .collect()
}

#[cfg(test)]
#[path = "blueprint_schema_tests.rs"]
mod tests;
