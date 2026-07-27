//! The providers `lev setup` can configure, and how each one is configured.
//!
//! The old wizard walked every user through every provider in a fixed order,
//! asking for four API keys whether or not they had them, then asked for a
//! `default_provider` as free text with no validation — so a typo produced a
//! config that only failed at the first agent run. This table replaces both:
//! the wizard shows it as a pick-list, and the default-provider choice is a
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
    /// Nothing — the provider is enabled by selecting it. Claude Code
    /// authenticates through its own CLI.
    None,
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
}

/// Every provider the wizard offers, in the order it offers them.
pub fn providers() -> Vec<Provider> {
    vec![
        Provider {
            id: "anthropic",
            display: "Anthropic",
            blurb: "Claude models. The default for every shipped blueprint.",
            credential: Credential::ApiKey,
            hint: "sk-ant-...",
            env_var: Some("ANTHROPIC_API_KEY"),
            signup_url: Some("https://console.anthropic.com/settings/keys"),
        },
        Provider {
            id: "openai",
            display: "OpenAI",
            blurb: "GPT models.",
            credential: Credential::ApiKey,
            hint: "sk-...",
            env_var: Some("OPENAI_API_KEY"),
            signup_url: Some("https://platform.openai.com/api-keys"),
        },
        Provider {
            id: "google",
            display: "Google (Gemini)",
            blurb: "Gemini models.",
            credential: Credential::ApiKey,
            hint: "AIza...",
            env_var: Some("GOOGLE_API_KEY"),
            signup_url: Some("https://aistudio.google.com/app/apikey"),
        },
        Provider {
            id: "openrouter",
            display: "OpenRouter",
            blurb: "One key, many vendors' models.",
            credential: Credential::ApiKey,
            hint: "sk-or-...",
            env_var: Some("OPENROUTER_API_KEY"),
            signup_url: Some("https://openrouter.ai/keys"),
        },
        Provider {
            id: "ollama",
            display: "Ollama (local)",
            blurb: "Models running on this machine. No key needed.",
            credential: Credential::BaseUrl,
            hint: DEFAULT_OLLAMA_URL,
            env_var: Some("OLLAMA_HOST"),
            signup_url: Some("https://ollama.com/download"),
        },
        Provider {
            id: "claude-code",
            display: "Claude Code transport",
            blurb: "Runs on your Claude subscription instead of an API key. \
                    The CLI adds ~130 tokens of its own context to every call, \
                    including your account email and the date. This cannot be \
                    disabled.",
            credential: Credential::None,
            hint: "",
            env_var: None,
            signup_url: None,
        },
    ]
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

/// Read a provider's currently-configured credential out of a config.
///
/// Note `openrouter_api_key` and `ollama_base_url` sit at the top level of
/// `Config` while the other three live under `[providers]` — a historical split
/// this function hides from everything else.
pub fn stored_credential(config: &Config, id: &str) -> Option<String> {
    match id {
        "anthropic" => config.providers.anthropic_api_key.clone(),
        "openai" => config.providers.openai_api_key.clone(),
        "google" => config.providers.google_api_key.clone(),
        "openrouter" => config.openrouter_api_key.clone(),
        "ollama" => config.ollama_base_url.clone(),
        _ => None,
    }
}

/// Write a provider's credential into a config. `None` clears it.
pub fn set_credential(config: &mut Config, id: &str, value: Option<String>) {
    match id {
        "anthropic" => config.providers.anthropic_api_key = value,
        "openai" => config.providers.openai_api_key = value,
        "google" => config.providers.google_api_key = value,
        "openrouter" => config.openrouter_api_key = value,
        "ollama" => config.ollama_base_url = value,
        // `claude-code` is a boolean, handled by the selection itself, and an
        // unknown id has nowhere to go.
        _ => {}
    }
}

/// Whether a provider counts as configured in this config: it has a credential,
/// or it needs none and is switched on.
pub fn is_configured(config: &Config, id: &str) -> bool {
    match id {
        "claude-code" => config.providers.claude_code_enabled,
        // Ollama is always usable at its default endpoint, but "configured"
        // here means "the user chose it", which is the stored URL.
        _ => stored_credential(config, id).is_some(),
    }
}

/// Redact a credential for display: first 8 characters, then `...`.
///
/// Counts *characters*, not bytes. A byte-based version both panicked on a key
/// containing a multi-byte character straddling byte 8 (the shape of issue
/// #115) and, worse, leaked: a 3-character 9-byte key is longer than 8 bytes,
/// so the byte branch would print every character of it followed by an "I
/// truncated this" marker.
pub fn redact(key: &str) -> String {
    if key.chars().count() <= 8 {
        "***".to_string()
    } else {
        format!("{}...", key.chars().take(8).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── the table ──────────────────────────────────────────────────────────

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
        assert!(all.iter().any(|p| p.credential == Credential::None));
    }

    #[test]
    fn the_default_provider_is_in_the_table() {
        // Otherwise a fresh config would name a provider the wizard can't
        // configure, which is how the old free-text prompt went wrong.
        let config = Config::default();
        assert!(
            providers().iter().any(|p| p.id == config.default_provider),
            "default_provider {} is not offered by the wizard",
            config.default_provider
        );
    }

    #[test]
    fn claude_code_states_its_privacy_cost_up_front() {
        // Offered, never chosen for the user, and never without the caveat.
        let all = providers();
        let cc = all
            .iter()
            .find(|p| p.id == "claude-code")
            .expect("the transport is offered");
        assert!(cc.blurb.contains("email"));
        assert!(cc.blurb.contains("cannot be disabled"));
        assert_eq!(cc.credential, Credential::None);
        assert!(!Config::default().providers.claude_code_enabled);
    }

    // ─── credential accessors ───────────────────────────────────────────────

    #[test]
    fn every_provider_with_a_credential_round_trips_through_the_config() {
        // Catches the top-level/`[providers]` split silently dropping a field.
        for p in providers()
            .iter()
            .filter(|p| p.credential != Credential::None)
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

    #[test]
    fn claude_code_is_configured_by_its_flag_not_a_credential() {
        let mut config = Config::default();
        assert!(!is_configured(&config, "claude-code"));
        // It has no credential slot, so writing one must not make it look on.
        set_credential(&mut config, "claude-code", Some("x".to_string()));
        assert!(!is_configured(&config, "claude-code"));

        config.providers.claude_code_enabled = true;
        assert!(is_configured(&config, "claude-code"));
    }

    // ─── redact ─────────────────────────────────────────────────────────────

    #[test]
    fn redact_hides_short_keys_entirely() {
        assert_eq!(redact(""), "***");
        assert_eq!(redact("abc"), "***");
        assert_eq!(redact("12345678"), "***");
    }

    #[test]
    fn redact_shows_a_recognisable_prefix_of_a_long_key() {
        assert_eq!(redact("sk-ant-api-key-12345"), "sk-ant-a...");
        assert_eq!(redact("123456789"), "12345678...");
    }

    #[test]
    fn redact_counts_characters_not_bytes() {
        // Issue #115: byte 8 falls inside the third '日' (bytes 6..9), which
        // used to panic.
        assert_eq!(redact("日本語日本語日本語"), "日本語日本語日本...");
        // 3 characters but 9 bytes -- the byte-length guard would have
        // classified this as "long" and printed the whole key.
        assert_eq!(redact("日本語"), "***");
        assert_eq!(redact("日本語日本語日本"), "***");
    }
}
