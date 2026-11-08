//! The slice of `config.toml` that decides which providers are registered and
//! how stages pick among them.
//!
//! Narrow on purpose: the CLI's own `Config` is the superset and the authority
//! on what the file may hold. Every field here carries the name and the type
//! the CLI reads it with, so a file the CLI loads loads here too, and anything
//! this does not name (MCP servers, permissions, limits, `[model_providers]`)
//! is ignored rather than refused.

use leviath_core::CredentialStoreKind;
use leviath_providers::anthropic::CacheTtl;
use leviath_providers::{ModelCapabilityOverride, RateLimitConfig};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};

/// The top level of the file, as far as provider credentials go.
#[derive(Deserialize)]
#[serde(default)]
pub(super) struct Settings {
    /// `default_provider`: the single front-runner for a bare model name.
    pub(super) default_provider: String,
    /// `openrouter_api_key`: the one key at the top of the file rather than
    /// under `[providers]`.
    pub(super) openrouter_api_key: Option<String>,
    /// `ollama_base_url`: Ollama's address, which also opts it in.
    pub(super) ollama_base_url: Option<String>,
    /// `override_model`: the model every stage that allows a user default
    /// starts on.
    pub(super) override_model: Option<String>,
    /// `fallback_model`: tried after a stage's own models.
    pub(super) fallback_model: Option<String>,
    /// `default_model`, the name `fallback_model` had before it was renamed.
    /// The CLI reads it as `fallback_model` when that is absent, and so does
    /// this.
    pub(super) default_model: Option<String>,
    /// `[model_capabilities]`: per-model overrides handed to every provider.
    pub(super) model_capabilities: HashMap<String, ModelCapabilityOverride>,
    /// `request_timeout_secs`: the host-wide provider request timeout.
    pub(super) request_timeout_secs: Option<u64>,
    /// `[rate_limits.<provider>]`: client-side throttles for the built-ins.
    pub(super) rate_limits: HashMap<String, RateLimitConfig>,
    /// `[providers]`.
    pub(super) providers: ProviderSettings,
    /// `[security]`, for where the keys are kept.
    pub(super) security: SecuritySettings,
}

impl Default for Settings {
    /// What a missing file means: the CLI's defaults, `anthropic` as the
    /// default provider included.
    fn default() -> Self {
        Self {
            default_provider: "anthropic".to_string(),
            openrouter_api_key: None,
            ollama_base_url: None,
            override_model: None,
            fallback_model: None,
            default_model: None,
            model_capabilities: HashMap::new(),
            request_timeout_secs: None,
            rate_limits: HashMap::new(),
            providers: ProviderSettings::default(),
            security: SecuritySettings::default(),
        }
    }
}

/// `[security]`, the one key of it that matters here.
#[derive(Deserialize, Default)]
#[serde(default)]
pub(super) struct SecuritySettings {
    /// `credential_store`: `file` (the default) or `keychain`.
    pub(super) credential_store: CredentialStoreKind,
}

/// `[providers]`: per-provider keys, hosts and headers, the sign-in
/// transports' switches, and the routing preferences.
#[derive(Deserialize)]
#[serde(default)]
pub(super) struct ProviderSettings {
    pub(super) anthropic_api_key: Option<String>,
    pub(super) openai_api_key: Option<String>,
    pub(super) google_api_key: Option<String>,
    pub(super) meshy_api_key: Option<String>,
    pub(super) bedrock_api_key: Option<String>,
    pub(super) xai_api_key: Option<String>,
    pub(super) meta_api_key: Option<String>,
    pub(super) bedrock_region: Option<String>,
    pub(super) anthropic_base_url: Option<String>,
    pub(super) openai_base_url: Option<String>,
    pub(super) google_base_url: Option<String>,
    pub(super) openrouter_base_url: Option<String>,
    pub(super) meshy_base_url: Option<String>,
    pub(super) bedrock_base_url: Option<String>,
    pub(super) xai_base_url: Option<String>,
    pub(super) meta_base_url: Option<String>,
    pub(super) anthropic_headers: BTreeMap<String, String>,
    pub(super) openai_headers: BTreeMap<String, String>,
    pub(super) google_headers: BTreeMap<String, String>,
    pub(super) openrouter_headers: BTreeMap<String, String>,
    pub(super) meshy_headers: BTreeMap<String, String>,
    pub(super) bedrock_headers: BTreeMap<String, String>,
    pub(super) xai_headers: BTreeMap<String, String>,
    pub(super) meta_headers: BTreeMap<String, String>,
    pub(super) ollama_enabled: bool,
    pub(super) codex_enabled: bool,
    pub(super) codex_originator: Option<String>,
    pub(super) codex_reasoning_effort: Option<String>,
    pub(super) codex_verbosity: Option<String>,
    pub(super) codex_replay_reasoning: bool,
    pub(super) grok_enabled: bool,
    pub(super) file_uploads: bool,
    pub(super) anthropic_cache_ttl: Option<CacheTtl>,
    pub(super) fallback_order: Vec<String>,
    pub(super) provider_order: Vec<String>,
    pub(super) zero_retention: bool,
    pub(super) zero_retention_agreements: Vec<String>,
}

