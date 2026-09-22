//! The `models`, `providers`, `tools` and `toolGroups` fields: the catalogue
//! this machine can route a run to, whatever run asks.

use async_graphql::{Context, ID};

use super::super::super::blocking::blocking;
use super::super::super::types::AppState;
use super::super::connection::Connection;
use super::super::error::IntoGraphql;
use super::super::paging::order::{OrderDirection, Term};
use super::super::scalars::Cursor;
use super::super::types::catalog::{
    Model, ModelFilter, ModelOrder, ModelOrderField, Provider, ProviderFilter, ProviderOrder,
    ProviderOrderField, SkippedTool, Tool, ToolFilter, ToolGroup, ToolOrder, ToolOrderField,
    ToolSkips, provider_id,
};
use super::listing::{Window, connection, terms};

/// How many catalogue rows one page may carry.
///
/// A machine routing to three providers has tens of models; one pointed at a
/// gateway has thousands, and the same listing has to serve both.
pub(crate) const PAGE_CAP: usize = 500;

/// The order the models are read in when nothing says otherwise: by id, which
/// carries the provider first, so a provider's models stay together.
fn models_by_id() -> Vec<Term<ModelOrderField>> {
    vec![Term {
        field: ModelOrderField::Id,
        direction: OrderDirection::Asc,
    }]
}

/// Every model this machine can route to.
///
/// Answered from the catalogue this server keeps, so it costs no provider
/// call. Two providers can serve the same model id and bill to different
/// places, so the provider is part of each answer rather than something a
/// client infers.
pub(crate) async fn models(
    ctx: &Context<'_>,
    filter: Option<ModelFilter>,
    order_by: Option<Vec<ModelOrder>>,
    first: i32,
    after: Option<Cursor>,
) -> async_graphql::Result<Connection<Model>> {
    connection(
        catalogue(ctx).await,
        filter,
        terms(order_by, ModelOrder::term, models_by_id),
        |model: &Model| model.id.to_string(),
        Window {
            first,
            after,
            cap: PAGE_CAP,
            cap_name: "the model page cap",
        },
    )
    .await
}

/// One model, by the id this schema gives it.
///
/// Null for an id nothing serves. That covers a model a provider has retired
/// and one this machine has no provider for, which are the same answer to a
/// client: not routable from here.
pub(crate) async fn model(ctx: &Context<'_>, id: ID) -> Option<Model> {
    model_by_id(ctx, id.as_str()).await
}

/// One model, by the id this schema gives it, for `node`.
pub(crate) async fn model_by_id(ctx: &Context<'_>, id: &str) -> Option<Model> {
    catalogue(ctx)
        .await
        .into_iter()
        .find(|model| model.id.as_str() == id)
}

/// The catalogue this server holds, without asking any provider for a fresh
/// one: a query dials nothing, and `refreshModels` is the mutation that does.
async fn catalogue(ctx: &Context<'_>) -> Vec<Model> {
    let state = ctx.data_unchecked::<AppState>();
    let query = super::super::super::config_types::ModelsQuery {
        provider: None,
        refresh: false,
    };
    let (_, listing) = super::super::super::config::models_with(state, &query).await;
    listing.0.iter().map(Model::from).collect()
}

/// The order the providers are read in when nothing says otherwise: by the
/// registry name, which is the key a machine holds one provider per.
fn providers_by_name() -> Vec<Term<ProviderOrderField>> {
    vec![Term {
        field: ProviderOrderField::Name,
        direction: OrderDirection::Asc,
    }]
}

/// The providers a person signs in to through a browser, signed in or not.
///
/// Not every provider this machine can talk to. One that takes an API key is
/// never signed in to, so it is not here; `config.providers` is where every
/// provider this build knows is listed.
///
/// `enabled` and `signedIn` are different questions with different
/// answers: a provider can be turned on with no credential stored, and a
/// credential can outlive the config entry that used it.
pub(crate) async fn providers(
    ctx: &Context<'_>,
    filter: Option<ProviderFilter>,
    order_by: Option<Vec<ProviderOrder>>,
    first: i32,
    after: Option<Cursor>,
) -> async_graphql::Result<Connection<Provider>> {
    connection(
        reachable(ctx),
        filter,
        terms(order_by, ProviderOrder::term, providers_by_name),
        |provider: &Provider| provider.name.clone(),
        Window {
            first,
            after,
            cap: PAGE_CAP,
            cap_name: "the provider page cap",
        },
    )
    .await
}

