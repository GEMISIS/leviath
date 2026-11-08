//! The directory of providers: who they are and what credential each takes.
//!
//! Purpose-neutral metadata an embedder needs to put up its own "connect a
//! provider" screen without copying a table. What is *not* here: how a
//! front end words a row, what it shows in an empty field, and where its own
//! config stores the choice. Those belong to the front end, which joins them
//! to a row here by [`ProviderMeta::id`].
//!
//! The ids are registry names: what a blueprint's
//! `models = [{ provider = ... }]` names. A row is listed whether or not this
//! build compiled the provider in; see [`crate::compiled`] for that.

/// The shape of credential a provider takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Credential {
    /// A secret API key.
    ApiKey,
    /// An interactive sign-in, usually in a browser. There is nothing to
    /// type; the grant is held by the sign-in flow, not by a setting.
    Signin,
    /// A base URL of a server that needs no key, with a working default.
    BaseUrl,
    /// One or more user-named endpoints, each with an address and an
    /// optional key. The row is a preset the endpoints are filed under.
    Endpoint,
}

/// One provider, as the directory knows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderMeta {
    /// Registry name.
    pub id: &'static str,
    /// Name to show.
    pub display: &'static str,
    /// What to ask for.
    pub credential: Credential,
    /// Environment variable the credential is also read from.
    pub env_var: Option<&'static str>,
    /// Where to get a credential.
    pub signup_url: Option<&'static str>,
}

/// Every provider Leviath knows, in the order it offers them.
pub const PROVIDERS: &[ProviderMeta] = &[
    ProviderMeta {
        id: "anthropic",
        display: "Anthropic",
        credential: Credential::ApiKey,
        env_var: Some("ANTHROPIC_API_KEY"),
        signup_url: Some("https://console.anthropic.com/settings/keys"),
    },
    ProviderMeta {
        id: "openai",
        display: "OpenAI",
        credential: Credential::ApiKey,
        env_var: Some("OPENAI_API_KEY"),
        signup_url: Some("https://platform.openai.com/api-keys"),
    },
    ProviderMeta {
        id: "google",
        display: "Google",
        credential: Credential::ApiKey,
        env_var: Some("GOOGLE_API_KEY"),
        signup_url: Some("https://aistudio.google.com/app/apikey"),
    },
    ProviderMeta {
        id: "xai",
        display: "xAI",
        credential: Credential::ApiKey,
        env_var: Some("XAI_API_KEY"),
        signup_url: Some("https://console.x.ai"),
    },
    ProviderMeta {
        id: "meta",
        display: "Meta",
        credential: Credential::ApiKey,
        env_var: Some("META_AI_API_KEY"),
        signup_url: Some("https://dev.meta.ai/"),
    },
    ProviderMeta {
        id: "openrouter",
        display: "OpenRouter",
        credential: Credential::ApiKey,
        env_var: Some("OPENROUTER_API_KEY"),
        signup_url: Some("https://openrouter.ai/keys"),
    },
    ProviderMeta {
        id: "meshy",
        display: "Meshy",
        credential: Credential::ApiKey,
        env_var: Some("MESHY_API_KEY"),
        signup_url: Some("https://www.meshy.ai/api"),
    },
    ProviderMeta {
        id: "bedrock",
        display: "AWS Bedrock",
        credential: Credential::ApiKey,
        env_var: Some("AWS_BEARER_TOKEN_BEDROCK"),
        signup_url: Some("https://console.aws.amazon.com/bedrock/home#/api-keys"),
    },
    ProviderMeta {
        id: "codex",
        display: "OpenAI Codex",
        credential: Credential::Signin,
        env_var: None,
        signup_url: Some("https://chatgpt.com/codex"),
    },
    ProviderMeta {
        id: "grok",
        display: "Grok",
        credential: Credential::Signin,
        env_var: None,
        signup_url: Some("https://grok.com"),
    },
    ProviderMeta {
        id: "ollama",
        display: "Ollama",
        credential: Credential::BaseUrl,
        env_var: Some("OLLAMA_HOST"),
        signup_url: Some("https://ollama.com/download"),
    },
    ProviderMeta {
        id: "llama-cpp",
        display: "llama.cpp",
        credential: Credential::Endpoint,
        env_var: None,
        signup_url: Some("https://github.com/ggml-org/llama.cpp"),
    },
    ProviderMeta {
        id: "lm-studio",
        display: "LM Studio",
        credential: Credential::Endpoint,
        env_var: None,
        signup_url: Some("https://lmstudio.ai"),
    },
    ProviderMeta {
        id: "openai-compatible",
        display: "Custom OpenAI-compatible endpoint",
        credential: Credential::Endpoint,
        env_var: None,
        signup_url: None,
    },
];

/// The directory row for `id`, if there is one.
pub fn meta(id: &str) -> Option<&'static ProviderMeta> {
    PROVIDERS.iter().find(|p| p.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique() {
        let mut ids: Vec<&str> = PROVIDERS.iter().map(|p| p.id).collect();
        ids.sort_unstable();
        let total = ids.len();
        ids.dedup();
        assert_eq!(total, ids.len(), "duplicate provider ids");
    }

    #[test]
    fn meta_finds_a_row_by_id_and_only_by_id() {
        let anthropic = meta("anthropic").expect("listed");
        assert_eq!(anthropic.display, "Anthropic");
        assert_eq!(anthropic.credential, Credential::ApiKey);
        assert_eq!(anthropic.env_var, Some("ANTHROPIC_API_KEY"));
        assert!(meta("not-a-provider").is_none());
    }

    #[test]
    fn an_api_key_row_names_its_env_var_and_a_place_to_get_one() {
        for p in PROVIDERS
            .iter()
            .filter(|p| p.credential == Credential::ApiKey)
        {
            assert!(p.env_var.is_some(), "{} has no env var", p.id);
            assert!(p.signup_url.is_some(), "{} has no signup URL", p.id);
        }
        // A sign-in has no key to find in the environment.
        for p in PROVIDERS
            .iter()
            .filter(|p| p.credential == Credential::Signin)
        {
            assert!(p.env_var.is_none(), "{}", p.id);
        }
    }

    /// The provider kinds this crate can build and the directory cannot
    /// drift apart: every kind a build can carry is listed. The directory
    /// also lists endpoint presets, which are no kind of their own.
    #[test]
    fn every_buildable_kind_has_a_directory_row() {
        for (kind, _feature) in crate::compiled::KNOWN {
            let row = meta(kind);
            assert!(row.is_some(), "{kind} has no directory row");
            assert_ne!(row.unwrap().credential, Credential::Endpoint, "{kind}");
        }
    }

    #[test]
    fn every_credential_kind_is_in_use() {
        for kind in [
            Credential::ApiKey,
            Credential::Signin,
            Credential::BaseUrl,
            Credential::Endpoint,
        ] {
            assert!(PROVIDERS.iter().any(|p| p.credential == kind), "{kind:?}");
        }
    }

    #[cfg(feature = "bedrock")]
    #[test]
    fn bedrock_reads_the_variable_aws_tooling_reads() {
        assert_eq!(
            meta("bedrock").unwrap().env_var,
            Some(crate::bedrock::KEY_ENV)
        );
    }
}
