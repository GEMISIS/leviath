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

/// The primed catalogue of every provider, keyed by provider name then model id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilityCache {
    /// The format version; a file whose version is not [`CACHE_VERSION`] is
    /// treated as absent.
    version: u32,
    /// When it was written, Unix seconds, so a reader can judge staleness.
    saved_at: i64,
    /// provider name -> (model id -> what that provider's listing said).
    providers: BTreeMap<String, BTreeMap<String, LearnedModel>>,
}

impl CapabilityCache {
    /// An empty cache stamped `saved_at` (Unix seconds).
    pub fn new(saved_at: i64) -> Self {
        Self {
            version: CACHE_VERSION,
            saved_at,
            providers: BTreeMap::new(),
        }
    }

    /// The cache at `path`, or `None` when there is no readable, parseable file
    /// of the current version. Never an error: `None` means "prime instead".
    pub fn load(path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        let cache: Self = serde_json::from_str(&text).ok()?;
        (cache.version == CACHE_VERSION).then_some(cache)
    }

    /// Record one provider's primed catalogue, replacing any it held.
    pub fn set(&mut self, provider: &str, models: BTreeMap<String, LearnedModel>) {
        self.providers.insert(provider.to_string(), models);
    }

    /// One provider's catalogue, if the cache holds it.
    pub fn get(&self, provider: &str) -> Option<&BTreeMap<String, LearnedModel>> {
        self.providers.get(provider)
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
