//! What is remembered about each provider's last check, across surfaces.
//!
//! The wizard opened every provider on "not checked yet", including one the
//! daemon had primed an hour before and one `lev models` had just listed.
//! Each of those is a check, and they all land in the shared capability
//! cache (`model_capabilities.json`) as a [`ProviderCheck`] beside the model
//! list. The wizard reads them when it opens and records its own when they
//! land, so whichever surface asked last is what the next one shows.
//!
//! A check is only trusted for the credential it was made with: the record
//! carries a keyed fingerprint of the key (see [`crate::provider_checks`]),
//! and a row whose key differs opens unchecked. A sign-in and a local server
//! have nothing to fingerprint and match on `None`.

use std::collections::HashMap;
use std::path::PathBuf;

use leviath_providers::{CapabilityCache, CheckOutcome, ProviderCheck};

use crate::provider_checks::CheckKey;

use super::{Credential, Outcome, ProviderRow, Wizard};

impl Wizard {
    /// Where checks are kept, and the key they are fingerprinted with beside
    /// it. `None` (every test) reads and writes nothing.
    pub(crate) fn with_check_store(mut self, path: Option<PathBuf>) -> Self {
        self.check_key = path.as_deref().and_then(CheckKey::load_or_create);
        self.check_store = path;
        self
    }

    /// Open each configured provider, and each endpoint entry, on what the
    /// cache remembers about it, where the record was made with the
    /// credential the row holds now.
    pub(crate) fn seed_checks(&mut self, cache: &CapabilityCache) {
        let env_only = &self.env_only;
        let key = self.check_key.as_ref();
        for row in &mut self.providers {
            if !row.selected || row.provider.credential == Credential::Endpoint {
                continue;
            }
            let Some(check) = cache.check(row.provider.id) else {
                continue;
            };
            if expected_print(row_key(row, env_only), key).as_ref() != Some(&check.credential) {
                continue;
            }
            row.outcome = outcome_from(check, cache.model_ids(row.provider.id));
            row.checked_at = Some(check.checked_at);
        }
        for entry in &mut self.endpoints {
            let Some(check) = cache.check(&entry.name) else {
                continue;
            };
            if expected_print(entry.secret(), key).as_ref() != Some(&check.credential) {
                continue;
            }
            entry.outcome = outcome_from(check, cache.model_ids(&entry.name));
            entry.checked_at = Some(check.checked_at);
            entry.settle_default_model();
        }
    }

    /// The fingerprint a check of `id` records: of the key the entry or the
    /// row holds, or `None` where there is nothing to fingerprint (or no
    /// check key, which leaves the record matching no keyed row).
    fn check_credential(&self, id: &str) -> Option<String> {
        let secret = if let Some(entry) = self.endpoints.iter().find(|e| e.name == id) {
            entry.secret()
        } else {
            let row = self.providers.iter().find(|r| r.provider.id == id)?;
            row_key(row, &self.env_only)
        };
        expected_print(secret, self.check_key.as_ref()).flatten()
    }

    /// Record a check that just landed, so the next surface opens on it.
    /// Nothing is written for a check that never ran, or where there is no
    /// store; a store that cannot be written is logged, since the check
    /// itself is already on screen.
    pub(crate) fn remember_check(&self, id: &str, outcome: &Outcome, now: i64) {
        let Some(path) = &self.check_store else {
            return;
        };
        let result = match outcome {
            Outcome::Skipped => return,
            Outcome::Reachable { models } => CheckOutcome::Reachable {
                models: models.len(),
            },
            Outcome::Failed { message } => CheckOutcome::Failed {
                message: message.clone(),
            },
        };
        let mut cache = CapabilityCache::load_or_new(path, now);
        if let Outcome::Reachable { models } = outcome {
            cache.set_model_ids(id, models);
        }
        cache.record_check(
            id,
            ProviderCheck {
                checked_at: now,
                credential: self.check_credential(id),
                outcome: result,
            },
        );
        if let Err(e) = cache.save(path) {
            tracing::warn!(error = %e, provider = id, "could not record the provider check");
        }
    }
}