impl Default for ProviderSettings {
    /// Agrees with the CLI's serde defaults: `codex_replay_reasoning` and
    /// `file_uploads` are on unless turned off.
    fn default() -> Self {
        Self {
            anthropic_api_key: None,
            openai_api_key: None,
            google_api_key: None,
            meshy_api_key: None,
            bedrock_api_key: None,
            xai_api_key: None,
            meta_api_key: None,
            bedrock_region: None,
            anthropic_base_url: None,
            openai_base_url: None,
            google_base_url: None,
            openrouter_base_url: None,
            meshy_base_url: None,
            bedrock_base_url: None,
            xai_base_url: None,
            meta_base_url: None,
            anthropic_headers: BTreeMap::new(),
            openai_headers: BTreeMap::new(),
            google_headers: BTreeMap::new(),
            openrouter_headers: BTreeMap::new(),
            meshy_headers: BTreeMap::new(),
            bedrock_headers: BTreeMap::new(),
            xai_headers: BTreeMap::new(),
            meta_headers: BTreeMap::new(),
            ollama_enabled: false,
            codex_enabled: false,
            codex_originator: None,
            codex_reasoning_effort: None,
            codex_verbosity: None,
            codex_replay_reasoning: true,
            grok_enabled: false,
            file_uploads: true,
            anthropic_cache_ttl: None,
            fallback_order: Vec::new(),
            provider_order: Vec::new(),
            zero_retention: false,
            zero_retention_agreements: Vec::new(),
        }
    }
}

/// One API-keyed provider: its names, and where its key, host and headers
/// sit in [`Settings`].
pub(super) struct Keyed {
    /// The provider and the variable its key falls back to, from
    /// [`super::PROVIDER_KEYS`].
    pub(super) names: (&'static str, &'static str),
    /// The variable its host falls back to.
    pub(super) base_url_env: &'static str,
    /// The key's field.
    pub(super) key: fn(&mut Settings) -> &mut Option<String>,
    /// The host's field.
    pub(super) base_url: fn(&mut Settings) -> &mut Option<String>,
    /// The extra headers' field.
    pub(super) headers: fn(&Settings) -> &BTreeMap<String, String>,
}

