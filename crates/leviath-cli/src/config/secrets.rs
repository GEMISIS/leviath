//! The provider API keys a config holds, and keeping them in the credential
//! store the user chose.
//!
//! [`PROVIDER_KEYS`] is the one list of them. The environment fallbacks, the
//! credential store overlay, the copy saved in keychain mode and `lev auth`
//! all read it, so a key listed there is a key every one of them handles, and
//! a key left out of it would sit in `config.toml` in plaintext under a
//! keychain the user asked for.

use super::Config;

/// One provider API key: where it sits in [`Config`], the variable that
/// stands in for it, and the account the credential store files it under.
pub(crate) struct ProviderKey {
    /// The provider, as the credential store's account names it.
    provider: &'static str,
    /// The environment variable read when the file leaves the key unset.
    env: &'static str,
    /// The key, as the config holds it.
    get: fn(&Config) -> Option<&str>,
    /// The key's field, to fill or clear.
    slot: fn(&mut Config) -> &mut Option<String>,
}

impl ProviderKey {
    /// The credential store account this key is filed under.
    pub(crate) fn account(&self) -> String {
        leviath_core::provider_account(self.provider)
    }
}

/// Every provider API key the config holds.
///
/// Fixed, because the OS stores offer no portable "list everything under this
/// service" operation: the accounts to look for have to come from somewhere,
/// and for provider keys it is this table. A test holds it to every
/// `*_api_key` field of [`Config`] and [`super::ProviderConfig`].
pub(crate) const PROVIDER_KEYS: &[ProviderKey] = &[
    ProviderKey {
        provider: "anthropic",
        env: "ANTHROPIC_API_KEY",
        get: |c| c.providers.anthropic_api_key.as_deref(),
        slot: |c| &mut c.providers.anthropic_api_key,
    },
    ProviderKey {
        provider: "openai",
        env: "OPENAI_API_KEY",
        get: |c| c.providers.openai_api_key.as_deref(),
        slot: |c| &mut c.providers.openai_api_key,
    },
    ProviderKey {
        provider: "google",
        env: "GOOGLE_API_KEY",
        get: |c| c.providers.google_api_key.as_deref(),
        slot: |c| &mut c.providers.google_api_key,
    },
    // The one key at the top level of the file rather than under
    // `[providers]`.
    ProviderKey {
        provider: "openrouter",
        env: "OPENROUTER_API_KEY",
        get: |c| c.openrouter_api_key.as_deref(),
        slot: |c| &mut c.openrouter_api_key,
    },
    ProviderKey {
        provider: "meshy",
        env: "MESHY_API_KEY",
        get: |c| c.providers.meshy_api_key.as_deref(),
        slot: |c| &mut c.providers.meshy_api_key,
    },
    // The variable AWS's own tooling reads.
    ProviderKey {
        provider: "bedrock",
        env: leviath_providers::bedrock::KEY_ENV,
        get: |c| c.providers.bedrock_api_key.as_deref(),
        slot: |c| &mut c.providers.bedrock_api_key,
    },
    ProviderKey {
        provider: "xai",
        env: "XAI_API_KEY",
        get: |c| c.providers.xai_api_key.as_deref(),
        slot: |c| &mut c.providers.xai_api_key,
    },
    // Not Meta's own `MODEL_API_KEY`: a name that generic could belong to
    // anything on the machine.
    ProviderKey {
        provider: "meta",
        env: "META_AI_API_KEY",
        get: |c| c.providers.meta_api_key.as_deref(),
        slot: |c| &mut c.providers.meta_api_key,
    },
];

impl Config {
    /// Fill each provider key the file left unset from its environment
    /// variable.
    pub(super) fn keys_from_env(&mut self) {
        for key in PROVIDER_KEYS {
            let slot = (key.slot)(self);
            if slot.is_none() {
                *slot = std::env::var(key.env).ok();
            }
        }
    }

    /// Fill any provider key still unset from the configured credential store.
    pub(super) fn fill_from_credential_store(&mut self) {
        let resolved = crate::credentials::store_for(self.security.credential_store);
        self.fill_from_credential_store_with(resolved);
    }

    /// Core of [`fill_from_credential_store`](Self::fill_from_credential_store)
    /// with the backend already resolved.
    ///
    /// Runs *after* the file and the environment, so precedence is file > env >
    /// keychain: what the user can see wins over what they cannot. In keychain
    /// mode `lev auth migrate` strips the keys out of the file, so in practice
    /// the keychain is the only source - but a key left behind by hand keeps
    /// working rather than being silently ignored, and `lev auth status` reports
    /// when a secret exists in both places.
    ///
    /// A store that cannot be opened is a warning, not a hard failure. The user
    /// may still have working keys in their environment, and refusing to load
    /// the config at all would take down `lev auth status` - the one command
    /// that can explain what is wrong. The resolution is the caller's so that
    /// path is testable: "no store is installed in this process" is not the same
    /// as "this machine has no keychain", and on a developer's Mac the first
    /// silently becomes the second.
    pub(super) fn fill_from_credential_store_with(
        &mut self,
        resolved: crate::credentials::Resolved,
    ) {
        match resolved {
            Ok(Some(store)) => self.apply_credential_store(store.as_ref()),
            // The file backend keeps its keys in this struct already.
            Ok(None) => {}
            Err(e) => {
                tracing::warn!("{e}. Falling back to keys from the config file and environment.");
            }
        }
    }

