//! The `providerSignIn`, `providerSignOut`, `checkProvider` and
//! `probeModels` fields: subscription sign-in and the diagnostics that reach
//! a provider or an OpenAI-compatible endpoint.

use async_graphql::Context;

use super::super::super::types::AppState;
use super::super::config_input::EnvEntryInput;
use super::super::error::IntoGraphql;

/// A provider sign-in that is waiting for the person to finish it.
#[derive(Debug, async_graphql::SimpleObject)]
pub(crate) struct SignInStarted {
    /// The provider, by its canonical name.
    pub(crate) provider: String,
    /// Where the person has to go, on the serving host.
    pub(crate) authorize_url: String,
    /// Whether this is the sign-in somebody already started rather than a new
    /// one. The URL is the same either way, which is what a client needs.
    pub(crate) already_waiting: bool,
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
pub(crate) async fn provider_sign_in(
    ctx: &Context<'_>,
    provider: String,
) -> async_graphql::Result<SignInStarted> {
    let state = ctx.data_unchecked::<AppState>();
    let name = super::super::super::providers::canonical(&provider).gql()?;
    let started = super::super::super::providers::sign_in_started(state, name)
        .await
        .gql()?;
    Ok(SignInStarted {
        provider: started.provider,
        authorize_url: started.authorize_url,
        already_waiting: started.already_waiting,
    })
}

/// Forget a provider's stored sign-in.
///
/// The config is untouched: signing out is not turning the provider off, and
/// doing both would surprise anybody who meant to sign in again.
pub(crate) async fn provider_sign_out(
    ctx: &Context<'_>,
    provider: String,
) -> async_graphql::Result<bool> {
    let state = ctx.data_unchecked::<AppState>();
    let name = super::super::super::providers::canonical(&provider).gql()?;
    super::super::super::providers::signed_out(state, name)
        .await
        .gql()?;
    Ok(true)
}

/// Ask a provider whether the stored sign-in works.
///
/// It asks the account rather than reading a table, so a green answer means
/// the subscription really did agree, and the models are what that account may
/// use. That costs a request, which is why this is a mutation.
pub(crate) async fn check_provider(
    ctx: &Context<'_>,
    provider: String,
) -> async_graphql::Result<Vec<String>> {
    let state = ctx.data_unchecked::<AppState>();
    let name = super::super::super::providers::canonical(&provider).gql()?;
    super::super::super::providers::checked(state, name)
        .await
        .gql()
}

/// Ask an OpenAI-compatible endpoint what models it serves.
///
/// Makes this host open a connection to an address the caller names, which is
/// the same act as testing an MCP server, and it exists to precede writing a
/// gateway for it: a person picks a default from what the endpoint really
/// serves rather than typing a model id and hoping.
pub(crate) async fn probe_models(
    base_url: String,
    api_key: Option<String>,
    headers: Option<Vec<EnvEntryInput>>,
) -> async_graphql::Result<Vec<String>> {
    super::super::super::config::probed(
        super::super::super::config_types::ProbeModelsReq {
            base_url,
            api_key,
            headers: headers.map(|headers| {
                headers
                    .into_iter()
                    .map(|entry| (entry.name, entry.value))
                    .collect()
            }),
        },
        &leviath_providers::provider::build_http_client,
    )
    .await
    .gql()
}
