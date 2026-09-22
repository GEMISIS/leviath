//! `Node`: the types one id is enough to fetch.
//!
//! A client that caches by id, or that holds an id from a webhook and wants
//! the thing behind it, needs one field to ask with and one promise behind it:
//! that an id names the same thing wherever it turns up in this schema.
//!
//! Five of the nine are keyed by a name rather than by a minted id, so their
//! ids carry a tag saying which kind of thing the name belongs to. A model's
//! is the longest of those, because a model id is the provider's own and two
//! providers can both serve `gpt-5.5`: the tag carries the provider as well.

use std::sync::Arc;

use async_graphql::{Context, ID};

use super::super::blocking::blocking;
use super::super::core::blueprints;
use super::super::core::error::ServeError;
use super::super::core::runs as run_core;
use super::super::types::AppState;
use super::error::IntoGraphql;
use super::query::{RunExport, yolo_profile as profile_named};
use super::types::blueprint::{Blueprint, revision_id};
use super::types::catalog::{MODEL_TAG, Model, PROVIDER_TAG, Provider};
use super::types::machine::{McpServer, Script, YoloProfile};
use super::types::run::Run;
use super::types::update::UpdateJob;

/// The tag on an MCP server's id.
const MCP_SERVER_TAG: &str = "mcpServer";

/// The tag on a yolo profile's id.
const YOLO_PROFILE_TAG: &str = "yoloProfile";

/// The tag on a script's id.
const SCRIPT_TAG: &str = "script";

/// Anything this schema can hand back from its id alone.
///
/// The id is globally unique: no two nodes of any type share one, so a client
/// may cache by id alone and `node` needs no type from the caller.
#[derive(async_graphql::Interface)]
#[graphql(field(
    name = "id",
    ty = "async_graphql::ID",
    desc = "This node's id, unique across every type that implements `Node`."
))]
pub(crate) enum Node {
    /// One run of one blueprint.
    Run(Run),
    /// One revision of one blueprint.
    Blueprint(Blueprint),
    /// One registered script.
    Script(Script),
    /// One configured MCP server.
    McpServer(McpServer),
    /// One named yolo profile.
    YoloProfile(YoloProfile),
    /// One model this machine can route to.
    Model(Model),
    /// One provider this machine can reach.
    Provider(Provider),
    /// One update this server ran.
    UpdateJob(UpdateJob),
    /// One export of the run store this server started.
    RunExport(RunExport),
}

/// One configured MCP server's node id.
pub(crate) fn mcp_server_id(name: &str) -> ID {
    ID(format!("{MCP_SERVER_TAG}:{name}"))
}

/// One yolo profile's node id.
pub(crate) fn yolo_profile_id(name: &str) -> ID {
    ID(format!("{YOLO_PROFILE_TAG}:{name}"))
}

/// One registered script's node id.
///
/// The kind carries the blueprint it belongs to, so the name is the whole tail
/// of the id and keeps the `/` a script in a subdirectory has in it.
pub(crate) fn script_id(kind: &str, blueprint: Option<&str>, name: &str) -> ID {
    match blueprint {
        Some(blueprint) => ID(format!("{SCRIPT_TAG}:{kind}@{blueprint}:{name}")),
        None => ID(format!("{SCRIPT_TAG}:{kind}:{name}")),
    }
}

/// Fetch whatever an id names, or nothing.
///
/// An id for something deleted, expired or never minted is an absence rather
/// than a failure, and so is an id this schema has no type for. What does fail
/// is a read that could not answer the question at all, such as a config file
/// that will not parse: the field it would have come from fails the same way,
/// and a null there would read as "no such server".
pub(crate) async fn resolve(ctx: &Context<'_>, id: &str) -> async_graphql::Result<Option<Node>> {
    // A tag first, because only a tagged id carries a `:`: a run id is folded
    // to ASCII alphanumerics and `-` when it is minted, and the job ids are a
    // word, a second and a counter.
    if let Some((tag, rest)) = id.split_once(':') {
        return match tag {
            MCP_SERVER_TAG => mcp_server(ctx, rest),
            YOLO_PROFILE_TAG => Ok(yolo_profile(rest)),
            SCRIPT_TAG => script(ctx, rest),
            PROVIDER_TAG => Ok(super::query::provider_by_id(ctx, rest).map(Node::Provider)),
            MODEL_TAG => Ok(super::query::model_by_id(ctx, id).await.map(Node::Model)),
            _ => Ok(None),
        };
    }
    // A blueprint revision id is `<name>@<digest prefix>`, and nothing else
    // this schema mints carries an `@`.
    if let Some((name, _)) = id.rsplit_once('@') {
        return blueprint(ctx, id, name).await;
    }
    minted(ctx, id).await
}

