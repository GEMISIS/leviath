//! Crate-local names for the shared helpers in `leviath-testkit`.
//!
//! Kept as a module (rather than importing testkit at every call site) so
//! the crate's established `crate::test_support::...` paths keep resolving.

pub(crate) use leviath_testkit::tracing_guard as always_on_tracing_guard;