/// Every API-keyed provider, in [`super::PROVIDER_KEYS`] order, which is the
/// order the CLI registers them in.
pub(super) const KEYED: [Keyed; 8] = [
    Keyed {
        names: super::PROVIDER_KEYS[0],
        base_url_env: "ANTHROPIC_BASE_URL",
        key: |s| &mut s.providers.anthropic_api_key,
        base_url: |s| &mut s.providers.anthropic_base_url,
        headers: |s| &s.providers.anthropic_headers,
    },
    Keyed {
        names: super::PROVIDER_KEYS[1],
        base_url_env: "OPENAI_BASE_URL",
        key: |s| &mut s.providers.openai_api_key,
        base_url: |s| &mut s.providers.openai_base_url,
        headers: |s| &s.providers.openai_headers,
    },
    Keyed {
        names: super::PROVIDER_KEYS[2],
        base_url_env: "GOOGLE_BASE_URL",
        key: |s| &mut s.providers.google_api_key,
        base_url: |s| &mut s.providers.google_base_url,
        headers: |s| &s.providers.google_headers,
    },
    Keyed {
        names: super::PROVIDER_KEYS[3],
        base_url_env: "OPENROUTER_BASE_URL",
        key: |s| &mut s.openrouter_api_key,
        base_url: |s| &mut s.providers.openrouter_base_url,
        headers: |s| &s.providers.openrouter_headers,
    },
    Keyed {
        names: super::PROVIDER_KEYS[4],
        base_url_env: "MESHY_BASE_URL",
        key: |s| &mut s.providers.meshy_api_key,
        base_url: |s| &mut s.providers.meshy_base_url,
        headers: |s| &s.providers.meshy_headers,
    },
    Keyed {
        names: super::PROVIDER_KEYS[5],
        base_url_env: "BEDROCK_BASE_URL",
        key: |s| &mut s.providers.bedrock_api_key,
        base_url: |s| &mut s.providers.bedrock_base_url,
        headers: |s| &s.providers.bedrock_headers,
    },
    Keyed {
        names: super::PROVIDER_KEYS[6],
        base_url_env: "XAI_BASE_URL",
        key: |s| &mut s.providers.xai_api_key,
        base_url: |s| &mut s.providers.xai_base_url,
        headers: |s| &s.providers.xai_headers,
    },
    Keyed {
        names: super::PROVIDER_KEYS[7],
        base_url_env: "META_AI_BASE_URL",
        key: |s| &mut s.providers.meta_api_key,
        base_url: |s| &mut s.providers.meta_base_url,
        headers: |s| &s.providers.meta_headers,
    },
];

impl Keyed {
    /// The provider's name.
    pub(super) fn provider(&self) -> &'static str {
        self.names.0
    }
}

impl Settings {
    /// Fill what the file left unset from the environment, the way the CLI's
    /// loader does: each key from its variable, each host from its own,
    /// Bedrock's region from AWS's variables, and Ollama's address from
    /// `OLLAMA_HOST`. A value in the file wins, a blank one included.
    pub(super) fn fill_from_env(
        &mut self,
        env: &dyn Fn(&str) -> Option<String>,
        origins: &mut BTreeMap<String, super::KeyOrigin>,
    ) {
        for keyed in &KEYED {
            let (provider, var) = keyed.names;
            let slot = (keyed.key)(self);
            if slot.is_none() {
                *slot = env(var);
                if slot.is_some() {
                    origins.insert(provider.to_string(), super::KeyOrigin::Env(var.to_string()));
                }
            }
            let base_url = (keyed.base_url)(self);
            if base_url.is_none() {
                *base_url = env(keyed.base_url_env);
            }
        }
        // Blank is unset: an exported empty variable is not a region.
        if self.providers.bedrock_region.is_none() {
            self.providers.bedrock_region = env("AWS_REGION")
                .or_else(|| env("AWS_DEFAULT_REGION"))
                .map(|r| r.trim().to_string())
                .filter(|r| !r.is_empty());
        }
        if self.ollama_base_url.is_none() {
            self.ollama_base_url = env("OLLAMA_HOST");
        }
    }

    /// Fill any key still unset from `store`, filed under
    /// `provider/<name>` as `lev auth` keeps them.
    pub(super) fn fill_from_store(
        &mut self,
        store: &dyn leviath_core::CredentialStore,
        origins: &mut BTreeMap<String, super::KeyOrigin>,
    ) {
        let accounts: Vec<String> = KEYED
            .iter()
            .map(|keyed| leviath_core::provider_account(keyed.provider()))
            .collect();
        let mut found = store.read_all(&accounts);
        for keyed in &KEYED {
            let slot = (keyed.key)(self);
            if slot.is_none() {
                *slot = found.remove(&leviath_core::provider_account(keyed.provider()));
                if slot.is_some() {
                    origins.insert(keyed.provider().to_string(), super::KeyOrigin::Keychain);
                }
            }
        }
    }
}