/// One provider, by the registry name a blueprint would write.
pub(crate) async fn provider(ctx: &Context<'_>, name: String) -> Option<Provider> {
    reachable(ctx)
        .into_iter()
        .find(|provider| provider.name == name)
}

/// One provider, by the node id `provider:<name>` carries.
pub(crate) fn provider_by_id(ctx: &Context<'_>, id: &str) -> Option<Provider> {
    reachable(ctx)
        .into_iter()
        .find(|provider| provider.id == provider_id(id))
}

/// Every provider a person signs in to through a browser, with its state.
///
/// The same rows `GET /api/providers` builds, so the two surfaces cannot
/// disagree about which providers have a sign-in to offer.
fn reachable(ctx: &Context<'_>) -> Vec<Provider> {
    let state = ctx.data_unchecked::<AppState>();
    super::super::super::providers::provider_infos(state)
        .iter()
        .map(Provider::from)
        .collect()
}

/// The order the tools are read in when nothing says otherwise: by the name
/// the model calls them by, which is how a tool list is read.
fn tools_by_name() -> Vec<Term<ToolOrderField>> {
    vec![Term {
        field: ToolOrderField::Name,
        direction: OrderDirection::Asc,
    }]
}

/// What one walk of the tool directories found: the tools, and the files it
/// could not offer.
pub(crate) struct Inventory {
    /// The tools themselves.
    pub(crate) tools: Vec<Tool>,
    /// The `.rhai` files that were found and could not be offered.
    pub(crate) skipped: Vec<SkippedTool>,
}

/// Walk the tool directories once, scoped to one blueprint's own when
/// `blueprint` names one.
///
/// Shared with `BlueprintOutput.tools`, which is this listing with the scope
/// preset, because the scope changes which directory is walked rather than
/// which of the results are kept.
pub(crate) async fn discover(
    ctx: &Context<'_>,
    blueprint: Option<&str>,
) -> async_graphql::Result<Inventory> {
    let state = ctx.data_unchecked::<AppState>();
    let config = state.current_config();
    let named = blueprint.map(str::to_string);
    let dir = match blueprint {
        Some(name) => Some(super::super::super::tools::agent_dir(&config, name).gql()?),
        None => None,
    };
    // The walk over a blueprint's own directory belongs on the blocking pool.
    let found = blocking(move || {
        crate::tool_inventory::ToolInventory::discover(dir.as_deref(), named.as_deref())
    })
    .await;
    Ok(Inventory {
        tools: found.tools.into_iter().map(Tool::of).collect(),
        skipped: found
            .skipped
            .into_iter()
            .map(|skipped| SkippedTool {
                path: skipped.path.display().to_string(),
                reason: skipped.reason,
            })
            .collect(),
    })
}

/// One page of the tools one scope can offer, with the skipped files beside
/// them.
pub(crate) async fn tool_page(
    ctx: &Context<'_>,
    blueprint: Option<&str>,
    filter: Option<ToolFilter>,
    order_by: Option<Vec<ToolOrder>>,
    first: i32,
    after: Option<Cursor>,
) -> async_graphql::Result<Connection<Tool, ToolSkips>> {
    let inventory = discover(ctx, blueprint).await?;
    let skipped = inventory.skipped;
    let page = connection(
        inventory.tools,
        filter,
        terms(order_by, ToolOrder::term, tools_by_name),
        |tool: &Tool| tool.tool_name().to_string(),
        Window {
            first,
            after,
            cap: PAGE_CAP,
            cap_name: "the tool page cap",
        },
    )
    .await?;
    Ok(page.with_extras(ToolSkips { skipped }))
}

/// The tools a run on this machine can call.
pub(crate) async fn tools(
    ctx: &Context<'_>,
    filter: Option<ToolFilter>,
    order_by: Option<Vec<ToolOrder>>,
    first: i32,
    after: Option<Cursor>,
) -> async_graphql::Result<Connection<Tool, ToolSkips>> {
    tool_page(ctx, None, filter, order_by, first, after).await
}

/// The group tokens an `available_tools` list may name in place of tool names.
///
/// A bare list rather than a connection: this build compiles the set in, so it
/// is bounded by the code rather than by anything on the machine.
pub(crate) async fn tool_groups() -> Vec<ToolGroup> {
    leviath_core::blueprint::ToolGroup::ALL
        .iter()
        .map(|group| ToolGroup {
            name: group.token().to_string(),
            description: group.describe().to_string(),
        })
        .collect()
}
