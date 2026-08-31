//! The providers `lev setup` can configure, and how each one is configured.
//!
//! This table is what keeps the wizard a pick-list rather than a fixed march
//! through every provider (asking for four API keys whether or not the user
//! has them) ending in a free-text `default_provider` with no validation -
//! where a typo produces a config that only fails at the first agent run. The
//! wizard shows the table as a pick-list, and the default-provider choice is a
//! radio over what was actually configured.
//!
//! Everything a provider needs to differ by lives here as data, so adding one
//! is a table entry rather than another branch in the wizard.

use crate::config::Config;

/// What a provider needs from the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Credential {
    /// An API key, entered masked.
    ApiKey,
    /// A base URL, with a working default.
    BaseUrl,
    /// One or more `[model_providers.<name>]` endpoints of kind
    /// `openai-compatible`, each with a name, an address and an optional key.
    /// The row is a preset; the entries under it are the wizard's own.
    Endpoint,
    /// A browser sign-in. Nothing is typed and nothing lands in `config.toml`:
    /// selecting the row turns the provider on, and the grant is taken by
    /// `lev auth login <id>` and stored outside the config entirely.
    Signin,
}

/// Where a provider's choice lives in a [`Config`], as the row itself
/// declares it.
///
/// Adding a provider used to mean adding an arm to `stored_credential`, one
/// to `set_credential`, sometimes one to `is_configured`, and an entry in
/// `configured_providers` - four places, none of which the compiler would
/// mention if you missed them, and one of which shipped missed. A row that
/// carries its own accessors is one place instead, and the functions below
/// are then written once against any provider rather than once per provider.
#[derive(Clone, Copy)]
pub enum Setting {
    /// One `Option<String>` on the config: an API key, or a base URL.
    /// `None` means "not configured".
    Text {
        /// Read the value.
        read: fn(&Config) -> Option<String>,
        /// Write it, or clear it with `None`.
        write: fn(&mut Config, Option<String>),
    },
    /// A `bool` on the config. The credential, if there is one, lives
    /// somewhere else entirely - a browser grant, or an installed CLI.
    Switch {
        /// Read the switch.
        read: fn(&Config) -> bool,
        /// Set it.
        write: fn(&mut Config, bool),
    },
    /// Written as `[model_providers]` entries under a name the user picks, so
    /// there is no single field to read: see `is_configured`.
    Endpoints,
}

/// Hand-written: function pointers have no useful `Debug`, and the shape is
/// what a reader wants anyway.
impl std::fmt::Debug for Setting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Text { .. } => "Text",
            Self::Switch { .. } => "Switch",
            Self::Endpoints => "Endpoints",
        })
    }
}

/// One configurable provider.
#[derive(Debug, Clone)]
pub struct Provider {
    /// Registry name. Must match what `provider_creds_from_config` builds and
    /// what a blueprint's `models = [{ provider = ... }]` names.
    pub id: &'static str,
    /// Name to show.
    pub display: &'static str,
    /// One line on what picking this means.
    pub blurb: &'static str,
    /// What to ask for.
    pub credential: Credential,
    /// Placeholder shown in the empty field.
    pub hint: &'static str,
    /// Environment variable this credential is also read from, so the wizard
    /// can say "already in your environment" instead of asking again.
    pub env_var: Option<&'static str>,
    /// Where to get a credential, opened on request.
    pub signup_url: Option<&'static str>,
    /// For an endpoint preset, the address a fresh entry starts with. `None`
    /// for the custom preset, which asks.
    pub preset_url: Option<&'static str>,
    /// Where this provider's choice lives in a [`Config`].
    pub setting: Setting,
}

