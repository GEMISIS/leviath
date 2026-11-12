//! The user's tool permissions, read from the same `config.toml` the
//! provider settings come from.

use super::{UserCredsError, UserCredsSource, config_path, read_config};
use crate::tool_policy::{AgentSafeCommandSettings, SafeCommandSettings, ToolPermissions};
use leviath_core::policy::ToolPolicy;
use serde::Deserialize;
use std::collections::HashMap;

/// The tables of the file the permissions sit in. Everything else it holds
/// is somebody else's to read.
#[derive(Default, Deserialize)]
#[serde(default)]
struct PermissionTables {
    /// `[tool_permissions]`.
    tool_permissions: HashMap<String, ToolPolicy>,
    /// `[agent_tool_permissions.<name>]`.
    agent_tool_permissions: HashMap<String, HashMap<String, ToolPolicy>>,
    /// `[safe_commands]`.
    safe_commands: SafeCommandSettings,
    /// `[agent_safe_commands.<name>]`.
    agent_safe_commands: HashMap<String, AgentSafeCommandSettings>,
    /// `[security]`, the keys of it that are permissions.
    security: PermissionSwitches,
}

/// `[security]`, as far as tool permissions go.
#[derive(Default, Deserialize)]
#[serde(default)]
struct PermissionSwitches {
    /// `allow_blueprint_permissions`.
    allow_blueprint_permissions: bool,
    /// `allow_blueprint_safe_commands`.
    allow_blueprint_safe_commands: bool,
}

/// Read the user's tool permissions from `source`'s config file, as `lev`
/// reads them: `[tool_permissions]`, `[agent_tool_permissions.<name>]`,
/// `[safe_commands]`, `[agent_safe_commands.<name>]`, and the two `[security]`
/// switches that let a graph's own count (`allow_blueprint_permissions`,
/// `allow_blueprint_safe_commands`).
///
/// For a program that embeds the runtime and checks a run against the
/// machine it will be started on
/// ([`check_with_permissions`](crate::resolve::check_with_permissions)). A
/// file that does not exist is the defaults, a machine whose user has set
/// nothing; one that cannot be read or parsed is an error. Only the file is
/// read: the environment and the credential store hold no permissions.
pub fn load_tool_permissions(
    source: &UserCredsSource<'_>,
) -> Result<ToolPermissions, UserCredsError> {
    let path = config_path(source);
    let Some((text, _)) = read_config(&path)? else {
        return Ok(ToolPermissions::default());
    };
    let tables: PermissionTables = match toml::from_str(&text) {
        Ok(tables) => tables,
        Err(source) => return Err(UserCredsError::Parse { path, source }),
    };
    Ok(ToolPermissions {
        tool_permissions: tables.tool_permissions,
        agent_tool_permissions: tables.agent_tool_permissions,
        allow_blueprint_permissions: tables.security.allow_blueprint_permissions,
        safe_commands: tables.safe_commands,
        agent_safe_commands: tables.agent_safe_commands,
        allow_blueprint_safe_commands: tables.security.allow_blueprint_safe_commands,
    })
}
