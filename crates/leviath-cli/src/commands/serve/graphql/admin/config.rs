//! The `updateConfig` field: the one write onto the daemon's own config file.

use async_graphql::Context;

use super::super::super::types::AppState;
use super::super::config_input::ConfigInput;
use super::super::error::IntoGraphql;
use super::super::types::machine::Config;

/// Change the machine's config.
///
/// A partial edit: a field left out leaves the setting alone, `null` clears
/// it, and a value sets it. An empty string is refused rather than read as a
/// clear, because a form that posts its empty box should be told rather than
/// obeyed. Every refusal happens before anything is written, so a request
/// that is going to fail leaves the file as it was.
pub(crate) async fn update_config(
    ctx: &Context<'_>,
    input: ConfigInput,
) -> async_graphql::Result<Config> {
    let state = ctx.data_unchecked::<AppState>();
    let written = super::super::super::core::config::write(input.into_request()).gql()?;
    // The models a settings page asks for next are the new config's, so the
    // catalogue starts on them now rather than when that request arrives.
    state
        .caches
        .model_catalog
        .request_refresh(state.current_config(), true);
    Ok(super::super::query::config_of(
        &written,
        &state.limits.request_limits,
        &state.config.health(),
        // True by construction: this mutation is behind the guard.
        true,
    ))
}
