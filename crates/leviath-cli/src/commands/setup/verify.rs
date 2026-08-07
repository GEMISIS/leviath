//! Proving a provider credential actually works, before the config is written.
//!
//! A `key.starts_with("sk-ant-")` check never touches the network, so ending
//! `lev setup` with "All API keys look valid." on that basis is a sentence
//! that is false for a revoked key, a key pasted with a trailing space, a key
//! for the wrong account, and every key belonging to a provider the check does
//! not cover at all (Google, OpenRouter, Ollama). The first time the user
//! learns otherwise is a failed agent run.
//!
//! Every provider already implements
//! [`list_models`](leviath_providers::Provider::list_models) against a real
//! endpoint - `/v1/models` on Anthropic and OpenAI, `/v1beta/models` on Gemini,
//! `/api/v1/models` on OpenRouter, `/api/tags` on Ollama - so one call both
//! proves the credential and returns the model list the wizard's default-model
//! picker needs. Two answers for the price of one round trip.
//!
//! ## The seam
//!
//! [`ProviderVerifier`] exists so no test ever reaches the network. Tests use a
//! canned implementation, `--no-verify` uses [`SkipVerifier`], and the binary
//! wires in [`LiveVerifier`]. A failed check is always a warning and never a
//! blocker: an offline laptop, a corporate proxy, or a provider outage must not
//! stop someone finishing setup.

use leviath_runtime::provider_creds::ProviderCreds;

/// What a verification attempt found out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Not attempted - `--no-verify`, or no credential to check.
    Skipped,
    /// The provider answered. Carries its model ids, for the model picker.
    Reachable {
        /// The model ids it advertised, which is what the model picker offers.
        models: Vec<String>,
    },
    /// The provider refused or could not be reached.
    Failed {
        /// What went wrong, shown next to the provider's row.
        message: String,
    },
}

impl Outcome {
    /// A short status line for the provider card.
    pub fn summary(&self) -> String {
        match self {
            Self::Skipped => "not checked".to_string(),
            Self::Reachable { models } if models.len() == 1 => "1 model".to_string(),
            Self::Reachable { models } => format!("{} models", models.len()),
            Self::Failed { message } => message.clone(),
        }
    }

    /// Model ids to offer in the default-model picker.
    pub fn models(&self) -> &[String] {
        match self {
            Self::Reachable { models } => models,
            Self::Skipped | Self::Failed { .. } => &[],
        }
    }

    /// Whether this outcome should be drawn as a problem.
    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }
}

/// Checks whether a set of provider credentials actually works.
///
/// The whole point is that the wizard never calls a provider directly, so its
/// tests never open a socket.
#[expect(
    async_fn_in_trait,
    reason = "same as dispatch::RiskyExecutors: a test seam, never a dyn object"
)] // callers are concrete; no `dyn` and no boxing needed
pub trait ProviderVerifier {
    /// Ask the provider whether these credentials work, and what models they
    /// reach. Never fails: an unreachable provider is an [`Outcome::Failed`],
    /// not an error, because the wizard reports it rather than stopping.
    async fn verify(&self, creds: &ProviderCreds) -> Outcome;
}

/// `--no-verify`: report everything as unchecked without a round trip.
pub struct SkipVerifier;

impl ProviderVerifier for SkipVerifier {
    async fn verify(&self, _creds: &ProviderCreds) -> Outcome {
        Outcome::Skipped
    }
}

/// Build a one-provider registry and ask it to list its models.
///
/// Split out of [`LiveVerifier`] so the mapping from "registry answer" to
/// [`Outcome`] is exercised without a network call: a registry built from
/// credentials for a provider name nothing recognises is empty, which drives
/// the `None` arm, and every other arm is the provider's own I/O.
pub async fn verify_via_registry(creds: &ProviderCreds) -> Outcome {
    let registry =
        leviath_runtime::provider_creds::build_provider_registry(std::slice::from_ref(creds));
    let Some(provider) = registry.get(&creds.name) else {
        return Outcome::Failed {
            message: format!("no provider named '{}'", creds.name),
        };
    };
    match provider.list_models().await {
        Ok(models) => Outcome::Reachable {
            models: models.into_iter().map(|m| m.id).collect(),
        },
        Err(e) => Outcome::Failed {
            message: describe(&e.to_string()),
        },
    }
}

