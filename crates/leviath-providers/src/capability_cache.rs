//! A shared on-disk cache of what each provider's live listing said about its
//! models.
//!
//! Priming a provider means one network call to its `/models`-style endpoint,
//! and it fills that provider's in-memory [`LearnedModels`](crate::learned).
//! Only a long-lived process (the daemon) keeps that warm; every short-lived
//! surface - `lev models`, `lev validate`, a serve handler, the dashboard - used
//! to build its own provider registry and re-prime, and when its prime timed out
//! it fell back to the compiled table's conservative defaults. So the same model
//! reported one context window inside the daemon and another from `lev models
//! show`.
//!
//! This is the shared source. The daemon writes it after a successful prime; any
//! process fills a freshly built registry from it before running, so all of them
//! answer a model's limits from the same numbers without each re-fetching. It is
//! only ever a convenience over the network, never authoritative: a missing,
//! unreadable, stale-versioned or out-of-date file just means "prime instead",
//! which is exactly what happened before a cache existed.

use crate::learned::LearnedModel;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// The on-disk format version. A file written by a newer or unrecognised
/// version is ignored rather than misread.
const CACHE_VERSION: u32 = 1;

/// What asking a provider concluded, the last time anything asked.
///
/// Every surface that checks a provider records here: the daemon after a
/// prime, `lev setup` after its check, `lev models` after a live listing. So
/// the wizard can open on "checked an hour ago, 12 models" rather than
/// "not checked yet" for a provider something else proved works, and a key
/// rejected on the last listing is shown as rejected until it is checked
/// again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCheck {
    /// When the provider was asked, Unix seconds.
    pub checked_at: i64,
    /// A fingerprint of the credential the check used, made by the caller,
    /// so a check made with a key that has since changed reads as no check
    /// at all. Never the credential, and never something a credential can be
    /// guessed back from: this file travels in bug reports, so the CLI's
    /// fingerprint is keyed with a secret that stays on the machine. `None`
    /// for a provider with nothing to fingerprint: a local server, or a
    /// sign-in kept elsewhere.
    pub credential: Option<String>,
    /// What it said.
    pub outcome: CheckOutcome,
}

/// The answer a provider gave when it was last asked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CheckOutcome {
    /// It answered, and listed this many models.
    Reachable {
        /// How many models it listed; the ids are under the provider's entry.
        models: usize,
    },
    /// It refused or could not be reached.
    Failed {
        /// What went wrong, as shown to a person.
        message: String,
    },
}

/// The primed catalogue of every provider, keyed by provider name then model id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilityCache {
    /// The format version; a file whose version is not [`CACHE_VERSION`] is
    /// treated as absent.
    version: u32,
    /// When it was written, Unix seconds. The daemon reloads a cache of any
    /// age and re-primes on its own schedule; [`Self::is_fresh`] is for an
    /// embedder that wants an age bound.
    saved_at: i64,
    /// provider name -> (model id -> what that provider's listing said).
    providers: BTreeMap<String, BTreeMap<String, LearnedModel>>,
    /// provider name -> what it said the last time anything asked it.
    /// Defaulted so a file written before checks were recorded still loads.
    #[serde(default)]
    checks: BTreeMap<String, ProviderCheck>,
}

impl CapabilityCache {
    /// An empty cache stamped `saved_at` (Unix seconds).
    pub fn new(saved_at: i64) -> Self {
        Self {
            version: CACHE_VERSION,
            saved_at,
            providers: BTreeMap::new(),
            checks: BTreeMap::new(),
        }
    }

    /// The cache at `path`, or `None` when there is no readable, parseable file
    /// of the current version. Never an error: `None` means "prime instead".
    pub fn load(path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        let cache: Self = serde_json::from_str(&text).ok()?;
        (cache.version == CACHE_VERSION).then_some(cache)
    }

    /// The cache at `path` to add to, stamped `now`: what is there, or an
    /// empty one when nothing readable is. A writer starts here so a check
    /// recorded by one surface survives a save by another.
    pub fn load_or_new(path: &Path, now: i64) -> Self {
        let mut cache = Self::load(path).unwrap_or_else(|| Self::new(now));
        cache.saved_at = now;
        cache
    }

    /// Record one provider's primed catalogue, replacing any it held.
    pub fn set(&mut self, provider: &str, models: BTreeMap<String, LearnedModel>) {
        self.providers.insert(provider.to_string(), models);
    }