/// Fetch what each of several ids names, in the order they were asked about.
///
/// One entry per id, null where that id names nothing, so a client reading a
/// page of cached keys can line the answers up against what it asked without
/// matching on ids. Resolved one after another rather than all at once: the
/// reads behind them are a file each at most.
///
/// Bounded by the same cap the run listing puts on a named list of ids, for
/// the same reason: one request is one read per id, and a list nobody capped
/// is a fan-out over the whole store.
pub(crate) async fn resolve_many(
    ctx: &Context<'_>,
    ids: Vec<ID>,
) -> async_graphql::Result<Vec<Option<Node>>> {
    if ids.len() > run_core::MAX_IDS {
        return Err(ServeError::BadRequest(format!(
            "`ids` names {} nodes; at most {} may be named at once",
            ids.len(),
            run_core::MAX_IDS
        )))
        .gql();
    }
    let mut found = Vec::with_capacity(ids.len());
    for id in ids {
        found.push(resolve(ctx, id.as_str()).await?);
    }
    Ok(found)
}

/// The MCP server one tagged id names.
fn mcp_server(ctx: &Context<'_>, name: &str) -> async_graphql::Result<Option<Node>> {
    let state = ctx.data_unchecked::<AppState>();
    // The same failure `mcpServers` answers with, rather than a null: a config
    // that will not parse is not this server having been deleted.
    let servers = super::super::mcp::server_infos(state).gql()?;
    Ok(servers
        .into_iter()
        .find(|server| server.name == name)
        .map(|server| Node::McpServer(McpServer::from_info(server))))
}

/// The yolo profile one tagged id names.
fn yolo_profile(name: &str) -> Option<Node> {
    profile_named(name).map(Node::YoloProfile)
}

/// The script one tagged id names.
///
/// `rest` is `<kind>:<name>` for a script every blueprint gets, and
/// `<kind>@<blueprint>:<name>` for one blueprint's own.
fn script(ctx: &Context<'_>, rest: &str) -> async_graphql::Result<Option<Node>> {
    let state = ctx.data_unchecked::<AppState>();
    let Some((scope, name)) = rest.split_once(':') else {
        return Ok(None);
    };
    let (kind, blueprint) = match scope.split_once('@') {
        Some((kind, blueprint)) => (kind, Some(blueprint)),
        None => (scope, None),
    };
    // A blueprint name the script routes refuse is a name no script can be
    // filed under, so an id carrying one names nothing rather than failing.
    let Ok(listed) = super::super::scripts::registered(state, blueprint) else {
        return Ok(None);
    };
    Ok(listed
        .into_iter()
        .find(|item| item.kind == kind && item.name == name && item.agent.as_deref() == blueprint)
        .map(|item| Node::Script(Script::from_item(item))))
}

/// The installed blueprint revision one id names.
///
/// A run's frozen copy carries the same id when it is the same bytes, and is
/// read through the run rather than from here, so an id whose digest is not
/// the installed revision's answers with nothing.
async fn blueprint(ctx: &Context<'_>, id: &str, name: &str) -> async_graphql::Result<Option<Node>> {
    let state = ctx.data_unchecked::<AppState>();
    let config = state.current_config();
    let roots = super::super::blueprints::blueprint_roots(&config);
    let installed = blocking(move || super::super::blueprints::discover_in(roots)).await;
    let Some(info) = installed.iter().find(|info| info.name == name) else {
        return Ok(None);
    };
    let manifest = blueprints::ManifestText::installed(info.manifest.clone());
    if revision_id(name, &manifest.digest) != ID(id.to_string()) {
        return Ok(None);
    }
    Ok(Some(Node::Blueprint(Blueprint {
        parsed: Arc::clone(&info.parsed),
        digest: manifest.digest,
        source: manifest.source.into(),
    })))
}

/// The run or job one minted id names.
///
/// The two job registries live in memory and are keyed by exactly these ids,
/// so they are asked first and the run store only for an id neither holds.
async fn minted(ctx: &Context<'_>, id: &str) -> async_graphql::Result<Option<Node>> {
    let state = ctx.data_unchecked::<AppState>();
    if let Some(job) = state.update_jobs.get(id) {
        return Ok(Some(Node::UpdateJob(UpdateJob::from(job))));
    }
    if let Some(job) = state.caches.exports.get(id) {
        return Ok(Some(Node::RunExport(RunExport::from_job(state, &job))));
    }
    let asked = id.to_string();
    let meta = blocking(move || crate::runstate::read_meta(&asked).ok()).await;
    let now = leviath_core::duration::now_secs();
    Ok(meta.map(|meta| {
        Node::Run(Run {
            meta: Arc::new(meta),
            now,
        })
    }))
}

#[cfg(test)]
#[path = "node_tests.rs"]
mod tests;