/// Every provider the wizard offers, in the order it offers them.
pub(crate) fn providers() -> Vec<Provider> {
    vec![
        Provider {
            id: "anthropic",
            display: "Anthropic",
            blurb: "Claude models. The default for every shipped blueprint.",
            credential: Credential::ApiKey,
            hint: "sk-ant-...",
            env_var: Some("ANTHROPIC_API_KEY"),
            signup_url: Some("https://console.anthropic.com/settings/keys"),
            preset_url: None,
            setting: Setting::Text {
                read: |c| c.providers.anthropic_api_key.clone(),
                write: |c, v| c.providers.anthropic_api_key = v,
            },
        },
        Provider {
            id: "openai",
            display: "OpenAI",
            blurb: "GPT models.",
            credential: Credential::ApiKey,
            hint: "sk-...",
            env_var: Some("OPENAI_API_KEY"),
            signup_url: Some("https://platform.openai.com/api-keys"),
            preset_url: None,
            setting: Setting::Text {
                read: |c| c.providers.openai_api_key.clone(),
                write: |c, v| c.providers.openai_api_key = v,
            },
        },
        Provider {
            id: "google",
            display: "Google (Gemini)",
            blurb: "Gemini models.",
            credential: Credential::ApiKey,
            hint: "AIza...",
            env_var: Some("GOOGLE_API_KEY"),
            signup_url: Some("https://aistudio.google.com/app/apikey"),
            preset_url: None,
            setting: Setting::Text {
                read: |c| c.providers.google_api_key.clone(),
                write: |c, v| c.providers.google_api_key = v,
            },
        },
        Provider {
            id: "openrouter",
            display: "OpenRouter",
            blurb: "One key, many vendors' models.",
            credential: Credential::ApiKey,
            hint: "sk-or-...",
            env_var: Some("OPENROUTER_API_KEY"),
            signup_url: Some("https://openrouter.ai/keys"),
            preset_url: None,
            setting: Setting::Text {
                read: |c| c.openrouter_api_key.clone(),
                write: |c, v| c.openrouter_api_key = v,
            },
        },
        Provider {
            id: "codex",
            display: "OpenAI Codex (ChatGPT subscription)",
            blurb: "GPT-5.x billed to a ChatGPT plan instead of an API balance. \
                    Signs in with a browser.",
            credential: Credential::Signin,
            hint: "",
            env_var: None,
            signup_url: Some("https://chatgpt.com/codex"),
            preset_url: None,
            setting: Setting::Switch {
                read: |c| c.providers.codex_enabled,
                write: |c, v| c.providers.codex_enabled = v,
            },
        },
        Provider {
            id: "ollama",
            display: "Ollama (local)",
            blurb: "Models running on this machine. No key needed.",
            credential: Credential::BaseUrl,
            hint: DEFAULT_OLLAMA_URL,
            env_var: Some("OLLAMA_HOST"),
            signup_url: Some("https://ollama.com/download"),
            preset_url: None,
            // Two config fields, one setting. `ollama_enabled` is "I chose
            // Ollama" and the URL is "and it is not at the default address",
            // so a config that names a URL counts as chosen even if it
            // predates the switch. The default address is deliberately not
            // stored: pinning it would stop `$OLLAMA_HOST` and the built-in
            // default from applying.
            setting: Setting::Text {
                read: |c| {
                    (c.providers.ollama_enabled || c.ollama_base_url.is_some())
                        .then(|| c.ollama_base_url.clone().unwrap_or_default())
                },
                write: |c, v| {
                    c.providers.ollama_enabled = v.is_some();
                    c.ollama_base_url =
                        v.filter(|url| !url.is_empty() && url != DEFAULT_OLLAMA_URL);
                },
            },
        },
        Provider {
            id: "llama-cpp",
            display: "llama.cpp",
            blurb: "A llama.cpp server on this machine, over its OpenAI-compatible API. \
                    No key needed.",
            credential: Credential::Endpoint,
            hint: LLAMA_CPP_URL,
            env_var: None,
            signup_url: Some("https://github.com/ggml-org/llama.cpp"),
            preset_url: Some(LLAMA_CPP_URL),
            setting: Setting::Endpoints,
        },
        Provider {
            id: "lm-studio",
            display: "LM Studio",
            blurb: "LM Studio's local server, over its OpenAI-compatible API. No key \
                    needed.",
            credential: Credential::Endpoint,
            hint: LM_STUDIO_URL,
            env_var: None,
            signup_url: Some("https://lmstudio.ai"),
            preset_url: Some(LM_STUDIO_URL),
            setting: Setting::Endpoints,
        },
        Provider {
            id: "openai-compatible",
            display: "Custom OpenAI-compatible endpoint",
            blurb: "vLLM, BionicGPT, a gateway, or any server that speaks the OpenAI \
                    chat API: a name, a base URL, and a key or headers if it wants them.",
            credential: Credential::Endpoint,
            hint: "http://host:port/v1",
            env_var: None,
            signup_url: None,
            preset_url: None,
            setting: Setting::Endpoints,
        },
    ]
}

