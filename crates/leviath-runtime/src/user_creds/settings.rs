//! The slice of `config.toml` that decides which providers are registered and
//! how stages pick among them.
//!
//! [`UserSettings`] is what both readers of the file build from: `lev` copies
//! these fields out of its own `Config`, which is the superset and the
//! authority on what the file may hold, and [`load_user_creds`](super::load_user_creds)
//! deserializes them directly. Anything the file holds that this does not
//! name (MCP servers, permissions, limits) is ignored rather than refused.

use super::{ModelProviderConfig, ProviderConfig};
use leviath_core::CredentialStoreKind;
use leviath_providers::{ModelCapabilityOverride, RateLimitConfig};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};

/// The top level of the file, as far as provider credentials go.
///
/// No `Debug`: [`openrouter_api_key`](Self::openrouter_api_key) is a secret
/// sitting in the open, and a derived one would print it.
#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct UserSettings {
    /// `default_provider`: the single front-runner for a bare model name.
    pub default_provider: String,
    /// `openrouter_api_key`: the one key at the top of the file rather than
    /// under `[providers]`.
    pub openrouter_api_key: Option<String>,
    /// `ollama_base_url`: Ollama's address, which also opts it in.
    pub ollama_base_url: Option<String>,
    /// `override_model`: the model every stage that allows a user default
    /// starts on.
    pub override_model: Option<String>,
    /// `fallback_model`: tried after a stage's own models.
    pub fallback_model: Option<String>,
    /// `[model_capabilities]`: per-model overrides handed to every provider.
    pub model_capabilities: HashMap<String, ModelCapabilityOverride>,
    /// `request_timeout_secs`: the host-wide provider request timeout.
    pub request_timeout_secs: Option<u64>,
    /// `[rate_limits.<provider>]`: client-side throttles for the built-ins.
    pub rate_limits: HashMap<String, RateLimitConfig>,
    /// `[providers]`.
    pub providers: ProviderConfig,
    /// `[model_providers.<name>]`: the endpoints registered under their own
    /// names, and the script entries, read here for their retention words.
    pub model_providers: HashMap<String, ModelProviderConfig>,
    /// `[security]`, for where the keys are kept.
    pub security: SecuritySettings,
}

impl Default for UserSettings {
    /// What a missing file means: `lev`'s defaults, `anthropic` as the
    /// default provider included.
    fn default() -> Self {
        Self {
            default_provider: "anthropic".to_string(),
            openrouter_api_key: None,
            ollama_base_url: None,
            override_model: None,
            fallback_model: None,
            model_capabilities: HashMap::new(),
            request_timeout_secs: None,
            rate_limits: HashMap::new(),
            providers: ProviderConfig::default(),
            model_providers: HashMap::new(),
            security: SecuritySettings::default(),
        }
    }
}

/// `[security]`, the one key of it that matters here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct SecuritySettings {
    /// `credential_store`: `file` (the default) or `keychain`.
    pub credential_store: CredentialStoreKind,
}

/// Where one API-keyed provider's key, host and headers sit in [`UserSettings`].
///
/// Bound to its provider by name, never by position: [`slots`] looks an
/// entry up by the name [`super::KEYED_PROVIDERS`] gives it.
pub(super) struct Slots {
    /// The provider these fields belong to.
    pub(super) provider: &'static str,
    /// The key's field, to fill.
    pub(super) key: fn(&mut UserSettings) -> &mut Option<String>,
    /// The key's field, to read.
    pub(super) key_ref: fn(&UserSettings) -> &Option<String>,
    /// The host's field, to fill.
    pub(super) base_url: fn(&mut UserSettings) -> &mut Option<String>,
    /// The host's field, to read.
    pub(super) base_url_ref: fn(&UserSettings) -> &Option<String>,
    /// The extra headers' field.
    pub(super) headers: fn(&UserSettings) -> &BTreeMap<String, String>,
}

