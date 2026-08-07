//! Turning `config.toml` into the shapes the spawn path needs.
//!
//! Three small translations that have nothing to do with each other except
//! their direction: the user's default model, the shell environment policy, and
//! the provider fallback chain. They are here rather than beside the code that
//! consumes them because each is a pure function of the config and none of them
//! is about spawning - keeping them together stops the assembly below from
//! reading like it also parses settings.

use super::*;

/// The user's default provider/model from `config.toml`, in the plain form the
/// runtime's stage resolver takes.
pub(crate) fn model_defaults(config: &Config) -> ModelDefaults {
    ModelDefaults {
        provider: config.default_provider.clone(),
        model: config.default_model.clone(),
        fallback_order: parse_fallback_order(&config.providers.fallback_order),
    }
}

/// Parse `[providers] fallback_order` entries (`"provider/model"`) into the
/// runtime's own form.
///
/// A malformed entry is dropped with a warning rather than failing the load: a
/// typo in a *safety net* should not stop the daemon from starting, and the
/// warning says which entry went nowhere. Splitting on the first `/` keeps
/// model ids that contain one (`deepseek/deepseek-v4-flash`) intact.
/// The `[security] shell_env` decision for this daemon, in the shape the tools
/// layer wants.
///
/// One resolver rather than three: the built-in `shell` tool, a Rhai `shell()`,
/// and a region's command seed all hand the daemon's environment to a child, so
/// they answer to the same setting. A script that has `shell` would otherwise be
/// the way around the `env_var` gate.
pub(super) fn shell_env_policy(config: &Config) -> leviath_tools::ShellEnvPolicy {
    leviath_tools::ShellEnvPolicy {
        mode: config.security.shell_env,
        allow_env_vars: config.security.allow_env_vars.clone(),
        withhold: config.security.shell_env_withhold.clone(),
    }
}

pub(super) fn parse_fallback_order(entries: &[String]) -> Vec<leviath_core::blueprint::ModelEntry> {
    entries
        .iter()
        .filter_map(|raw| match raw.split_once('/') {
            Some((provider, model)) if !provider.is_empty() && !model.is_empty() => Some(
                leviath_core::blueprint::ModelEntry::new(provider.to_string(), model.to_string()),
            ),
            _ => {
                tracing::warn!(
                    entry = %raw,
                    "ignoring [providers] fallback_order entry: expected \"provider/model\""
                );
                None
            }
        })
        .collect()
}
