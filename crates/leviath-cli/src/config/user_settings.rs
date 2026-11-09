//! The provider settings a [`Config`] holds, in the form the runtime builds
//! providers from.
//!
//! `leviath_runtime::user_creds` is the one place that turns provider settings
//! into the providers a registry holds and the defaults a stage resolves
//! against, for `lev` and for a host embedding the runtime alike. This is the
//! copy that hands `lev`'s settings to it.

use super::Config;
use leviath_runtime::user_creds::{SecuritySettings, UserSettings};

impl Config {
    /// The provider settings in this config, as the runtime reads them.
    ///
    /// Every field is named, with no `..Default::default()`: a field the
    /// runtime starts reading is a compile error here until it is copied.
    pub(crate) fn user_settings(&self) -> UserSettings {
        UserSettings {
            default_provider: self.default_provider.clone(),
            openrouter_api_key: self.openrouter_api_key.clone(),
            ollama_base_url: self.ollama_base_url.clone(),
            override_model: self.override_model.clone(),
            fallback_model: self.fallback_model.clone(),
            model_capabilities: self.model_capabilities.clone(),
            request_timeout_secs: self.request_timeout_secs,
            rate_limits: self.rate_limits.clone(),
            providers: self.providers.clone(),
            model_providers: self.model_providers.clone(),
            security: SecuritySettings {
                credential_store: self.security.credential_store,
            },
        }
    }
}

/// Log each of `warnings`, which the runtime returns as text rather than
/// logging itself: an embedding host decides where they go, and for `lev`
/// that is the log.
pub(crate) fn log_warnings(warnings: &[String]) {
    for warning in warnings {
        tracing::warn!("{warning}");
    }
}

#[cfg(test)]
#[path = "user_settings_tests.rs"]
mod tests;