/// Turn a provider error into something a person can act on.
///
/// The raw strings are HTTP-shaped (`API error 401: {"type":"error",...}`) and
/// the status code is the only part that tells the user what to *do*.
fn describe(raw: &str) -> String {
    if raw.contains("401") || raw.contains("Unauthorized") || raw.contains("invalid_api_key") {
        "rejected - check the key".to_string()
    } else if raw.contains("403") {
        "forbidden - the key is valid but lacks access".to_string()
    } else if raw.contains("429") {
        "rate limited - the key works".to_string()
    } else if raw.contains("timed out") || raw.contains("timeout") {
        "timed out - no answer from the provider".to_string()
    } else if raw.contains("dns") || raw.contains("connect") || raw.contains("Connection") {
        "unreachable - check your network".to_string()
    } else {
        raw.to_string()
    }
}

/// Production [`ProviderVerifier`]: really calls the provider.
///
/// Wired in only by the binary. Nothing in the library instantiates it, so no
/// test can accidentally reach the network through it.
pub struct LiveVerifier;

impl ProviderVerifier for LiveVerifier {
    async fn verify(&self, creds: &ProviderCreds) -> Outcome {
        verify_via_registry(creds).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_testkit::spawn_mock_server;

    fn creds(name: &str) -> ProviderCreds {
        ProviderCreds {
            name: name.to_string(),
            api_key: Some("sk-test".to_string()),
            base_url: None,
            model_capabilities: std::collections::HashMap::new(),
            request_timeout_secs: Some(1),
            rate_limit: None,
            options: std::collections::HashMap::new(),
        }
    }

    // ─── Outcome ────────────────────────────────────────────────────────────

    #[test]
    fn summary_reads_naturally_for_every_outcome() {
        assert_eq!(Outcome::Skipped.summary(), "not checked");
        assert_eq!(
            Outcome::Reachable {
                models: vec!["a".into()]
            }
            .summary(),
            "1 model"
        );
        assert_eq!(
            Outcome::Reachable {
                models: vec!["a".into(), "b".into()]
            }
            .summary(),
            "2 models"
        );
        assert_eq!(
            Outcome::Reachable { models: vec![] }.summary(),
            "0 models",
            "a provider that answers with nothing is still reachable"
        );
        assert_eq!(
            Outcome::Failed {
                message: "rejected - check the key".into()
            }
            .summary(),
            "rejected - check the key"
        );
    }

    #[test]
    fn only_a_reachable_outcome_offers_models() {
        assert_eq!(
            Outcome::Reachable {
                models: vec!["m".into()]
            }
            .models(),
            ["m"]
        );
        assert!(Outcome::Skipped.models().is_empty());
        assert!(
            Outcome::Failed {
                message: "x".into()
            }
            .models()
            .is_empty()
        );
    }

    #[test]
    fn only_a_failed_outcome_reads_as_a_problem() {
        assert!(
            Outcome::Failed {
                message: "x".into()
            }
            .is_failure()
        );
        assert!(!Outcome::Skipped.is_failure());
        assert!(!Outcome::Reachable { models: vec![] }.is_failure());
    }

    // ─── describe ───────────────────────────────────────────────────────────

    #[test]
    fn describe_turns_status_codes_into_advice() {
        assert_eq!(
            describe("API error 401: bad key"),
            "rejected - check the key"
        );
        assert_eq!(describe("Unauthorized"), "rejected - check the key");
        assert_eq!(describe("invalid_api_key"), "rejected - check the key");
        assert_eq!(
            describe("API error 403: no access"),
            "forbidden - the key is valid but lacks access"
        );
        // A 429 proves the credential works, which is the useful part.
        assert_eq!(
            describe("API error 429: slow down"),
            "rate limited - the key works"
        );
        assert_eq!(
            describe("operation timed out"),
            "timed out - no answer from the provider"
        );
        assert_eq!(
            describe("error trying to connect"),
            "unreachable - check your network"
        );
        assert_eq!(describe("dns error"), "unreachable - check your network");
        assert_eq!(
            describe("Connection refused"),
            "unreachable - check your network"
        );
    }

    #[test]
    fn describe_passes_through_anything_it_does_not_recognise() {
        // Better a raw provider message than a wrong guess about what it means.
        assert_eq!(describe("something entirely new"), "something entirely new");
    }

    // ─── verifiers ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn skip_verifier_never_reports_anything_but_skipped() {
        assert_eq!(
            SkipVerifier.verify(&creds("anthropic")).await,
            Outcome::Skipped
        );
        assert_eq!(
            SkipVerifier.verify(&creds("ollama")).await,
            Outcome::Skipped
        );
    }

