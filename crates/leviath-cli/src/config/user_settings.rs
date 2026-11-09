//! The provider settings a [`Config`] holds, in the form the runtime builds
//! providers from.
//!
//! `leviath_runtime::user_creds` is the one place that turns provider settings
//! into the providers a registry holds and the defaults a stage resolves
//! against, for `lev` and for a host embedding the runtime alike. This is the
//! copy that hands `lev`'s settings to it.

use super::Config;
use leviath_runtime::user_creds::{SecuritySettings, UserSettings};
use std::collections::BTreeMap;

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

impl Config {
    /// Fill every provider setting the file left unset from `env`: each key
    /// and each host from its variable, Bedrock's region from AWS's, and
    /// Ollama's address from `OLLAMA_HOST`. A value in the file wins, a blank
    /// one included.
    ///
    /// A key variable that is set but empty leaves its provider
    /// unregistered. That is common where a pipeline exports every secret it
    /// might have, so it is said at debug level rather than on every command.
    pub(super) fn fill_from_env(&mut self, env: &dyn Fn(&str) -> Option<String>) {
        let mut settings = self.user_settings();
        let mut notes = Vec::new();
        self.env_fill = settings.fill_from_env(env, &mut notes);
        for note in &notes {
            tracing::debug!("{note}");
        }
        self.adopt_filled(settings);
    }

    /// This config as it is kept: with what the environment supplied taken
    /// back out.
    ///
    /// A load fills what the file leaves unset from the environment, and the
    /// config a run uses has to carry that. The copy written to disk must
    /// not: every command that edits the config loads it, changes one thing
    /// and saves it, and writing the struct as it stands would pin a key the
    /// user keeps in their environment, a gateway address and a region into
    /// `config.toml`. A key the credential store supplied stays, since it is
    /// kept, only somewhere else; and a setting changed since the load is
    /// the caller's own and stays as they set it.
    pub(crate) fn as_stored(&self) -> Self {
        let mut stored = self.clone();
        let mut settings = stored.user_settings();
        settings.without_env_fill(&self.env_fill);
        stored.adopt_filled(settings);
        stored.env_fill = leviath_runtime::user_creds::EnvFill::default();
        stored
    }

    /// Fill each provider key still unset from `store`, reading only those
    /// keys' accounts.
    pub(super) fn fill_keys_from_store(&mut self, store: &dyn leviath_core::CredentialStore) {
        let mut settings = self.user_settings();
        let missing = settings.missing_keys();
        settings.fill_from_store(store, &missing, &mut BTreeMap::new());
        self.adopt_filled(settings);
    }

    /// Say which provider settings are being ignored: a `fallback_order`
    /// entry that is not `provider/model`, a `[model_providers]` retention
    /// word that is not one.
    ///
    /// Said here, where the file is loaded, beside the unread-key warning,
    /// and nowhere else. The daemon asks for the retention settings every
    /// two seconds and for the model defaults at every stage; a warning
    /// from there is one line repeated for as long as the daemon runs.
    pub(super) fn warn_ignored_provider_settings(&self) {
        let mut warnings = Vec::new();
        leviath_runtime::user_creds::model_defaults(&self.user_settings(), &mut warnings);
        for warning in &warnings {
            tracing::warn!("{warning}");
        }
    }

    /// Take back the three places a fill writes: the one key and the one
    /// address at the top of the file, and `[providers]`.
    fn adopt_filled(&mut self, settings: UserSettings) {
        self.openrouter_api_key = settings.openrouter_api_key;
        self.ollama_base_url = settings.ollama_base_url;
        self.providers = settings.providers;
    }
}

#[cfg(test)]
#[path = "user_settings_tests.rs"]
mod tests;