/// Where llama.cpp's server listens by default.
pub const LLAMA_CPP_URL: &str = "http://localhost:8080/v1";

/// Where LM Studio's server listens by default.
pub const LM_STUDIO_URL: &str = "http://localhost:1234/v1";

/// The catalogue id of the endpoint preset that best describes an entry
/// read from the config: the preset whose id the name starts with, then the
/// preset whose default address it uses, then the custom one.
///
/// Nothing in the file records which row created an entry, and nothing needs
/// to: the row only decides which heading the entry is shown under.
pub(crate) fn preset_for(name: &str, entry: &crate::config::ModelProviderConfig) -> &'static str {
    let presets: Vec<Provider> = providers()
        .into_iter()
        .filter(|p| p.credential == Credential::Endpoint)
        .collect();
    let by_name = presets
        .iter()
        .find(|p| name == p.id || name.starts_with(&format!("{}-", p.id)));
    let by_url = presets
        .iter()
        .find(|p| p.preset_url.is_some() && p.preset_url == entry.base_url.as_deref());
    by_name.or(by_url).map_or("openai-compatible", |p| p.id)
}

/// Ollama's default endpoint, and the value the wizard treats as "unset" so a
/// user who leaves it alone gets the built-in default rather than a pinned copy
/// of it in their config.
pub const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";

/// Local inference realistically serves one model at a time, so the default
/// concurrency for an Ollama-first setup is 1 rather than the usual 8: eight
/// concurrent requests against one Ollama instance queue and thrash rather than
/// going faster.
pub const OLLAMA_MAX_CONCURRENT_INFERENCES: usize = 1;

/// The row for `id`, if the catalog has one.
fn row(id: &str) -> Option<Provider> {
    providers().into_iter().find(|p| p.id == id)
}

/// Read a provider's currently-configured credential out of a config.
///
/// The `Switch` kinds answer a marker string rather than a secret: there is
/// no credential in the config to read, and the wizard's "is this configured"
/// and "did this change" both want a value that changes when the switch does.
pub(crate) fn stored_credential(config: &Config, id: &str) -> Option<String> {
    match row(id)?.setting {
        Setting::Text { read, .. } => read(config),
        Setting::Switch { read, .. } => read(config).then(|| "enabled".to_string()),
        Setting::Endpoints => None,
    }
}

/// Write a provider's credential into a config. `None` clears it.
pub(crate) fn set_credential(config: &mut Config, id: &str, value: Option<String>) {
    // An id the catalog does not have goes nowhere, which is what a
    // `_ => {}` arm used to say.
    let Some(row) = row(id) else {
        return;
    };
    match row.setting {
        Setting::Text { write, .. } => write(config, value),
        Setting::Switch { write, .. } => write(config, value.is_some()),
        Setting::Endpoints => {}
    }
}

/// Whether a provider counts as configured in this config.
pub(crate) fn is_configured(config: &Config, id: &str) -> bool {
    match row(id).map(|p| p.setting) {
        // An endpoint preset is "configured" when the file holds an endpoint
        // entry that sits under it.
        Some(Setting::Endpoints) => config
            .model_providers
            .iter()
            .any(|(name, entry)| entry.is_endpoint() && preset_for(name, entry) == id),
        _ => stored_credential(config, id).is_some(),
    }
}

