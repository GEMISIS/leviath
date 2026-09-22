//! The `updateConfig` field: the one write onto the daemon's own config file.

use async_graphql::{Context, SimpleObject};

use super::super::super::types::AppState;
use super::super::config_input::UpdateConfigRequest;
use super::super::error::IntoGraphql;
use super::super::types::machine::Config;

/// What changing the config left behind.
#[derive(Debug, SimpleObject)]
pub(crate) struct UpdateConfigResult {
    /// The config as it now stands, read back through the same shape a query
    /// answers with.
    pub(crate) config: Config,
}

/// Change the machine's config.
///
/// A partial edit in four parts: `set` names the settings to change, `clear`
/// the ones to take back to nothing, `providers` the per-provider settings,
/// and the two gateway lists what to add and what to remove. What none of them
/// mentions is left alone. Every refusal happens before anything is written,
/// so a request that is going to fail leaves the file as it was.
pub(crate) async fn update_config(
    ctx: &Context<'_>,
    request: UpdateConfigRequest,
) -> async_graphql::Result<UpdateConfigResult> {
    let state = ctx.data_unchecked::<AppState>();
    let written = super::super::super::core::config::write(request.into_request().gql()?).gql()?;
    // The models a settings page asks for next are the new config's, so the
    // catalogue starts on them now rather than when that request arrives.
    state
        .caches
        .model_catalog
        .request_refresh(state.current_config(), true);
    Ok(UpdateConfigResult {
        config: super::super::query::config_of(
            &written,
            &state.limits.request_limits,
            &state.config.health(),
            // True by construction: this mutation is behind the guard.
            true,
        ),
    })
}
