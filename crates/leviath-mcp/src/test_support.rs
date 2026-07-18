//! Shared test-only tracing helper for `leviath-mcp`.
//!
//! A no-op `tracing::Subscriber` that reports every callsite/level as
//! enabled. Without a registered subscriber, `tracing`'s macros
//! short-circuit field-expression evaluation before the "is this level
//! enabled" check even runs -- so a line like
//! `tracing::info!(command = %command, "...")` shows a nonzero hit count
//! (the macro call itself executes) while the `%command` field-expansion
//! sub-region shows zero, even though the enclosing branch genuinely ran.
//! Setting this as the default subscriber for a test's duration (via the
//! `_guard` binding kept alive across `.await` points on a single-threaded
//! runtime) makes those field expressions actually evaluate.
//!
//! Shared via `crate::test_support::always_on_tracing_guard`.
//!
//! `tracing::subscriber::set_default` installs a thread-local, scope-guarded
//! default (returned as a `DefaultGuard`) rather than a process-global one,
//! so -- unlike `tracing::subscriber::set_global_default`, which can only
//! succeed once per test binary -- every test that calls
//! `always_on_tracing_guard()` gets its own independent, fully-functioning
//! subscriber for the lifetime of its guard, regardless of test execution
//! order or how many other tests (in this file or others) also call it.

pub(crate) struct AlwaysOnSubscriber;

impl tracing::Subscriber for AlwaysOnSubscriber {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
    fn event(&self, _event: &tracing::Event<'_>) {}
    fn enter(&self, _span: &tracing::span::Id) {}
    fn exit(&self, _span: &tracing::span::Id) {}
}

pub(crate) fn always_on_tracing_guard() -> tracing::subscriber::DefaultGuard {
    tracing::subscriber::set_default(AlwaysOnSubscriber)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn always_on_subscriber_span_methods_are_all_no_ops() {
        let _guard = always_on_tracing_guard();
        let span = tracing::info_span!("test-span", field = 1);
        span.record("field", 2);
        span.follows_from(&span);
        span.in_scope(|| {
            tracing::info!("inside span");
        });
    }
}
