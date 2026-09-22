//! The `signInProvider`, `signOutProvider`, `checkProvider` and
//! `checkEndpoint` fields: subscription sign-in and the diagnostics that reach
//! a provider or an OpenAI-compatible endpoint.

use async_graphql::{Context, InputObject, SimpleObject};

use super::super::super::types::AppState;
use super::super::error::IntoGraphql;
use super::super::inputs::KeyValueWrite;
use super::super::types::catalog::{Model, Provider};

/// Which provider to sign in to.
#[derive(Debug, InputObject)]
pub(crate) struct SignInProviderRequest {
    /// The provider, by name.
    pub(crate) provider: String,
}

/// A sign-in that is waiting for the person to finish it.
#[derive(Debug, SimpleObject)]
pub(crate) struct SignInProviderResult {
    /// The provider as it stands now, which is still signed out: the grant
    /// lands when the person finishes in the browser.
    pub(crate) provider: Provider,
    /// Where the person has to go, on the serving host.
    pub(crate) authorize_url: String,
    /// Whether this is the sign-in somebody already started rather than a new
    /// one. The URL is the same either way, which is what a client needs.
    pub(crate) is_already_waiting: bool,
}

/// Which provider to forget.
#[derive(Debug, InputObject)]
pub(crate) struct SignOutProviderRequest {
    /// The provider, by name.
    pub(crate) provider: String,
}

/// The provider, forgotten.
#[derive(Debug, SimpleObject)]
pub(crate) struct SignOutProviderResult {
    /// The provider as it stands now: `signedIn` false, and `enabled`
    /// untouched, because signing out is not turning the provider off.
    pub(crate) provider: Provider,
}

/// Which provider to ask.
#[derive(Debug, InputObject)]
pub(crate) struct CheckProviderRequest {
    /// The provider, by name.
    pub(crate) provider: String,
}

/// What the account said.
#[derive(Debug, SimpleObject)]
pub(crate) struct CheckProviderResult {
    /// The provider that was asked.
    pub(crate) provider: Provider,
    /// The models the account may use that this machine's catalogue also
    /// knows, with everything the catalogue knows about them.
    pub(crate) models: Vec<Model>,
    /// The ids the account named that the catalogue has no entry for. Ids
    /// rather than models, because that is all there is to say about them.
    pub(crate) unlisted_model_ids: Vec<String>,
}

/// Where to look, and what to send.
#[derive(Debug, InputObject)]
pub(crate) struct CheckEndpointRequest {
    /// Where the endpoint is.
    pub(crate) base_url: String,
    /// Its API key, when it wants one. Used for this one call and dropped:
    /// never written to the config, and this server logs no request body, so
    /// it reaches nothing on disk.
    pub(crate) api_key: Option<String>,
    /// Extra headers the request carries.
    pub(crate) headers: Option<Vec<KeyValueWrite>>,
}

/// What the endpoint said it serves.
#[derive(Debug, SimpleObject)]
pub(crate) struct CheckEndpointResult {
    /// The model ids, sorted, exactly as the endpoint spells them. Ids rather
    /// than models: nothing here is in this machine's catalogue yet, which is
    /// the point of asking.
    pub(crate) model_ids: Vec<String>,
}

/// One provider as the providers listing describes it.
///
/// Built from the catalog entry the caller resolved rather than searched for
/// by name: every act here starts by asking the catalog what the caller's
/// string means, so by the time this runs there is no name left to miss.
fn provider_of(state: &AppState, name: &str, display: &str) -> Provider {
    Provider::from(&super::super::super::providers::described(
        state, name, display,
    ))
}

/// Sign in to a subscription provider.
///
/// Answers as soon as there is a URL to go to, because what happens after
/// that is the person's business: they open it, approve, and the flow lands
/// the grant. Read `providers` to see whether it did.
///
/// The browser has to be on the serving host. The flow listens on a loopback
/// port there, so a browser anywhere else cannot complete it, and one sign-in
/// runs at a time because a second could not bind that port.
pub(crate) async fn sign_in_provider(
    ctx: &Context<'_>,
    request: SignInProviderRequest,
) -> async_graphql::Result<SignInProviderResult> {
    let state = ctx.data_unchecked::<AppState>();
    let (name, display) = super::super::super::providers::canonical(&request.provider).gql()?;
    let started = super::super::super::providers::sign_in_started(state, name)
        .await
        .gql()?;
    Ok(SignInProviderResult {
        provider: provider_of(state, name, display),
        authorize_url: started.authorize_url,
        is_already_waiting: started.already_waiting,
    })
}

/// Forget a provider's stored sign-in.
///
/// The config is untouched: signing out is not turning the provider off, and
/// doing both would surprise anybody who meant to sign in again.
pub(crate) async fn sign_out_provider(
    ctx: &Context<'_>,
    request: SignOutProviderRequest,
) -> async_graphql::Result<SignOutProviderResult> {
    let state = ctx.data_unchecked::<AppState>();
    let (name, display) = super::super::super::providers::canonical(&request.provider).gql()?;
    super::super::super::providers::signed_out(state, name)
        .await
        .gql()?;
    Ok(SignOutProviderResult {
        provider: provider_of(state, name, display),
    })
}

/// Ask a provider whether the stored sign-in works.
///
/// It asks the account rather than reading a table, so a green answer means
/// the subscription really did agree, and the models are what that account may
/// use. That costs a request, which is why this is a mutation.
pub(crate) async fn check_provider(
    ctx: &Context<'_>,
    request: CheckProviderRequest,
) -> async_graphql::Result<CheckProviderResult> {
    let state = ctx.data_unchecked::<AppState>();
    let (name, display) = super::super::super::providers::canonical(&request.provider).gql()?;
    let allowed = super::super::super::providers::checked(state, name)
        .await
        .gql()?;
    // The catalogue for this provider alone, so an id served by two providers
    // cannot bring the other one's entry back with it.
    let query = super::super::super::config_types::ModelsQuery {
        provider: Some(name.to_string()),
        refresh: false,
    };
    let (_, listing) = super::super::super::config::models_with(state, &query).await;
    let mut models = Vec::new();
    let mut unlisted = Vec::new();
    for id in allowed {
        match listing.0.iter().find(|entry| entry.id == id) {
            Some(entry) => models.push(Model::from(entry)),
            None => unlisted.push(id),
        }
    }
    Ok(CheckProviderResult {
        provider: provider_of(state, name, display),
        models,
        unlisted_model_ids: unlisted,
    })
}

/// Ask an OpenAI-compatible endpoint what models it serves.
///
/// Makes this host open a connection to an address the caller names, which is
/// the same act as checking an MCP server, and it exists to precede writing a
/// gateway for it: a person picks a default from what the endpoint really
/// serves rather than typing a model id and hoping.
pub(crate) async fn check_endpoint(
    request: CheckEndpointRequest,
) -> async_graphql::Result<CheckEndpointResult> {
    let model_ids = super::super::super::config::probed(
        super::super::super::config_types::ProbeModelsReq {
            base_url: request.base_url,
            api_key: request.api_key,
            headers: request.headers.map(|headers| {
                headers
                    .into_iter()
                    .map(|entry| (entry.key, entry.value))
                    .collect()
            }),
        },
        &leviath_providers::provider::build_http_client,
    )
    .await
    .gql()?;
    Ok(CheckEndpointResult { model_ids })
}
