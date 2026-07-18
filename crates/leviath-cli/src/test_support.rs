//! Shared test-only helpers for this crate's `#[cfg(test)]` code.
//!
//! Keep this as the single crate-wide copy: `set_global_default` succeeds only
//! once per test binary, so only one `AlwaysOnSubscriber` can become the active
//! subscriber. Duplicate copies would leave the losing ones' trait methods as
//! uncovered dead code.

/// No-op `Subscriber` that reports every callsite as enabled. A `tracing::`
/// macro skips evaluating its fields when no subscriber is interested, so
/// running code under [`with_tracing`] (which installs this) is what makes
/// those macro-argument lines execute — and count as covered.
pub(crate) struct AlwaysOnSubscriber;

impl tracing::Subscriber for AlwaysOnSubscriber {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn register_callsite(
        &self,
        _metadata: &'static tracing::Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        tracing::subscriber::Interest::always()
    }
    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        // Callsites are cached as always-enabled, so `enabled` is never called
        // via the macros; call it here so it's exercised.
        assert!(self.enabled(event.metadata()));
    }
    fn enter(&self, _span: &tracing::span::Id) {}
    fn exit(&self, _span: &tracing::span::Id) {}
    fn max_level_hint(&self) -> Option<tracing::metadata::LevelFilter> {
        Some(tracing::metadata::LevelFilter::TRACE)
    }
}

/// Install [`AlwaysOnSubscriber`] as the process-wide default subscriber
/// (at most once per test binary) and run `f` under it.
pub(crate) fn with_tracing<T>(f: impl FnOnce() -> T) -> T {
    static INSTALLED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    INSTALLED.get_or_init(|| {
        // `rebuild_interest_cache` is required: callsites registered before the
        // global default is set are cached as interest=never, so without it the
        // macro bodies stay unreachable (and uncovered) in tests.
        let _ = tracing::subscriber::set_global_default(AlwaysOnSubscriber);
        tracing::callsite::rebuild_interest_cache();
    });
    f()
}

/// A value whose `Serialize` impl always returns `Err`, so tests can drive the
/// `?` error arm of the crate's `serde_json::to_string_pretty(...)?` helpers
/// (which serialize trivially-serializable structs that never fail on real input).
pub(crate) struct PoisonSerialize;

impl serde::Serialize for PoisonSerialize {
    fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        Err(serde::ser::Error::custom("PoisonSerialize always fails"))
    }
}

/// Write an `agent.leviath` manifest into `dir` and return its path.
///
/// Consolidates the `std::fs::write(dir.join("agent.leviath"), ...).unwrap()`
/// idiom repeated across the CLI command test modules. `contents` accepts
/// anything byte-like (`&str`, `String`, byte slices) so both manifest text
/// and deliberately-malformed byte payloads route through the same helper.
pub(crate) fn write_test_agent(
    dir: impl AsRef<std::path::Path>,
    contents: impl AsRef<[u8]>,
) -> std::path::PathBuf {
    let path = dir.as_ref().join("agent.leviath");
    std::fs::write(&path, contents).unwrap();
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_test_agent_creates_manifest_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_agent(dir.path(), "version = \"1.0\"\n");
        assert!(path.exists());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "version = \"1.0\"\n"
        );
    }

    #[test]
    fn poison_serialize_always_errs() {
        let err = serde_json::to_string(&PoisonSerialize).unwrap_err();
        assert!(err.to_string().contains("PoisonSerialize always fails"));
    }

    /// Exercises the span-related trait methods (`new_span`, `record`,
    /// `record_follows_from`, `enter`, `exit`, `event`) that plain
    /// `tracing::info!`/`warn!` event macros elsewhere don't reach.
    #[test]
    fn always_on_subscriber_span_methods_are_all_no_ops() {
        with_tracing(|| {
            let span = tracing::info_span!("test-span", field = tracing::field::Empty);
            span.record("field", 1);
            let other = tracing::info_span!("other-span");
            span.follows_from(&other);
            let _enter = span.enter();
            tracing::info!(parent: &span, "inside span");
        });
    }
}
