//! Turning `config.toml` into the shapes the spawn path needs.
//!
//! Two small translations that have nothing to do with each other except
//! their direction: the user's model defaults and the shell environment
//! policy. They are here rather than beside the code that consumes them
//! because each is a pure function of the config and neither is about
//! spawning - keeping them together stops the assembly below from reading
//! like it also parses settings.

use super::*;

/// The user's default provider and model settings from `config.toml`, in the
/// plain form the runtime's stage resolver takes: the runtime's own
/// [`model_defaults`](leviath_runtime::user_creds::model_defaults) over this
/// config's provider settings. A malformed `[providers] fallback_order` entry
/// is dropped with a warning that names it.
pub(crate) fn model_defaults(config: &Config) -> ModelDefaults {
    let mut warnings = Vec::new();
    let defaults =
        leviath_runtime::user_creds::model_defaults(&config.user_settings(), &mut warnings);
    crate::config::log_warnings(&warnings);
    defaults
}

/// The `[security] shell_env` decision for this daemon, in the shape the tools
/// layer wants.
///
/// One resolver rather than three: the built-in `shell` tool, a Rhai `shell()`,
/// and a region's command seed all hand the daemon's environment to a child, so
/// they answer to the same setting. A script that has `shell` would otherwise be
/// the way around the `env_var` gate.
pub(crate) fn shell_env_policy(config: &Config) -> leviath_tools::ShellEnvPolicy {
    leviath_tools::ShellEnvPolicy {
        mode: config.security.shell_env,
        allow_env_vars: config.security.allow_env_vars.clone(),
        withhold: config.security.shell_env_withhold.clone(),
    }
}