/// The fields of every API-keyed provider.
pub(super) const SLOTS: [Slots; 8] = [
    Slots {
        provider: "anthropic",
        key: |s| &mut s.providers.anthropic_api_key,
        key_ref: |s| &s.providers.anthropic_api_key,
        base_url: |s| &mut s.providers.anthropic_base_url,
        base_url_ref: |s| &s.providers.anthropic_base_url,
        headers: |s| &s.providers.anthropic_headers,
    },
    Slots {
        provider: "openai",
        key: |s| &mut s.providers.openai_api_key,
        key_ref: |s| &s.providers.openai_api_key,
        base_url: |s| &mut s.providers.openai_base_url,
        base_url_ref: |s| &s.providers.openai_base_url,
        headers: |s| &s.providers.openai_headers,
    },
    Slots {
        provider: "google",
        key: |s| &mut s.providers.google_api_key,
        key_ref: |s| &s.providers.google_api_key,
        base_url: |s| &mut s.providers.google_base_url,
        base_url_ref: |s| &s.providers.google_base_url,
        headers: |s| &s.providers.google_headers,
    },
    Slots {
        provider: "openrouter",
        key: |s| &mut s.openrouter_api_key,
        key_ref: |s| &s.openrouter_api_key,
        base_url: |s| &mut s.providers.openrouter_base_url,
        base_url_ref: |s| &s.providers.openrouter_base_url,
        headers: |s| &s.providers.openrouter_headers,
    },
    Slots {
        provider: "meshy",
        key: |s| &mut s.providers.meshy_api_key,
        key_ref: |s| &s.providers.meshy_api_key,
        base_url: |s| &mut s.providers.meshy_base_url,
        base_url_ref: |s| &s.providers.meshy_base_url,
        headers: |s| &s.providers.meshy_headers,
    },
    Slots {
        provider: leviath_providers::bedrock::PROVIDER_NAME,
        key: |s| &mut s.providers.bedrock_api_key,
        key_ref: |s| &s.providers.bedrock_api_key,
        base_url: |s| &mut s.providers.bedrock_base_url,
        base_url_ref: |s| &s.providers.bedrock_base_url,
        headers: |s| &s.providers.bedrock_headers,
    },
    Slots {
        provider: leviath_providers::xai::PROVIDER_NAME,
        key: |s| &mut s.providers.xai_api_key,
        key_ref: |s| &s.providers.xai_api_key,
        base_url: |s| &mut s.providers.xai_base_url,
        base_url_ref: |s| &s.providers.xai_base_url,
        headers: |s| &s.providers.xai_headers,
    },
    Slots {
        provider: leviath_providers::meta::PROVIDER_NAME,
        key: |s| &mut s.providers.meta_api_key,
        key_ref: |s| &s.providers.meta_api_key,
        base_url: |s| &mut s.providers.meta_base_url,
        base_url_ref: |s| &s.providers.meta_base_url,
        headers: |s| &s.providers.meta_headers,
    },
];

/// The fields of `provider`, an entry of [`super::KEYED_PROVIDERS`].
pub(super) fn slots(provider: &str) -> &'static Slots {
    SLOTS
        .iter()
        .find(|slots| slots.provider == provider)
        .expect("every keyed provider has its fields in UserSettings")
}

/// The variables that stand in for Bedrock's region, in the order they are
/// asked: the two AWS's own tooling reads.
const REGION_ENV: [&str; 2] = ["AWS_REGION", "AWS_DEFAULT_REGION"];
/// The variable that stands in for Ollama's address, the one Ollama reads.
const OLLAMA_ENV: &str = "OLLAMA_HOST";

/// Every environment variable [`UserSettings::fill_from_env`] reads: each
/// keyed provider's key and host, Bedrock's region, and Ollama's address.
///
/// For a host that wants to show which variables count, and for a test that
/// has to keep the machine's own out of a load.
pub fn env_names() -> Vec<&'static str> {
    super::KEYED_PROVIDERS
        .iter()
        .flat_map(|keyed| std::iter::once(keyed.key_env).chain(keyed.base_url_env))
        .chain(REGION_ENV)
        .chain([OLLAMA_ENV])
        .collect()
}

