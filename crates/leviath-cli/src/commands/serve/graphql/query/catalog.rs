//! The `models`, `providers` and `tools` fields: the catalogue this machine
//! can route a run to, whatever run asks.

use async_graphql::Context;

use super::super::super::blocking::blocking;
use super::super::super::types::AppState;
use super::super::error::IntoGraphql;
use super::super::inputs::BlueprintInput;
use super::super::types::catalog::{Model, Provider, SkippedTool, Tool, ToolGroup, ToolInventory};

/// Every model this machine can route to.
///
/// Answered from the catalogue this server keeps, so it costs no provider
/// call. Two providers can serve the same model id and bill to different
/// places, so `provider` is part of each answer rather than something a
/// client infers.
pub(crate) async fn models(
    ctx: &Context<'_>,
    provider: Option<String>,
    refresh: bool,
) -> Vec<Model> {
    let state = ctx.data_unchecked::<AppState>();
    let query = super::super::super::config_types::ModelsQuery { provider, refresh };
    let (_, listing) = super::super::super::config::models_with(state, &query).await;
    listing.0.iter().map(Model::from).collect()
}

/// The providers this machine can reach, configured or not.
///
/// `enabled` and `signedIn` are different questions with different
/// answers: a provider can be turned on with no credential stored, and a
/// credential can outlive the config entry that used it.
pub(crate) async fn providers(ctx: &Context<'_>) -> Vec<Provider> {
    let state = ctx.data_unchecked::<AppState>();
    super::super::super::providers::provider_infos(state)
        .iter()
        .map(Provider::from)
        .collect()
}

/// The tools a run on this machine can call.
///
/// Scoped to one blueprint's own directory when `blueprint` names one,
/// which is what an editor offering an `available_tools` list wants.
pub(crate) async fn tools(
    ctx: &Context<'_>,
    blueprint: Option<BlueprintInput>,
) -> async_graphql::Result<ToolInventory> {
    let state = ctx.data_unchecked::<AppState>();
    let named = match blueprint {
        Some(input) => Some(input.installed(state).await.gql()?),
        None => None,
    };
    let config = state.current_config();
    let dir = match named.as_deref() {
        Some(name) => Some(super::super::super::tools::agent_dir(&config, name).gql()?),
        None => None,
    };
    // The walk over a blueprint's own directory belongs on the blocking
    // pool.
    let inventory = blocking(move || {
        crate::tool_inventory::ToolInventory::discover(dir.as_deref(), named.as_deref())
    })
    .await;
    Ok(ToolInventory {
        tools: inventory.tools.into_iter().map(Tool::of).collect(),
        groups: leviath_core::blueprint::ToolGroup::ALL
            .iter()
            .map(|group| ToolGroup {
                name: group.token().to_string(),
                description: group.describe().to_string(),
            })
            .collect(),
        skipped: inventory
            .skipped
            .into_iter()
            .map(|skipped| SkippedTool {
                path: skipped.path.display().to_string(),
                reason: skipped.reason,
            })
            .collect(),
    })
}