    #[tokio::test]
    async fn an_unknown_provider_name_fails_without_touching_the_network() {
        // `build_provider_registry` silently ignores names it doesn't know, so
        // the registry comes back empty. Reporting that as "unreachable" would
        // send the user hunting for a network problem that isn't there.
        let outcome = verify_via_registry(&creds("not-a-real-provider")).await;

        assert_eq!(
            outcome,
            Outcome::Failed {
                message: "no provider named 'not-a-real-provider'".to_string()
            }
        );
    }

    #[tokio::test]
    async fn a_reachable_provider_reports_the_models_it_lists() {
        // The whole reason verification calls `list_models` rather than some
        // cheaper ping: one round trip both proves the credential and fills the
        // wizard's default-model picker.
        let url = spawn_mock_server(
            200,
            "OK",
            r#"{"models":[{"name":"llama3:8b"},{"name":"qwen2:7b"}]}"#,
        )
        .await;
        let mut creds = creds("ollama");
        creds.api_key = None;
        creds.base_url = Some(url);

        let outcome = verify_via_registry(&creds).await;

        assert_eq!(
            outcome,
            Outcome::Reachable {
                models: vec!["llama3:8b".to_string(), "qwen2:7b".to_string()]
            }
        );
        assert!(!outcome.is_failure());
        assert_eq!(outcome.summary(), "2 models");
    }

    #[tokio::test]
    async fn a_rejected_credential_is_reported_as_such_not_as_a_network_problem() {
        // A 401 from a real endpoint is the case this whole module exists for:
        // a prefix-only check calls this key valid.
        let url = spawn_mock_server(401, "Unauthorized", r#"{"error":"bad key"}"#).await;
        let mut creds = creds("ollama");
        creds.api_key = None;
        creds.base_url = Some(url);

        let outcome = verify_via_registry(&creds).await;

        assert_eq!(
            outcome,
            Outcome::Failed {
                message: "rejected - check the key".to_string()
            }
        );
    }

    #[tokio::test]
    async fn a_provider_pointed_at_a_dead_endpoint_fails_rather_than_hanging() {
        // Ollama needs no key and honours `base_url`, so it can be aimed at a
        // reserved TEST-NET-1 address (RFC 5737) that cannot route anywhere.
        // With a 1s timeout this is bounded, and it exercises the real
        // registry -> list_models -> error mapping path end to end.
        let mut creds = creds("ollama");
        creds.api_key = None;
        creds.base_url = Some("http://192.0.2.1:11434".to_string());

        let outcome = verify_via_registry(&creds).await;

        assert!(outcome.is_failure(), "expected a failure, got {outcome:?}");
        assert!(!outcome.summary().is_empty());
        assert!(outcome.models().is_empty());
    }

    #[tokio::test]
    async fn live_verifier_delegates_to_the_registry_path() {
        // Same unknown-provider input, so this asserts the delegation without
        // opening a socket.
        let outcome = LiveVerifier.verify(&creds("not-a-real-provider")).await;

        assert_eq!(
            outcome,
            Outcome::Failed {
                message: "no provider named 'not-a-real-provider'".to_string()
            }
        );
    }
}
