//! Tests for what a blueprint needs on the machine, and for the mirrors of
//! those types.

use std::collections::BTreeMap;

use super::{
    BlueprintDependency, DependencyInstall, DependencyKind, EnvEntry, InstallCommand,
    McpServerTemplate, McpTransport,
};
use crate::commands::serve::graphql::filter::testkit::{exercise, exercise_enum, exercise_list};

/// One MCP server template, the way a manifest's install block writes it.
fn server() -> McpServerTemplate {
    McpServerTemplate {
        transport: Some(McpTransport::Stdio),
        command: Some("docs-mcp".to_string()),
        url: None,
        args: vec!["--stdio".to_string()],
        env: vec![EnvEntry {
            name: "DOCS_TOKEN".to_string(),
            value: "${DOCS_TOKEN}".to_string(),
        }],
        headers: Vec::new(),
    }
}

/// A core dependency of one kind, with the fields every kind shares filled in.
fn core_dependency(
    kind: leviath_core::blueprint::DependencyKind,
) -> leviath_core::blueprint::Dependency {
    leviath_core::blueprint::Dependency {
        name: "thing".to_string(),
        kind,
        required: true,
        remedy: Some("install it".to_string()),
        description: Some("needed for the run".to_string()),
        install: Some(leviath_core::blueprint::DependencyInstall {
            command: Some("npm i -g docs-mcp".to_string()),
            commands: BTreeMap::from([("macos".to_string(), "brew install thing".to_string())]),
            script: Some("install.rhai".to_string()),
            server: Some(leviath_core::blueprint::McpServerTemplate {
                transport: Some("stdio".to_string()),
                command: Some("docs-mcp".to_string()),
                url: None,
                args: vec!["--stdio".to_string()],
                headers: BTreeMap::new(),
                env: BTreeMap::from([("DOCS_TOKEN".to_string(), "${DOCS_TOKEN}".to_string())]),
            }),
        }),
    }
}

/// Every kind a manifest can declare maps to the one field set that kind uses,
/// and the shared fields and the install block carry over untouched.
#[test]
fn every_dependency_kind_maps_to_its_own_fields() {
    use leviath_core::blueprint::DependencyKind as Core;

    let mcp = BlueprintDependency::from(&core_dependency(Core::McpServer {
        server: "docs".to_string(),
        env: vec!["DOCS_TOKEN".to_string()],
    }));
    assert_eq!(mcp.kind, DependencyKind::McpServer);
    assert_eq!(mcp.server, Some("docs".to_string()));
    assert_eq!(mcp.env, vec!["DOCS_TOKEN".to_string()]);
    assert_eq!(mcp.name, "thing");
    assert_eq!(mcp.remedy, Some("install it".to_string()));
    assert_eq!(mcp.description, Some("needed for the run".to_string()));
    let install = mcp.install.expect("an install block");
    assert_eq!(install.command, Some("npm i -g docs-mcp".to_string()));
    assert_eq!(
        install.commands[0].command,
        "brew install thing".to_string()
    );
    assert_eq!(install.script, Some("install.rhai".to_string()));
    let server = install.server.expect("a server template");
    assert_eq!(server.transport, Some(McpTransport::Stdio));
    assert_eq!(server.env[0].name, "DOCS_TOKEN");

    let env = BlueprintDependency::from(&core_dependency(Core::Env {
        var: "API_KEY".to_string(),
    }));
    assert_eq!(env.kind, DependencyKind::Env);
    assert_eq!(env.var, Some("API_KEY".to_string()));
    assert!(env.server.is_none());

    let binary = BlueprintDependency::from(&core_dependency(Core::Binary {
        command: "blender".to_string(),
    }));
    assert_eq!(binary.kind, DependencyKind::Binary);
    assert_eq!(binary.command, Some("blender".to_string()));
    assert!(binary.var.is_none());

    let script = BlueprintDependency::from(&core_dependency(Core::Script {
        check: "checks/thing.rhai".to_string(),
    }));
    assert_eq!(script.kind, DependencyKind::Script);
    assert_eq!(script.check, Some("checks/thing.rhai".to_string()));
    assert!(script.command.is_none());
}

/// A transport word the daemon knows reads back as its enum value; anything
/// else is left out rather than guessed at.
#[test]
fn a_transport_reads_back_or_is_left_out() {
    let template = |transport: Option<&str>| leviath_core::blueprint::McpServerTemplate {
        transport: transport.map(str::to_string),
        command: None,
        url: None,
        args: Vec::new(),
        headers: BTreeMap::new(),
        env: BTreeMap::new(),
    };
    assert_eq!(
        McpServerTemplate::from(&template(Some("HTTP"))).transport,
        Some(McpTransport::Http),
        "case-insensitive, like the manifest reader"
    );
    assert!(
        McpServerTemplate::from(&template(Some("carrier-pigeon")))
            .transport
            .is_none()
    );
}

/// Every function `#[mirror]` wrote for this file's types runs at least once.
///
/// The mirrors are straight lines of delegation, so running each of them once
/// is enough to measure all of them.
#[tokio::test]
async fn every_mirrored_function_runs() {
    exercise_enum(&[
        DependencyKind::McpServer,
        DependencyKind::Env,
        DependencyKind::Binary,
        DependencyKind::Script,
    ])
    .await;
    exercise_enum(&[McpTransport::Stdio, McpTransport::Http]).await;

    let entries = vec![EnvEntry {
        name: "DOCS_TOKEN".to_string(),
        value: "secret".to_string(),
    }];
    exercise(&entries).await;
    exercise_list(&entries).await;

    let commands = vec![InstallCommand {
        os: "macos".to_string(),
        command: "brew install thing".to_string(),
    }];
    exercise(&commands).await;
    exercise_list(&commands).await;

    exercise(&[server()]).await;

    exercise(&[DependencyInstall {
        command: Some("npm i -g docs-mcp".to_string()),
        commands: vec![InstallCommand {
            os: "linux".to_string(),
            command: "apt install docs-mcp".to_string(),
        }],
        script: Some("install.rhai".to_string()),
        server: Some(server()),
    }])
    .await;

    let dependency = BlueprintDependency {
        name: "docs".to_string(),
        kind: DependencyKind::McpServer,
        required: true,
        remedy: Some("configure the docs server".to_string()),
        description: Some("looks things up".to_string()),
        install: Some(DependencyInstall {
            command: None,
            commands: Vec::new(),
            script: None,
            server: Some(server()),
        }),
        server: Some("docs".to_string()),
        env: vec!["DOCS_TOKEN".to_string()],
        var: None,
        command: None,
        check: None,
    };
    exercise(std::slice::from_ref(&dependency)).await;
    exercise_list(&[dependency]).await;
}
