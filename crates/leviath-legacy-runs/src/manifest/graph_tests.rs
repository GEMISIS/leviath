//! Reading parsed manifests as run graphs: caller-input seeds, and the
//! tables the parsed form does not carry.

use leviath_runtime::spec::graph::{
    McpTransport, RunGraph, ScriptPermission, ScriptPermissionsDef,
};
use leviath_runtime::spec::issues::IssueCode;

use super::{parse_manifest, read_manifest_tables};

#[test]
fn caller_input_seeds_become_text_inputs() {
    let bp = parse_manifest(
        r#"
[agent]
name = "t"
version = "1.0.0"
description = "d"

[context.regions]
task = { kind = "pinned", max_tokens = 100, required = true, seed = "task" }
notes = { kind = "pinned", max_tokens = 100, seed = "input" }

[stages.main]
system_prompt = "p"
"#,
    )
    .unwrap();
    let g = crate::old::graph::from_blueprint(&bp).unwrap();
    let names: Vec<(&str, bool)> = g
        .inputs
        .iter()
        .map(|i| (i.name.as_str(), i.required))
        .collect();
    assert_eq!(names, vec![("notes", false), ("task", true)]);
    assert!(g.layout.regions.iter().all(|r| r.seed.is_none()));
    assert_eq!(g.title.as_deref(), Some("t"));
}

#[test]
fn a_manifests_own_mcp_servers_and_script_permissions_are_read_into_the_graph() {
    let manifest = "[agent]\nname = \"t\"\nversion = \"1\"\n\
        [stages.main]\nsystem_prompt = \"p\"\n\
        [[mcp_servers]]\nname = \"srv\"\ncommand = \"python3\"\nargs = [\"-m\", \"srv\"]\n\
        env = { TOKEN = \"x\" }\n\
        [[mcp_servers]]\nname = \"web\"\ntransport = \"http\"\nurl = \"https://mcp.example\"\n\
        [tool_script_permissions]\nshell = \"deny\"\nhttp_get = \"inherit\"\n";
    let bp = parse_manifest(manifest).unwrap();
    let mut g = crate::old::graph::from_blueprint(&bp).unwrap();
    assert!(g.mcp_servers.is_empty());
    read_manifest_tables(&mut g, manifest).unwrap();
    assert_eq!(g.mcp_servers.len(), 2);
    assert_eq!(g.mcp_servers[0].name.as_str(), "srv");
    assert_eq!(g.mcp_servers[0].args, ["-m", "srv"]);
    assert_eq!(g.mcp_servers[0].env["TOKEN"], "x");
    assert_eq!(g.mcp_servers[1].transport, Some(McpTransport::Http));
    assert_eq!(
        g.script_permissions,
        ScriptPermissionsDef {
            shell: Some(ScriptPermission::Deny),
            http_get: Some(ScriptPermission::Inherit),
            ..ScriptPermissionsDef::default()
        }
    );
    let back: RunGraph = toml::from_str(&toml::to_string(&g).unwrap()).unwrap();
    assert_eq!(back, g, "both read back from a blueprint file as written");

    let mut plain = g.clone();
    plain.mcp_servers.clear();
    read_manifest_tables(&mut plain, "[agent]\nname = \"t\"\n").unwrap();
    assert!(
        plain.mcp_servers.is_empty(),
        "nothing declared, nothing read"
    );

    let bad = "[[mcp_servers]]\nname = \"bad name\"\n\
        [tool_script_permissions]\nshell = \"sometimes\"\n";
    let issues = read_manifest_tables(&mut g.clone(), bad).unwrap_err();
    let paths: Vec<String> = issues.iter().map(|i| i.path.to_string()).collect();
    assert_eq!(paths, ["mcp_servers", "script_permissions"]);
    let unreadable = read_manifest_tables(&mut g, "not [ toml").unwrap_err();
    assert_eq!(unreadable.0[0].code, IssueCode::Invalid);
}