    /// Record the model ids a listing named, keeping what a prime learned
    /// about the ones it already held and dropping the ones it no longer
    /// names. For a surface that has the ids but not the limits (`lev setup`,
    /// `lev models`), so it neither wipes the daemon's numbers nor keeps a
    /// model the provider stopped serving.
    pub fn set_model_ids(&mut self, provider: &str, ids: &[String]) {
        let known = self.providers.remove(provider).unwrap_or_default();
        let models = ids
            .iter()
            .map(|id| {
                let learned = known.get(id).cloned().unwrap_or_default();
                (id.clone(), learned)
            })
            .collect();
        self.providers.insert(provider.to_string(), models);
    }

    /// Record what a provider said when it was asked.
    pub fn record_check(&mut self, provider: &str, check: ProviderCheck) {
        self.checks.insert(provider.to_string(), check);
    }

    /// What a provider said the last time anything asked it, if anything has.
    pub fn check(&self, provider: &str) -> Option<&ProviderCheck> {
        self.checks.get(provider)
    }

    /// The model ids the cache holds for a provider, in listing order.
    pub fn model_ids(&self, provider: &str) -> Vec<String> {
        self.providers
            .get(provider)
            .map(|models| models.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// One provider's catalogue, if the cache holds it.
    pub fn get(&self, provider: &str) -> Option<&BTreeMap<String, LearnedModel>> {
        self.providers.get(provider)
    }

    /// The context window a primed listing recorded for `model` under
    /// `provider`, matched whole or by its last path segment.
    ///
    /// A gateway namespaces its ids (`x-ai/grok-4`) while a blueprint may name
    /// the bare model, so both spellings find the entry - the same rule
    /// [`LearnedModels::find_by_key`](crate::learned::LearnedModels::find_by_key)
    /// uses. `None` when the cache holds no such model, or recorded no window
    /// for it.
    pub fn context_window(&self, provider: &str, model: &str) -> Option<usize> {
        let models = self.providers.get(provider)?;
        models
            .get(model)
            .or_else(|| {
                models
                    .iter()
                    .find(|(id, _)| id.rsplit('/').next() == Some(model))
                    .map(|(_, m)| m)
            })
            .and_then(|m| m.max_context_tokens)
    }

    /// Its age in seconds at `now` (Unix seconds), saturating at 0 for a file
    /// stamped in the future (a clock that moved back).
    pub fn age_secs(&self, now: i64) -> i64 {
        now.saturating_sub(self.saved_at).max(0)
    }

    /// Whether it is younger than `max_age_secs` at `now`.
    pub fn is_fresh(&self, now: i64, max_age_secs: i64) -> bool {
        self.age_secs(now) < max_age_secs
    }

    /// Write to `path` atomically, creating parent directories. The file is
    /// world-unreadable (`0o600`): a primed listing can carry per-account
    /// pricing, and a cache is not the place to widen who can read it.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        // `map(...).transpose()?` rather than `if let Some`: the no-parent case
        // is `None` flowing through, not a dead `else` arm a test must reach.
        path.parent().map(std::fs::create_dir_all).transpose()?;
        // Every field serializes, so this cannot fail; the fallible part is the
        // file write below, which is what the caller's `Result` is for.
        let json = serde_json::to_vec_pretty(self).expect("a CapabilityCache always serializes");
        leviath_sys::write_atomic(path, &json, Some(0o600))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> CapabilityCache {
        let mut cache = CapabilityCache::new(1_000);
        cache.set(
            "openrouter",
            BTreeMap::from([(
                "anthropic/claude-opus-5".to_string(),
                LearnedModel {
                    max_context_tokens: Some(200_000),
                    max_output_tokens: Some(64_000),
                    ..Default::default()
                },
            )]),
        );
        cache
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("caps").join("model_capabilities.json");
        let cache = sample();
        cache.save(&path).unwrap();
        let loaded = CapabilityCache::load(&path).unwrap();
        assert_eq!(loaded, cache);
        assert_eq!(
            loaded.get("openrouter").unwrap()["anthropic/claude-opus-5"].max_context_tokens,
            Some(200_000)
        );
        assert!(loaded.get("anthropic").is_none());
    }

    #[test]
    fn a_missing_or_unparseable_or_wrong_version_file_loads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.json");
        assert!(CapabilityCache::load(&missing).is_none());

        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, "{not json").unwrap();
        assert!(CapabilityCache::load(&bad).is_none());

        let old = dir.path().join("old.json");
        std::fs::write(
            &old,
            serde_json::json!({ "version": 999, "saved_at": 1, "providers": {} }).to_string(),
        )
        .unwrap();
        assert!(CapabilityCache::load(&old).is_none());
    }

    #[test]
    fn save_reports_the_io_error_when_a_parent_cannot_be_made() {
        let dir = tempfile::tempdir().unwrap();
        // A file sits where a directory would have to be, so create_dir_all fails.
        let file = dir.path().join("in-the-way");
        std::fs::write(&file, "x").unwrap();
        let path = file.join("sub").join("cache.json");
        assert!(CapabilityCache::new(1).save(&path).is_err());
    }