/// The key a provider row would be checked with: what was typed, else what
/// the environment supplies. A local server and a sign-in have none.
fn row_key<'a>(
    row: &'a ProviderRow,
    env_only: &'a HashMap<&'static str, String>,
) -> Option<&'a str> {
    match row.provider.credential {
        Credential::ApiKey if !row.value.is_empty() => Some(row.value.as_str()),
        Credential::ApiKey => row
            .from_env
            .and_then(|var| env_only.get(var))
            .map(String::as_str),
        _ => None,
    }
}

/// The fingerprint a row's check must carry to count: `Some(None)` for a row
/// with no secret, `Some(Some(print))` for a keyed one, and `None` when a
/// keyed row has no check key to fingerprint with, which nothing matches.
fn expected_print(secret: Option<&str>, key: Option<&CheckKey>) -> Option<Option<String>> {
    match secret {
        None => Some(None),
        Some(secret) => key.map(|key| Some(key.fingerprint(secret))),
    }
}

/// A row's outcome from a remembered check, with the model ids the cache
/// holds for it (the check records only how many).
fn outcome_from(check: &ProviderCheck, ids: Vec<String>) -> Outcome {
    match &check.outcome {
        CheckOutcome::Reachable { .. } => Outcome::Reachable { models: ids },
        CheckOutcome::Failed { message } => Outcome::Failed {
            message: message.clone(),
        },
    }
}