/// What [`UserSettings::fill_from_env`] took from the environment: which
/// slots it filled, and with what.
///
/// Kept by a host that writes the settings back to disk, so it can take
/// these out first with [`UserSettings::without_env_fill`]: a key the user
/// keeps in their environment is not one to copy into their config file.
///
/// `Debug` names the providers and never prints a key or a host.
#[derive(Clone, Default)]
pub struct EnvFill {
    /// Each key filled, by provider.
    keys: Vec<(&'static str, String)>,
    /// Each host filled, by provider.
    base_urls: Vec<(&'static str, String)>,
    /// Bedrock's region, when it was filled.
    bedrock_region: Option<String>,
    /// Ollama's address, when it was filled.
    ollama_base_url: Option<String>,
}

impl EnvFill {
    /// The providers whose key came from the environment, in table order.
    pub fn keys(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.keys.iter().map(|(provider, _)| *provider)
    }
}

impl std::fmt::Debug for EnvFill {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names = |filled: &[(&'static str, String)]| -> Vec<&'static str> {
            filled.iter().map(|(provider, _)| *provider).collect()
        };
        f.debug_struct("EnvFill")
            .field("keys", &names(&self.keys))
            .field("base_urls", &names(&self.base_urls))
            .field("bedrock_region", &self.bedrock_region.is_some())
            .field("ollama_base_url", &self.ollama_base_url.is_some())
            .finish()
    }
}

impl UserSettings {
    /// Fill what the file left unset from the environment: each key from its
    /// variable, each host from its own, Bedrock's region from AWS's
    /// variables, and Ollama's address from `OLLAMA_HOST`. A value in the
    /// file wins, a blank one included. What was filled comes back.
    ///
    /// A key variable that is set but blank fills the slot, so the provider
    /// is not registered and the keychain is not asked; a warning says so,
    /// since otherwise the provider just vanishes.
    pub fn fill_from_env(
        &mut self,
        env: &dyn Fn(&str) -> Option<String>,
        warnings: &mut Vec<String>,
    ) -> EnvFill {
        let mut fill = EnvFill::default();
        for keyed in super::KEYED_PROVIDERS {
            let slots = slots(keyed.provider);
            let slot = (slots.key)(self);
            if slot.is_none()
                && let Some(key) = env(keyed.key_env)
            {
                if key.trim().is_empty() {
                    warnings.push(format!(
                        "{} is set but empty, so {} is not registered; a key for it in the \
                         credential store is ignored",
                        keyed.key_env, keyed.provider
                    ));
                }
                fill.keys.push((keyed.provider, key.clone()));
                *slot = Some(key);
            }
            let base_url = (slots.base_url)(self);
            if base_url.is_none()
                && let Some(url) = keyed.base_url_env.and_then(env)
            {
                fill.base_urls.push((keyed.provider, url.clone()));
                *base_url = Some(url);
            }
        }
        // Blank is unset: an exported empty variable is not a region.
        if self.providers.bedrock_region.is_none() {
            fill.bedrock_region = REGION_ENV
                .into_iter()
                .find_map(env)
                .map(|r| r.trim().to_string())
                .filter(|r| !r.is_empty());
            self.providers.bedrock_region = fill.bedrock_region.clone();
        }
        if self.ollama_base_url.is_none() {
            fill.ollama_base_url = env(OLLAMA_ENV);
            self.ollama_base_url = fill.ollama_base_url.clone();
        }
        fill
    }

    /// Take back out what the environment supplied: every slot `fill` filled
    /// that still holds that value is unset again.
    ///
    /// A slot holding anything else was changed since the fill, by whoever
    /// is about to save these settings, and is left as they set it. One set
    /// to the very value the environment had is indistinguishable from the
    /// environment's and is unset with it, which reads back the same for as
    /// long as the variable stays.
    pub fn without_env_fill(&mut self, fill: &EnvFill) {
        for (provider, value) in &fill.keys {
            let slot = (slots(provider).key)(self);
            if slot.as_ref() == Some(value) {
                *slot = None;
            }
        }
        for (provider, value) in &fill.base_urls {
            let slot = (slots(provider).base_url)(self);
            if slot.as_ref() == Some(value) {
                *slot = None;
            }
        }
        if fill.bedrock_region.is_some() && self.providers.bedrock_region == fill.bedrock_region {
            self.providers.bedrock_region = None;
        }
        if fill.ollama_base_url.is_some() && self.ollama_base_url == fill.ollama_base_url {
            self.ollama_base_url = None;
        }
    }

    /// The extra headers `keyed`'s provider sends on every request.
    ///
    /// A header's value is a credential as often as not, so this is what a
    /// host that scrubs or redacts secrets walks, over
    /// [`KEYED_PROVIDERS`](super::KEYED_PROVIDERS), to reach every one.
    pub fn headers(&self, keyed: &super::KeyedProvider) -> &BTreeMap<String, String> {
        (slots(keyed.provider).headers)(self)
    }

    /// The keyed providers whose key is still unset after the file and the
    /// environment: the only accounts worth asking a credential store for.
    pub fn missing_keys(&self) -> Vec<&'static str> {
        super::KEYED_PROVIDERS
            .iter()
            .map(|keyed| keyed.provider)
            .filter(|provider| (slots(provider).key_ref)(self).is_none())
            .collect()
    }

    /// Fill each of the `missing` keys from `store`, filed under
    /// `provider/<name>` as `lev auth` keeps them. Reads those accounts and
    /// no others.
    pub fn fill_from_store(
        &mut self,
        store: &dyn leviath_core::CredentialStore,
        missing: &[&'static str],
        origins: &mut BTreeMap<String, super::KeyOrigin>,
    ) {
        let accounts: Vec<String> = missing
            .iter()
            .map(|provider| leviath_core::provider_account(provider))
            .collect();
        let mut found = store.read_all(&accounts);
        for provider in missing {
            if let Some(secret) = found.remove(&leviath_core::provider_account(provider)) {
                *(slots(provider).key)(self) = Some(secret);
                origins.insert(provider.to_string(), super::KeyOrigin::Keychain);
            }
        }
    }
}