    #[test]
    fn context_window_matches_whole_or_by_last_segment() {
        let mut cache = CapabilityCache::new(1);
        cache.set(
            "openrouter",
            BTreeMap::from([
                (
                    "x-ai/grok-4".to_string(),
                    LearnedModel {
                        max_context_tokens: Some(256_000),
                        ..Default::default()
                    },
                ),
                (
                    // A model the listing named but recorded no window for.
                    "vendor/no-window".to_string(),
                    LearnedModel::default(),
                ),
            ]),
        );
        // Whole-id match.
        assert_eq!(
            cache.context_window("openrouter", "x-ai/grok-4"),
            Some(256_000)
        );
        // Last-segment match: a blueprint naming the bare model still resolves.
        assert_eq!(cache.context_window("openrouter", "grok-4"), Some(256_000));
        // A model with no recorded window.
        assert_eq!(cache.context_window("openrouter", "no-window"), None);
        // An unknown model, and an unknown provider.
        assert_eq!(cache.context_window("openrouter", "nope"), None);
        assert_eq!(cache.context_window("anthropic", "x-ai/grok-4"), None);
    }

    #[test]
    fn a_check_is_recorded_kept_across_a_reload_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model_capabilities.json");
        let mut cache = sample();
        cache.record_check(
            "openrouter",
            ProviderCheck {
                checked_at: 1_000,
                credential: Some("0123abcd0123abcd".to_string()),
                outcome: CheckOutcome::Reachable { models: 1 },
            },
        );
        cache.record_check(
            "openai",
            ProviderCheck {
                checked_at: 900,
                credential: None,
                outcome: CheckOutcome::Failed {
                    message: "rejected - check the key".to_string(),
                },
            },
        );
        cache.save(&path).unwrap();
        // Another writer starts from the file and keeps what is there.
        let later = CapabilityCache::load_or_new(&path, 2_000);
        assert_eq!(later.age_secs(2_000), 0);
        assert_eq!(later.check("openrouter").unwrap().checked_at, 1_000);
        assert_eq!(
            later.check("openai").unwrap().outcome,
            CheckOutcome::Failed {
                message: "rejected - check the key".to_string()
            }
        );
        assert!(later.check("anthropic").is_none());
        assert_eq!(later.model_ids("openrouter"), ["anthropic/claude-opus-5"]);
        assert!(later.model_ids("anthropic").is_empty());
        // No file: an empty cache stamped now.
        let fresh = CapabilityCache::load_or_new(&dir.path().join("none.json"), 5);
        assert_eq!(fresh, CapabilityCache::new(5));
    }

    #[test]
    fn a_file_written_before_checks_existed_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old.json");
        std::fs::write(
            &old,
            serde_json::json!({ "version": 1, "saved_at": 1, "providers": {} }).to_string(),
        )
        .unwrap();
        let cache = CapabilityCache::load(&old).unwrap();
        assert!(cache.check("openrouter").is_none());
    }

    #[test]
    fn listed_ids_keep_what_a_prime_learned_and_drop_the_rest() {
        let mut cache = sample();
        cache.set_model_ids(
            "openrouter",
            &[
                "anthropic/claude-opus-5".to_string(),
                "new/model".to_string(),
            ],
        );
        let models = cache.get("openrouter").unwrap();
        assert_eq!(
            models["anthropic/claude-opus-5"].max_context_tokens,
            Some(200_000),
            "the learned window survives"
        );
        assert_eq!(models["new/model"], LearnedModel::default());
        cache.set_model_ids("openrouter", &["new/model".to_string()]);
        assert!(
            !cache
                .get("openrouter")
                .unwrap()
                .contains_key("anthropic/claude-opus-5")
        );
        // A provider the cache never held gets the ids with nothing learned.
        cache.set_model_ids("anthropic", &["claude-opus-5".to_string()]);
        assert_eq!(cache.model_ids("anthropic"), ["claude-opus-5"]);
    }

    #[test]
    fn set_replaces_a_providers_entry() {
        let mut cache = sample();
        cache.set("openrouter", BTreeMap::new());
        assert!(cache.get("openrouter").unwrap().is_empty());
    }

    #[test]
    fn age_and_freshness_read_the_clock_the_caller_passes() {
        let cache = CapabilityCache::new(1_000);
        assert_eq!(cache.age_secs(1_600), 600);
        // A clock that moved back never reports a negative age.
        assert_eq!(cache.age_secs(900), 0);
        assert!(cache.is_fresh(1_600, 3_600));
        assert!(!cache.is_fresh(5_000, 3_600));
    }
}
