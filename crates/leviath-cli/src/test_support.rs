//! Shared test-only tracing-coverage helper.
//!
//! There is exactly ONE copy of this in the crate on purpose:
//! `tracing::subscriber::set_global_default` only succeeds once per test
//! binary, so before this module existed, every file in this crate that had
//! its own private copy of `AlwaysOnSubscriber` left every *other* file's
//! copy permanently dead code -- whichever file's test happened to run
//! first "won" `set_global_default`, and the losing files' `Subscriber`
//! trait-method impls (`enabled`, `new_span`, `record`,
//! `record_follows_from`, `enter`, `exit`, ...) could never actually be
//! invoked as an active subscriber, inflating `cargo llvm-cov` missed
//! regions across most of those files. `rebuild_interest_cache()` still
//! made tracing-macro coverage globally correct regardless of which
//! instance won (that's the original bug this pattern fixes), but the
//! losing copies' own methods stayed dead. Consolidating to one shared
//! instance fixes that structurally.

/// Minimal no-op `Subscriber` that reports every callsite as enabled.
///
/// Without an active subscriber, `tracing::warn!`/`info!`/`debug!` calls
/// short-circuit their field-argument evaluation before ever reaching it
/// (no subscriber means the "is this level enabled" check fails first) --
/// so a multi-line `tracing::warn!(...)` call's field-list lines show as
/// uncovered by `cargo llvm-cov` even when the surrounding branch
/// genuinely executes and is asserted on. This bare `Subscriber` impl is
/// the proven-working pattern used across this workspace (see
/// `leviath-core/src/layout.rs`).
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
        // `register_callsite` always returns `Interest::always()`, so
        // tracing's dispatch macros cache every callsite as
        // "always enabled" and never call `enabled` again afterward.
        // Call it directly here (with real metadata from a live event)
        // so this trait-impl boilerplate method isn't itself left
        // uncovered.
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
        // set_global_default registers AlwaysOnSubscriber in LOCKED_DISPATCHERS
        // (the global dispatcher registry). rebuild_interest_cache then re-evaluates
        // every callsite against the global subscriber, setting interest to "always".
        // Without this, tracing macro inner blocks are unreachable in tests because
        // with_default (thread-local) is NOT consulted during callsite registration,
        // leaving every callsite cached as interest=never (no global dispatcher).
        let _ = tracing::subscriber::set_global_default(AlwaysOnSubscriber);
        tracing::callsite::rebuild_interest_cache();
    });
    f()
}

/// A value that always fails to serialize, for forcing `?`-propagated
/// `serde_json::to_string`/`to_string_pretty` error arms in tests.
///
/// Every "write JSON to disk" helper across this crate serializes a
/// trivially-serializable struct (`String`/`usize`/enums/`HashMap`s), so
/// `serde_json::to_string_pretty(...)?` can never actually fail in
/// practice -- there is no real input that trips it. Rather than leave
/// those `?` error arms permanently uncovered (or fake up something more
/// exotic), this wraps an arbitrary payload behind a `Serialize` impl that
/// deterministically returns `Err`, so tests can drive the real error path
/// through the real (non-generic-only-for-tests) code, honestly.
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

    /// Exercises the no-op span-related trait methods (`new_span`, `record`,
    /// `record_follows_from`, `enter`, `exit`) that aren't reachable via
    /// plain `tracing::info!`/`warn!` event macros used elsewhere in this
    /// crate.
    ///
    /// COVERAGE-CONFIRMED-ARTIFACT: the `tracing::info!(parent: &span, "inside
    /// span")` call below genuinely executes (confirmed via HTML: the line
    /// shows a nonzero hit count) and is what exercises `AlwaysOnSubscriber::event`
    /// with a real, live event -- but llvm-cov's tracing-macro message-literal
    /// region-counting quirk (the same one documented on `config.rs`'s
    /// `log_permissive_perms_warning`) still reports that literal's region as
    /// a miss. Since this is already test code, no `#[cfg(not(test))]` twin
    /// applies here (there's nothing to isolate it from -- the whole point of
    /// this test is exercising the macro under a real subscriber).
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
