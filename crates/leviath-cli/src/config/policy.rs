//! How permissive a tool or script capability is, before any run-time layering.
//!
//! Three small enums that every other config section refers to, so they sit
//! apart from the sections that use them rather than inside whichever one
//! happened to need them first.

use serde::{Deserialize, Serialize};

/// Whether a tool call should execute automatically or require user approval.
///
/// The effective policy for a tool is resolved by narrowest scope first:
/// launch-flag > stage > agent > global config > built-in default.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolPolicy {
    /// Execute without prompting.
    Allow,
    /// Ask the user before each call (or once per session with `allow_session`).
    #[default]
    Ask,
    /// Never execute - return a denied error to the model.
    Deny,
}

// `TitleConfig` (plain data used by the engine's title generation) lives in
// `leviath_core::config` so `leviath-runtime` can reference it without a CLI
// dependency. Re-exported here so `crate::config::TitleConfig` paths resolve.
pub(crate) use leviath_core::config::TitleConfig;

// Same arrangement for the `[observability]` section: the plain data lives in
// `leviath_core::config` (the telemetry sink crate reads it), re-exported here.
pub(crate) use leviath_core::config::ObservabilityConfig;
#[cfg(test)]
pub(crate) use leviath_core::config::TelemetryExporterKind;

/// Permission for one Rhai *script-tool* host function (Layer 3 of the
/// four-layer permission model). Gates what a registered script may *do*,
/// independent of
/// whether the tool itself is visible ([`available_tools`]) or approved at
/// runtime ([`ToolPolicy`]).
///
/// [`available_tools`]: leviath_core::blueprint::Stage::available_tools
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScriptPermission {
    /// The host function may run.
    Allow,
    /// The host function is blocked - the call returns a `[denied]` error.
    Deny,
    /// Defer to the agent's own `tool_permissions` for the equivalent built-in
    /// (`read_file`/`shell`): permitted only when that resolves to
    /// [`ToolPolicy::Allow`]. For the network/env functions (`http_get`,
    /// `http_post`, `env_var`), which have no built-in equivalent, `Inherit`
    /// permits the call (they're needed for tools to be useful, and the tool
    /// itself is still gated by Layers 1/2/4).
    #[default]
    Inherit,
}

/// Per-host-function permissions for Rhai script tools (`[tool_script_permissions]`).
///
/// Every field defaults to [`ScriptPermission::Inherit`], so an unconfigured
/// install lets network/env functions run while file/shell functions defer to
/// the agent's own tool permissions.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScriptToolPermissions {
    /// Permission for `http_get`.
    #[serde(default)]
    pub http_get: ScriptPermission,
    /// Permission for `http_post`.
    #[serde(default)]
    pub http_post: ScriptPermission,
    /// Permission for `shell`.
    #[serde(default)]
    pub shell: ScriptPermission,
    /// Permission for `read_file`.
    #[serde(default)]
    pub read_file: ScriptPermission,
    /// Permission for `write_file`.
    #[serde(default)]
    pub write_file: ScriptPermission,
    /// Permission for `env_var`.
    #[serde(default)]
    pub env_var: ScriptPermission,
}
