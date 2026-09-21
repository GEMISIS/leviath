//! What a blueprint needs on the machine before it will run.
//!
//! Declared rather than discovered, so a spawn fails with the reason instead of
//! a stage failing halfway through for want of a program.

use async_graphql::{Enum, SimpleObject};

/// What kind of thing a dependency is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum DependencyKind {
    /// An MCP server that must be in the config, with its variables set.
    McpServer,
    /// An environment variable that must be set and not empty.
    Env,
    /// A program that must resolve on the path.
    Binary,
    /// A condition a script decides.
    Script,
}

/// One name and value, for a server's environment or headers.
#[derive(Debug, SimpleObject)]
pub(crate) struct EnvEntry {
    /// The variable or header name.
    pub(crate) name: String,
    /// Its value, as the blueprint wrote it. A `${VAR}` reference stands for a
    /// secret the installer asks for, so the secret itself never ships in a
    /// manifest and never appears here.
    pub(crate) value: String,
}

/// A shell command for one operating system.
#[derive(Debug, SimpleObject)]
pub(crate) struct InstallCommand {
    /// Which system it is for: `macos`, `linux` or `windows`.
    pub(crate) os: String,
    /// The command line.
    pub(crate) command: String,
}

/// The transport an MCP server is reached over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum McpTransport {
    /// A program the daemon launches and talks to over its own pipes.
    Stdio,
    /// An HTTP endpoint.
    Http,
}

/// An MCP server a blueprint can ship, for `lev deps install` to write into the
/// config.
///
/// The settings that are the same for everybody. A secret is never here: it is
/// named in the dependency's variables and asked for when it is installed.
#[derive(Debug, SimpleObject)]
pub(crate) struct McpServerTemplate {
    /// How it is reached. Null when the manifest leaves it to be inferred from
    /// whichever of the command and the URL it gives.
    pub(crate) transport: Option<McpTransport>,
    /// The program to launch, for a stdio server.
    pub(crate) command: Option<String>,
    /// The endpoint, for an HTTP server.
    pub(crate) url: Option<String>,
    /// Arguments passed to the program.
    pub(crate) args: Vec<String>,
    /// The child process's environment, for a stdio server.
    pub(crate) env: Vec<EnvEntry>,
    /// Headers sent with every request, for an HTTP server.
    pub(crate) headers: Vec<EnvEntry>,
}

/// How a dependency can be put in place.
///
/// Nothing here runs by itself. Each option changes the machine, by running a
/// command, running a script, or writing a server into the config, so
/// `lev deps install` asks first every time.
#[derive(Debug, SimpleObject)]
pub(crate) struct DependencyInstall {
    /// One command for any system.
    pub(crate) command: Option<String>,
    /// Commands for particular systems, preferred over `command` on a match.
    pub(crate) commands: Vec<InstallCommand>,
    /// A Rhai script, relative to the blueprint, run with the same gates a
    /// script tool answers to.
    pub(crate) script: Option<String>,
    /// The server to write into the config, for an MCP dependency.
    pub(crate) server: Option<McpServerTemplate>,
}

/// One thing a blueprint needs.
#[derive(Debug, SimpleObject)]
pub(crate) struct BlueprintDependency {
    /// A short name, unique within the blueprint.
    pub(crate) name: String,
    /// What kind of thing it is. Which of the fields below is set follows from
    /// this.
    pub(crate) kind: DependencyKind,
    /// Whether the spawn fails when it is missing. False makes a miss a warning
    /// the run goes past.
    pub(crate) required: bool,
    /// A sentence telling the person how to satisfy it, shown wherever the miss
    /// is reported.
    pub(crate) remedy: Option<String>,
    /// One line on why the blueprint needs it.
    pub(crate) description: Option<String>,
    /// How it can be installed, when the blueprint says.
    pub(crate) install: Option<DependencyInstall>,
    /// The server that must be configured, for an MCP dependency.
    pub(crate) server: Option<String>,
    /// The variables that server needs set, for an MCP dependency.
    pub(crate) env: Vec<String>,
    /// The variable that must be set, for an environment dependency.
    pub(crate) var: Option<String>,
    /// The program that must resolve on the path, for a binary dependency.
    pub(crate) command: Option<String>,
    /// The script that decides, for a script dependency.
    pub(crate) check: Option<String>,
}

impl From<&leviath_core::blueprint::Dependency> for BlueprintDependency {
    fn from(dependency: &leviath_core::blueprint::Dependency) -> Self {
        use leviath_core::blueprint::DependencyKind as Core;
        let mut mapped = Self {
            name: dependency.name.clone(),
            kind: DependencyKind::Binary,
            required: dependency.required,
            remedy: dependency.remedy.clone(),
            description: dependency.description.clone(),
            install: dependency.install.as_ref().map(DependencyInstall::from),
            server: None,
            env: Vec::new(),
            var: None,
            command: None,
            check: None,
        };
        match &dependency.kind {
            Core::McpServer { server, env } => {
                mapped.kind = DependencyKind::McpServer;
                mapped.server = Some(server.clone());
                mapped.env = env.clone();
            }
            Core::Env { var } => {
                mapped.kind = DependencyKind::Env;
                mapped.var = Some(var.clone());
            }
            Core::Binary { command } => {
                mapped.kind = DependencyKind::Binary;
                mapped.command = Some(command.clone());
            }
            Core::Script { check } => {
                mapped.kind = DependencyKind::Script;
                mapped.check = Some(check.clone());
            }
        }
        mapped
    }
}

impl From<&leviath_core::blueprint::DependencyInstall> for DependencyInstall {
    fn from(install: &leviath_core::blueprint::DependencyInstall) -> Self {
        Self {
            command: install.command.clone(),
            commands: install
                .commands
                .iter()
                .map(|(os, command)| InstallCommand {
                    os: os.clone(),
                    command: command.clone(),
                })
                .collect(),
            script: install.script.clone(),
            server: install.server.as_ref().map(McpServerTemplate::from),
        }
    }
}

/// A name-value map as a list, in the map's own order.
fn entries(map: &std::collections::BTreeMap<String, String>) -> Vec<EnvEntry> {
    map.iter()
        .map(|(name, value)| EnvEntry {
            name: name.clone(),
            value: value.clone(),
        })
        .collect()
}

impl From<&leviath_core::blueprint::McpServerTemplate> for McpServerTemplate {
    fn from(template: &leviath_core::blueprint::McpServerTemplate) -> Self {
        Self {
            transport: template.transport.as_deref().and_then(|word| {
                match word.trim().to_ascii_lowercase().as_str() {
                    "stdio" => Some(McpTransport::Stdio),
                    "http" => Some(McpTransport::Http),
                    // A word neither the daemon nor this schema knows is left
                    // out rather than guessed at: the installer infers the
                    // transport from the command or the URL anyway.
                    _ => None,
                }
            }),
            command: template.command.clone(),
            url: template.url.clone(),
            args: template.args.clone(),
            env: entries(&template.env),
            headers: entries(&template.headers),
        }
    }
}