/// How long ago a check was, as a person would say it.
pub(crate) fn checked_ago(now: i64, then: i64) -> String {
    let secs = now.saturating_sub(then).max(0);
    let count = |n: i64, unit: &str| {
        if n == 1 {
            format!("1 {unit} ago")
        } else {
            format!("{n} {unit}s ago")
        }
    };
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3_600 {
        count(secs / 60, "minute")
    } else if secs < 86_400 {
        count(secs / 3_600, "hour")
    } else {
        count(secs / 86_400, "day")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::setup::state::tests::test_wizard;
    use crate::commands::setup::state::{Step, VerifyReply};
    use crate::config::Config;

    fn row(w: &Wizard, id: &str) -> usize {
        w.providers
            .iter()
            .position(|r| r.provider.id == id)
            .expect("the catalog offers it")
    }

    fn reachable(credential: Option<String>, at: i64) -> ProviderCheck {
        ProviderCheck {
            checked_at: at,
            credential,
            outcome: CheckOutcome::Reachable { models: 1 },
        }
    }

    /// `secret` fingerprinted under the wizard's check key.
    fn print(w: &Wizard, secret: &str) -> String {
        w.check_key
            .as_ref()
            .expect("a store brings a key")
            .fingerprint(secret)
    }

    /// [`bare_configured`], with its check store (and key) in the tempdir.
    fn configured() -> (tempfile::TempDir, Wizard) {
        let (dir, wizard) = bare_configured();
        let store = dir.path().join("model_capabilities.json");
        (dir, wizard.with_check_store(Some(store)))
    }

    /// A wizard over a config with an Anthropic key, an OpenAI key, a Google
    /// key from the environment, and one endpoint entry with a token.
    fn bare_configured() -> (tempfile::TempDir, Wizard) {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.providers.anthropic_api_key = Some("sk-ant-x".to_string());
        config.providers.openai_api_key = Some("sk-old".to_string());
        config.model_providers.insert(
            "myserver".to_string(),
            crate::config::ModelProviderConfig {
                kind: Some(crate::config::ModelProviderKind::OpenaiCompatible),
                base_url: Some("http://localhost:1234/v1".to_string()),
                api_key: Some("tok".to_string()),
                ..Default::default()
            },
        );
        let wizard = Wizard::new(
            config,
            &|name| (name == "GOOGLE_API_KEY").then(|| "sk-env".to_string()),
            Vec::new(),
            Vec::new(),
            dir.path(),
            std::sync::Arc::new(|_| true),
            Default::default(),
        );
        (dir, wizard)
    }

    /// A remembered check opens its row checked, but only when it was made
    /// with the key the row holds now: a typed key, one from the
    /// environment, a token on an endpoint entry, or nothing at all for a
    /// local server.
    #[test]
    fn remembered_checks_open_the_rows_they_were_made_for() {
        let (_dir, mut w) = configured();
        let ollama = row(&w, "ollama");
        w.providers[ollama].selected = true;
        let mut cache = CapabilityCache::new(1_000);
        cache.record_check("anthropic", reachable(Some(print(&w, "sk-ant-x")), 900));
        cache.set_model_ids("anthropic", &["claude-opus-5".to_string()]);
        // Made with a key this config no longer holds.
        cache.record_check("openai", reachable(Some(print(&w, "sk-new")), 900));
        cache.record_check("google", reachable(Some(print(&w, "sk-env")), 800));
        cache.record_check("ollama", reachable(None, 700));
        cache.record_check(
            "openrouter",
            ProviderCheck {
                checked_at: 600,
                credential: None,
                outcome: CheckOutcome::Failed {
                    message: "rejected - check the key".to_string(),
                },
            },
        );
        cache.record_check("myserver", reachable(Some(print(&w, "tok")), 500));
        cache.set_model_ids("myserver", &["local-a".to_string(), "local-b".to_string()]);
        w.seed_checks(&cache);

        let anthropic = &w.providers[row(&w, "anthropic")];
        assert_eq!(
            anthropic.outcome,
            Outcome::Reachable {
                models: vec!["claude-opus-5".to_string()]
            }
        );
        assert_eq!(anthropic.checked_at, Some(900));
        let openai = &w.providers[row(&w, "openai")];
        assert_eq!(
            openai.outcome,
            Outcome::Skipped,
            "a different key: not checked"
        );
        assert_eq!(openai.checked_at, None);
        assert_eq!(w.providers[row(&w, "google")].checked_at, Some(800));
        assert_eq!(w.providers[ollama].checked_at, Some(700));
        assert_eq!(
            w.providers[row(&w, "openrouter")].outcome,
            Outcome::Skipped,
            "a check for a provider this config does not have is not shown"
        );
        let entry = &w.endpoints[0];
        assert_eq!(entry.name, "myserver");
        assert_eq!(entry.checked_at, Some(500));
        assert_eq!(entry.default_model.as_deref(), Some("local-a"));
    }

    /// A failed check is remembered as such, and an endpoint entry whose
    /// token differs from the record's opens unchecked.
    #[test]
    fn a_remembered_failure_shows_and_a_changed_token_does_not() {
        let (_dir, mut w) = configured();
        let mut cache = CapabilityCache::new(1_000);
        cache.record_check(
            "anthropic",
            ProviderCheck {
                checked_at: 900,
                credential: Some(print(&w, "sk-ant-x")),
                outcome: CheckOutcome::Failed {
                    message: "rejected - check the key".to_string(),
                },
            },
        );
        cache.record_check("myserver", reachable(Some(print(&w, "other")), 500));
        w.seed_checks(&cache);
        assert_eq!(
            w.providers[row(&w, "anthropic")].outcome,
            Outcome::Failed {
                message: "rejected - check the key".to_string()
            }
        );
        assert_eq!(w.endpoints[0].outcome, Outcome::Skipped);
        assert_eq!(w.endpoints[0].checked_at, None);

        // A cache that holds nothing about these rows leaves them as they were.
        let (_other, mut bare) = configured();
        bare.seed_checks(&CapabilityCache::new(1));
        assert_eq!(bare.endpoints[0].checked_at, None);
        assert!(bare.providers.iter().all(|r| r.checked_at.is_none()));
    }

    /// With no check key, a keyed row matches nothing, not even a record
    /// with no fingerprint; a row with no secret still matches one.
    #[test]
    fn without_a_check_key_only_unkeyed_rows_match() {
        let (_dir, mut w) = bare_configured();
        assert!(w.check_key.is_none());
        let ollama = row(&w, "ollama");
        w.providers[ollama].selected = true;
        let mut cache = CapabilityCache::new(1_000);
        cache.record_check("anthropic", reachable(None, 900));
        cache.record_check("ollama", reachable(None, 800));
        w.seed_checks(&cache);
        assert_eq!(w.providers[row(&w, "anthropic")].checked_at, None);
        assert_eq!(w.providers[ollama].checked_at, Some(800));
        // And what it records for a keyed row carries no fingerprint.
        assert_eq!(w.check_credential("anthropic"), None);
    }

    /// A check that lands is written to the store with the row's key
    /// fingerprint and the ids it found; a failure is written as one; a
    /// check that never ran writes nothing; and no store means no file.
    #[test]
    fn a_landed_check_is_recorded_for_the_next_surface() {
        let (dir, w) = configured();
        let path = dir.path().join("model_capabilities.json");
        w.remember_check(
            "anthropic",
            &Outcome::Reachable {
                models: vec!["claude-opus-5".to_string()],
            },
            2_000,
        );
        w.remember_check(
            "myserver",
            &Outcome::Failed {
                message: "unreachable - check your network".to_string(),
            },
            2_001,
        );
        w.remember_check("ollama", &Outcome::Reachable { models: vec![] }, 2_002);
        let cache = CapabilityCache::load(&path).expect("written");
        let check = cache.check("anthropic").expect("recorded");
        assert_eq!(check.checked_at, 2_000);
        assert_eq!(check.credential, Some(print(&w, "sk-ant-x")));
        assert_eq!(check.outcome, CheckOutcome::Reachable { models: 1 });
        assert_eq!(cache.model_ids("anthropic"), ["claude-opus-5"]);
        let entry = cache.check("myserver").expect("recorded");
        assert_eq!(entry.credential, Some(print(&w, "tok")));
        assert_eq!(
            entry.outcome,
            CheckOutcome::Failed {
                message: "unreachable - check your network".to_string()
            }
        );
        assert_eq!(cache.check("ollama").expect("recorded").credential, None);
        // Never ran: nothing written. Unknown id: recorded with no key.
        w.remember_check("anthropic", &Outcome::Skipped, 3_000);
        assert_eq!(
            CapabilityCache::load(&path)
                .unwrap()
                .check("anthropic")
                .unwrap()
                .checked_at,
            2_000
        );
        w.remember_check("nobody", &Outcome::Reachable { models: vec![] }, 3_000);
        assert_eq!(
            CapabilityCache::load(&path)
                .unwrap()
                .check("nobody")
                .unwrap()
                .credential,
            None
        );
        // No store: nothing anywhere.
        let dir2 = tempfile::tempdir().unwrap();
        let bare = test_wizard(dir2.path());
        bare.remember_check("anthropic", &Outcome::Reachable { models: vec![] }, 1);
        assert!(std::fs::read_dir(dir2.path()).unwrap().next().is_none());
        // A store that cannot be written is logged, not a panic.
        let blocker = dir2.path().join("blocker");
        std::fs::write(&blocker, "x").unwrap();
        let stuck = test_wizard(dir2.path()).with_check_store(Some(blocker.join("caps.json")));
        stuck.remember_check("anthropic", &Outcome::Reachable { models: vec![] }, 1);
    }

    /// A reply that lands stamps its row and records itself; one for a
    /// provider nobody has is dropped.
    #[test]
    fn a_reply_stamps_the_row_and_records_the_check() {
        let (dir, mut w) = configured();
        let path = dir.path().join("model_capabilities.json");
        w.enter(Step::Providers);
        w.push_reply_for_test(VerifyReply {
            provider_id: "anthropic".to_string(),
            outcome: Outcome::Reachable {
                models: vec!["claude-opus-5".to_string()],
            },
        });
        w.push_reply_for_test(VerifyReply {
            provider_id: "nobody".to_string(),
            outcome: Outcome::Reachable { models: vec![] },
        });
        w.drain_verifications();
        let anthropic = &w.providers[row(&w, "anthropic")];
        assert!(anthropic.checked_at.is_some());
        assert!(!anthropic.checking);
        let cache = CapabilityCache::load(&path).expect("written");
        assert!(cache.check("anthropic").is_some());
        assert!(cache.check("nobody").is_none());
    }

    #[test]
    fn a_checks_age_reads_as_a_person_would_say_it() {
        assert_eq!(checked_ago(1_000, 1_000), "just now");
        assert_eq!(
            checked_ago(1_000, 1_100),
            "just now",
            "a clock that moved back"
        );
        assert_eq!(checked_ago(1_060, 1_000), "1 minute ago");
        assert_eq!(checked_ago(1_000 + 25 * 60, 1_000), "25 minutes ago");
        assert_eq!(checked_ago(1_000 + 3_600, 1_000), "1 hour ago");
        assert_eq!(checked_ago(1_000 + 5 * 3_600, 1_000), "5 hours ago");
        assert_eq!(checked_ago(1_000 + 86_400, 1_000), "1 day ago");
        assert_eq!(checked_ago(1_000 + 3 * 86_400, 1_000), "3 days ago");
    }
}