    /// Overlay `store`'s secrets onto whichever provider keys are still unset.
    pub(super) fn apply_credential_store(&mut self, store: &dyn leviath_core::CredentialStore) {
        let accounts: Vec<String> = PROVIDER_KEYS.iter().map(ProviderKey::account).collect();
        let mut found = store.read_all(&accounts);
        for key in PROVIDER_KEYS {
            let slot = (key.slot)(self);
            if slot.is_none() {
                *slot = found.remove(&key.account());
            }
        }
    }

    /// This config with every provider API key removed.
    ///
    /// What gets serialized in keychain mode: the secrets go to the OS store and
    /// the file keeps only the settings. Returning a stripped copy rather than
    /// mutating in place matters - the caller is usually saving a config it is
    /// still going to use for inference, and blanking its keys would break the
    /// run that triggered the save.
    pub(super) fn without_secrets(&self) -> Self {
        let mut copy = self.clone();
        for key in PROVIDER_KEYS {
            *(key.slot)(&mut copy) = None;
        }
        copy
    }

    /// Every provider key currently set, as `(account, secret)` pairs.
    pub(crate) fn provider_secrets(&self) -> Vec<(String, String)> {
        PROVIDER_KEYS
            .iter()
            .filter_map(|key| (key.get)(self).map(|k| (key.account(), k.to_string())))
            .collect()
    }

    /// Core of [`save_to_path`](Self::save_to_path) with the backend already
    /// resolved - see
    /// [`fill_from_credential_store_with`](Self::fill_from_credential_store_with)
    /// for why the resolution is the caller's.
    pub(super) fn write_to(
        &self,
        path: &std::path::Path,
        resolved: crate::credentials::Resolved,
    ) -> anyhow::Result<()> {
        let to_write = match resolved.map_err(|e| anyhow::anyhow!("{e}"))? {
            Some(store) => {
                for (account, secret) in self.provider_secrets() {
                    store
                        .set(&account, &secret)
                        .map_err(|e| anyhow::anyhow!("failed to store {account}: {e}"))?;
                }
                self.without_secrets()
            }
            None => self.clone(),
        };

        // Config contains only primitive-typed fields; toml serialization is infallible.
        let content =
            toml::to_string_pretty(&to_write).expect("Config serialization is infallible");

        // `write_private`, not `fs::write` + `chmod`. This file holds every
        // provider API key, and the two-step version left it at the umask
        // default (typically 0644) between the write and the mode change - so
        // every save had a moment where any local user could read the keys.
        leviath_sys::write_private(path, content.as_bytes()).map_err(|e| {
            anyhow::anyhow!("Failed to write config to '{}': {}", path.display(), e)
        })?;

        let path_display = path.display();
        tracing::debug!("Saved config to {}", path_display);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `*_api_key` field in the two structs, as a dotted path.
    fn api_key_fields(value: &serde_json::Value, prefix: &str, out: &mut Vec<String>) {
        for (name, field) in value.as_object().into_iter().flatten() {
            let path = format!("{prefix}{name}");
            if name.ends_with("_api_key") {
                out.push(path);
            } else if name == "providers" {
                api_key_fields(field, "providers.", out);
            }
        }
    }

    /// The table is the only place a provider key is named, so a key missing
    /// from it is a key that stays in the file under a keychain and that
    /// `lev auth migrate` never moves. Each entry has to reach exactly one
    /// field, and every field has to be reached by one.
    #[test]
    fn every_api_key_field_is_in_the_table() {
        let mut fields = Vec::new();
        api_key_fields(
            &serde_json::to_value(Config::default()).unwrap(),
            "",
            &mut fields,
        );
        fields.sort();

        let mut reached = Vec::new();
        for key in PROVIDER_KEYS {
            let mut config = Config::default();
            *(key.slot)(&mut config) = Some("set".to_string());
            assert_eq!((key.get)(&config), Some("set"), "{}", key.provider);
            let value = serde_json::to_value(&config).unwrap();
            let mut set = Vec::new();
            api_key_fields(&value, "", &mut set);
            set.retain(|path| {
                let (table, field) = path.split_once('.').unwrap_or(("", path));
                let holder = match table {
                    "" => &value,
                    table => &value[table],
                };
                holder[field] == "set"
            });
            assert_eq!(set.len(), 1, "{} reaches {set:?}", key.provider);
            reached.extend(set);
        }
        reached.sort();
        assert_eq!(
            reached, fields,
            "a field no entry reaches, or two that share one"
        );
    }

    /// Each key falls back to its own variable, and a key the file set is
    /// left alone.
    #[test]
    fn a_key_the_file_left_unset_comes_from_its_variable() {
        let vars: Vec<(&str, Option<&str>)> = PROVIDER_KEYS
            .iter()
            .map(|key| (key.env, Some(key.provider)))
            .collect();
        temp_env::with_vars(vars, || {
            let mut config = Config::default();
            config.providers.openai_api_key = Some("from-file".to_string());
            config.keys_from_env();
            for key in PROVIDER_KEYS.iter().filter(|key| key.provider != "openai") {
                assert_eq!((key.get)(&config), Some(key.provider), "{}", key.env);
            }
            assert_eq!(
                config.providers.openai_api_key.as_deref(),
                Some("from-file")
            );
        });
    }
}
