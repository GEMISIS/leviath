//! The two tables of a manifest that its parsed form does not carry.

use leviath_runtime::spec::graph::{McpServerDef, RunGraph, ScriptPermissionsDef};
use leviath_runtime::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};

/// Read the two tables of a blueprint manifest that [`super::parse_manifest`]
/// does not carry, `[[mcp_servers]]` and `[tool_script_permissions]`, into
/// `graph`. `manifest` is the text `graph`'s blueprint was parsed from. Every
/// entry that does not fit is reported, each at its own path.
pub fn read_manifest_tables(graph: &mut RunGraph, manifest: &str) -> Result<(), SpawnIssues> {
    let at = SpecPath::root;
    let mut issues = SpawnIssues::new();
    let table: toml::Table = match toml::from_str(manifest) {
        Ok(table) => table,
        Err(e) => return Err(SpawnIssue::new(at(), IssueCode::Invalid, e.to_string()).into()),
    };
    if let Some(value) = table.get("mcp_servers") {
        let servers: Result<Vec<McpServerDef>, _> = value.clone().try_into();
        match servers {
            Ok(servers) => graph.mcp_servers = servers,
            Err(e) => issues.push(SpawnIssue::new(
                at().field("mcp_servers"),
                IssueCode::Invalid,
                e.to_string(),
            )),
        }
    }
    if let Some(value) = table.get("tool_script_permissions") {
        let perms: Result<ScriptPermissionsDef, _> = value.clone().try_into();
        match perms {
            Ok(perms) => graph.script_permissions = perms,
            Err(e) => issues.push(SpawnIssue::new(
                at().field("script_permissions"),
                IssueCode::Invalid,
                e.to_string(),
            )),
        }
    }
    issues.into_result(())
}
