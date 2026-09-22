//! The `refreshModels` field: asking the providers what they serve now.
//!
//! Reading the catalogue is a query, and a query in this schema never reaches
//! out. Refreshing it dials every configured provider, so it is a mutation and
//! answers with the catalogue it just rebuilt.

use async_graphql::{Context, InputObject, SimpleObject};

use super::super::super::types::AppState;
use super::super::types::catalog::Model;

/// Which provider's models to go and ask for.
#[derive(Debug, InputObject)]
pub(crate) struct RefreshModelsRequest {
    /// The provider to refresh. Omitted means every configured one.
    pub(crate) provider: Option<String>,
}

/// What a refresh answers with.
#[derive(SimpleObject)]
pub(crate) struct RefreshModelsResult {
    /// The catalogue as it stands after the refresh.
    pub(crate) models: Vec<Model>,
}

/// Re-read what the providers serve, and answer with the catalogue.
///
/// A provider that cannot be reached leaves its own models as they were rather
/// than failing the request: the catalogue is a cache of several sources, and
/// one of them being down is no reason to answer nothing about the rest.
pub(crate) async fn refresh_models(
    ctx: &Context<'_>,
    request: RefreshModelsRequest,
) -> async_graphql::Result<RefreshModelsResult> {
    let state = ctx.data_unchecked::<AppState>();
    let query = super::super::super::config_types::ModelsQuery {
        provider: request.provider,
        refresh: true,
    };
    let (_, listing) = super::super::super::config::models_with(state, &query).await;
    Ok(RefreshModelsResult {
        models: listing.0.iter().map(Model::from).collect(),
    })
}