/// Every provider this config has configured, in catalog order.
///
/// Derived from the table rather than restated: the list that decides which
/// providers can be the default used to be its own hard-coded tuple, and a
/// provider missing from it was configured, usable, and not offered.
pub(crate) fn configured(config: &Config) -> Vec<&'static str> {
    providers()
        .into_iter()
        .filter(|p| is_configured(config, p.id))
        .map(|p| p.id)
        .collect()
}

/// Redact a credential for display.
///
/// Delegates to `leviath_core::secrets::redact`, which keeps the **last** four
/// characters. Showing the *first eight* would give a different answer from
/// the HTTP logger's, and keep the wrong half: API keys are structured at
/// the front, so `sk-ant-a`, `sk-proj-` and `ghp_…` identify the issuer and, on
/// a short token, expose a meaningful fraction of the value. A suffix is
/// unstructured and just as good for "is this the key I think it is".
///
/// Kept as a named wrapper rather than replacing every call site, so this
/// module's UI code reads the same and there is one place to look if the
/// wizard ever needs a different presentation.
pub(crate) fn redact(key: &str) -> String {
    leviath_core::redact(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── the table ──────────────────────────────────────────────────────────

    /// The hand-written `Debug` names the shape, which is the useful half:
    /// function pointers print as addresses and tell a reader nothing.
    #[test]
    fn a_setting_debugs_as_its_shape() {
        let shapes: Vec<String> = providers()
            .iter()
            .map(|p| format!("{:?}", p.setting))
            .collect();
        for shape in &shapes {
            assert!(
                ["Text", "Switch", "Endpoints"].contains(&shape.as_str()),
                "{shape}"
            );
        }
        assert!(shapes.iter().any(|s| s == "Text"));
        assert!(shapes.iter().any(|s| s == "Switch"));
        assert!(shapes.iter().any(|s| s == "Endpoints"));
    }

    /// An endpoint preset has no field of its own to write: its entries live
    /// under names the user picks, and `is_configured` looks for those.
    /// Writing to the preset is a no-op rather than a panic or a stray key.
    #[test]
    fn writing_to_an_endpoint_preset_changes_nothing() {
        let mut config = Config::default();
        set_credential(&mut config, "llama-cpp", Some("http://h/v1".to_string()));
        assert!(config.model_providers.is_empty());
        assert!(stored_credential(&config, "llama-cpp").is_none());
        assert!(!is_configured(&config, "llama-cpp"));
    }

    /// And an id the catalog does not have goes nowhere at all.
    #[test]
    fn writing_to_an_unknown_provider_changes_nothing() {
        let mut config = Config::default();
        set_credential(&mut config, "not-a-provider", Some("x".to_string()));
        assert!(stored_credential(&config, "not-a-provider").is_none());
        assert!(!is_configured(&config, "not-a-provider"));
        // And nothing landed in any of the fields a real provider writes.
        assert!(config.providers.anthropic_api_key.is_none());
        assert!(config.openrouter_api_key.is_none());
        assert!(!config.providers.codex_enabled);
    }

    #[test]
    fn every_provider_has_a_distinct_id_and_is_described() {
        let all = providers();
        let mut ids: Vec<&str> = all.iter().map(|p| p.id).collect();
        ids.sort_unstable();
        let total = ids.len();
        ids.dedup();
        assert_eq!(total, ids.len(), "duplicate provider ids");

        for p in &all {
            assert!(!p.display.is_empty(), "provider {} has no label", p.id);
            assert!(!p.blurb.is_empty(), "provider {} has no blurb", p.id);
        }
    }

    #[test]
    fn every_api_key_provider_names_its_env_var_and_a_place_to_get_one() {
        // Both drive real behaviour: the env var lets the wizard say "already
        // in your environment" instead of asking again, and the URL backs the
        // "open the signup page" key.
        for p in providers()
            .iter()
            .filter(|p| p.credential == Credential::ApiKey)
        {
            assert!(p.env_var.is_some(), "{} has no env var", p.id);
            assert!(p.signup_url.is_some(), "{} has no signup URL", p.id);
            assert!(!p.hint.is_empty(), "{} has no placeholder", p.id);
        }
    }

    #[test]
    fn the_table_covers_every_credential_kind() {
        let all = providers();
        assert!(all.iter().any(|p| p.credential == Credential::ApiKey));
        assert!(all.iter().any(|p| p.credential == Credential::BaseUrl));
        assert!(all.iter().any(|p| p.credential == Credential::Endpoint));
    }

    /// The two local presets start at their server's own default port and
    /// the custom one asks; every preset is an endpoint, and nothing else
    /// carries a preset address.
    #[test]
    fn the_endpoint_presets_carry_their_default_addresses() {
        let all = providers();
        let by_id = |id: &str| all.iter().find(|p| p.id == id).expect("in the table");
        assert_eq!(by_id("llama-cpp").preset_url, Some(LLAMA_CPP_URL));
        assert_eq!(by_id("lm-studio").preset_url, Some(LM_STUDIO_URL));
        assert_eq!(by_id("openai-compatible").preset_url, None);
        for p in &all {
            assert_eq!(
                p.preset_url.is_some(),
                p.credential == Credential::Endpoint && p.id != "openai-compatible",
                "{}",
                p.id
            );
        }
    }

    /// Which heading an entry from the file is shown under: the name first,
    /// then the address, then custom.
    #[test]
    fn an_entry_is_filed_under_the_preset_its_name_or_address_names() {
        use crate::config::{ModelProviderConfig, ModelProviderKind};
        let at = |url: &str| ModelProviderConfig {
            kind: Some(ModelProviderKind::OpenaiCompatible),
            base_url: Some(url.to_string()),
            ..Default::default()
        };
        assert_eq!(preset_for("llama-cpp", &at("http://x")), "llama-cpp");
        assert_eq!(preset_for("llama-cpp-2", &at("http://x")), "llama-cpp");
        assert_eq!(
            preset_for("llama-cppish", &at("http://x")),
            "openai-compatible"
        );
        assert_eq!(preset_for("mine", &at(LM_STUDIO_URL)), "lm-studio");
        assert_eq!(preset_for("mine", &at("http://x")), "openai-compatible");

        let mut config = Config::default();
        assert!(!is_configured(&config, "llama-cpp"));
        config
            .model_providers
            .insert("box".to_string(), at(LLAMA_CPP_URL));
        assert!(is_configured(&config, "llama-cpp"));
        assert!(!is_configured(&config, "lm-studio"));
        assert!(!is_configured(&config, "openai-compatible"));
        // A script entry at that address is not an endpoint.
        config.model_providers.get_mut("box").unwrap().kind = None;
        assert!(!is_configured(&config, "llama-cpp"));
    }

    #[test]
    fn the_default_provider_is_in_the_table() {
        // Otherwise a fresh config would name a provider the wizard can't
        // configure - which is exactly how a free-text prompt once went wrong.
        let config = Config::default();
        assert!(
            providers().iter().any(|p| p.id == config.default_provider),
            "default_provider {} is not offered by the wizard",
            config.default_provider
        );
    }

    #[test]
    fn the_claude_code_transport_is_not_offered() {
        // It is configured from the config keys or `lev setup --claude-code`,
        // never from the pick list, so the wizard has no row for it and does
        // not count the flag as a configured provider of its own.
        assert!(providers().iter().all(|p| p.id != "claude-code"));
        assert!(
            providers()
                .iter()
                .all(|p| !p.display.contains("Claude Code"))
        );

        let mut config = Config::default();
        config.providers.claude_code_enabled = true;
        assert!(!is_configured(&config, "claude-code"));
    }

    // ─── credential accessors ───────────────────────────────────────────────

    /// A sign-in row carries no value, so selecting it is the whole
    /// configuration and what round-trips is the flag.
    #[test]
    fn a_sign_in_row_round_trips_as_a_flag() {
        let mut config = Config::default();
        assert!(!is_configured(&config, "codex"));
        assert!(stored_credential(&config, "codex").is_none());

        // Whatever value the wizard passes, what is stored is the flag: the
        // grant itself never touches the config.
        set_credential(&mut config, "codex", Some("anything".to_string()));
        assert!(config.providers.codex_enabled);
        assert!(is_configured(&config, "codex"));
        assert!(stored_credential(&config, "codex").is_some());

        set_credential(&mut config, "codex", None);
        assert!(!config.providers.codex_enabled);
        assert!(!is_configured(&config, "codex"));
    }

    /// No token or account may reach `config.toml` through this row.
    #[test]
    fn a_sign_in_never_writes_a_credential_into_the_config() {
        let mut config = Config::default();
        set_credential(&mut config, "codex", Some("at-secret".to_string()));
        let rendered = toml::to_string(&config).expect("serializes");
        assert!(!rendered.contains("at-secret"), "{rendered}");
    }

    #[test]
    fn every_provider_with_a_credential_round_trips_through_the_config() {
        // Catches the top-level/`[providers]` split silently dropping a field.
        // Only the credentials that carry a value: an endpoint's live under
        // `[model_providers]`, and a sign-in carries no value at all.
        for p in providers()
            .iter()
            .filter(|p| !matches!(p.credential, Credential::Endpoint | Credential::Signin))
        {
            let mut config = Config::default();
            assert!(
                stored_credential(&config, p.id).is_none(),
                "{} starts set",
                p.id
            );

            set_credential(&mut config, p.id, Some("value".to_string()));
            assert_eq!(
                stored_credential(&config, p.id).as_deref(),
                Some("value"),
                "{} did not round trip",
                p.id
            );
            assert!(is_configured(&config, p.id), "{} reads unconfigured", p.id);

            set_credential(&mut config, p.id, None);
            assert!(
                stored_credential(&config, p.id).is_none(),
                "{} did not clear",
                p.id
            );
            assert!(!is_configured(&config, p.id));
        }
    }

    #[test]
    fn an_unknown_provider_id_stores_nothing_and_reads_back_nothing() {
        let mut config = Config::default();
        set_credential(&mut config, "not-a-provider", Some("x".to_string()));

        assert!(stored_credential(&config, "not-a-provider").is_none());
        assert!(!is_configured(&config, "not-a-provider"));
    }

    // ─── redact ─────────────────────────────────────────────────────────────

    /// The wizard shows the last four characters, matching the HTTP logger.
    /// Showing the first *eight* would be a second answer to "how much of a
    /// secret is safe to print", and the wrong half: API keys are structured
    /// at the front, so `sk-ant-a` names the issuer and, on a short token, is
    /// a meaningful fraction of the value.
    #[test]
    fn redact_hides_short_keys_entirely() {
        assert_eq!(redact(""), "****");
        assert_eq!(redact("abc"), "****");
        assert_eq!(redact("12345678"), "****");
    }

    #[test]
    fn redact_shows_a_recognisable_suffix_of_a_long_key() {
        assert_eq!(redact("sk-ant-api-key-12345"), "****2345");
        assert_eq!(redact("123456789"), "****6789");
        // The issuer prefix must not survive.
        assert!(!redact("sk-ant-api-key-12345").contains("sk-ant"));
    }

    #[test]
    fn redact_counts_characters_not_bytes() {
        // A byte-based cut lands inside a multi-byte character and panics.
        assert_eq!(redact("日本語日本語日本語"), "****語日本語");
        // 3 characters but 9 bytes - a byte-length guard would call this "long"
        // and print the whole key.
        assert_eq!(redact("日本語"), "****");
        assert_eq!(redact("日本語日本語日本"), "****");
    }
}
