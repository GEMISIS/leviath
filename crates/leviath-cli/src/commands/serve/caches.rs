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
    /// The model listing `GET /api/models` answers from.
    pub(super) model_catalog: super::model_catalog::ModelCatalog,
    /// The subscription usage `GET /api/providers?quota=true` answers from.
    pub(super) provider_quota: super::quota_cache::QuotaCache,
    /// The exports this server has been asked for, and what became of them.
    pub(super) exports: super::core::export::Exports,
    /// Manifests already parsed, by digest. Five hundred runs of one blueprint
    /// parse it once.
    pub(super) blueprints: super::core::blueprints::BlueprintCache,
}
