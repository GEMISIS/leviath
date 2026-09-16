//! What this server remembers between requests, so a page load is a lookup
//! rather than a re-read of the machine.

/// The caches on [`AppState`](super::types::AppState), as one field so a new
/// one does not cost every test that builds a state another line.
///
/// `Default` is empty: a test builds a state with nothing remembered and the
/// first request fills it, exactly as the server does at start-up.
#[derive(Clone, Default)]
pub(super) struct ServeCaches {
    /// The parse cache over the runs directory that every listing route reads.
    pub(super) run_index: super::run_index::RunIndex,
}
