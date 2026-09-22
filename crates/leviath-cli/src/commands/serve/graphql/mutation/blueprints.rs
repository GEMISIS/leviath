//! The `createBlueprint`, `updateBlueprint` and `deleteBlueprint` fields:
//! installing, replacing and removing a blueprint on this machine.
//!
//! Replacing and removing point at what is installed with a `BlueprintRef`,
//! whose optional `digest` is an optimistic-concurrency pin: a client that read
//! a blueprint and sends its digest back is told the manifest moved rather than
//! writing over somebody else's revision.

use async_graphql::{Context, ID, InputObject, SimpleObject};

use super::super::super::core::blueprints as blueprint_core;
use super::super::super::types::AppState;
use super::super::error::IntoGraphql;
use super::super::inputs::BlueprintRef;
use super::super::types::blueprint::Blueprint;

/// A blueprint to install.
#[derive(Debug, InputObject)]
pub(crate) struct CreateBlueprintRequest {
    /// The name to install it under.
    pub(crate) name: String,
    /// The manifest text.
    pub(crate) manifest: String,
}

/// What installing a blueprint answers with.
#[derive(SimpleObject)]
pub(crate) struct CreateBlueprintResult {
    /// The blueprint as it is now installed.
    pub(crate) blueprint: Blueprint,
}

/// A blueprint to replace, and what to replace it with.
#[derive(Debug, InputObject)]
pub(crate) struct UpdateBlueprintRequest {
    /// Which installed blueprint to replace. A `digest` on it refuses the
    /// write where what is installed is a different revision, so two clients
    /// editing one blueprint cannot silently overwrite each other.
    pub(crate) blueprint: BlueprintRef,
    /// The replacement manifest text.
    pub(crate) manifest: String,
}

/// What replacing a blueprint answers with.
#[derive(SimpleObject)]
pub(crate) struct UpdateBlueprintResult {
    /// The blueprint as it now stands.
    pub(crate) blueprint: Blueprint,
}

/// A blueprint to uninstall.
#[derive(Debug, InputObject)]
pub(crate) struct DeleteBlueprintRequest {
    /// The installed blueprint to remove, with an optional digest pin.
    pub(crate) blueprint: BlueprintRef,
}

/// What uninstalling a blueprint answers with.
#[derive(Debug, SimpleObject)]
pub(crate) struct DeleteBlueprintResult {
    /// The blueprint that was removed, by the name it was installed under.
    pub(crate) deleted_id: ID,
}

/// Write a blueprint and describe what was written.
fn installed(
    ctx: &Context<'_>,
    name: &str,
    manifest: String,
    replacing: bool,
) -> async_graphql::Result<Blueprint> {
    let state = ctx.data_unchecked::<AppState>();
    let written = blueprint_core::write_blueprint(name, manifest, replacing).gql()?;
    // Into the parse cache by digest, so the listing that follows this mutation
    // does not parse the same text again. Best effort on purpose: the text was
    // parsed to write it, the blueprint is on disk either way, and a cache that
    // did not warm costs one parse rather than the request.
    let _ = state.caches.blueprints.parse(&written.manifest);
    Ok(Blueprint {
        parsed: written.parsed,
        digest: written.manifest.digest,
        source: blueprint_core::BlueprintSource::Installed.into(),
    })
}

/// Install a blueprint.
///
/// A name that is already installed is a `CONFLICT`: replacing somebody's
/// blueprint is what `updateBlueprint` is for, and doing it silently here is
/// how a blueprint disappears without anybody asking for it.
pub(crate) async fn create_blueprint(
    ctx: &Context<'_>,
    request: CreateBlueprintRequest,
) -> async_graphql::Result<CreateBlueprintResult> {
    Ok(CreateBlueprintResult {
        blueprint: installed(ctx, &request.name, request.manifest, false)?,
    })
}

/// Replace an installed blueprint.
///
/// The name is the key and does not change. Runs already spawned keep their own
/// snapshot of what they executed, so this never rewrites history.
pub(crate) async fn update_blueprint(
    ctx: &Context<'_>,
    request: UpdateBlueprintRequest,
) -> async_graphql::Result<UpdateBlueprintResult> {
    let state = ctx.data_unchecked::<AppState>();
    // The pin is checked before the write, so a stale one answers `CONFLICT`
    // with nothing written rather than after the fact.
    let name = request.blueprint.installed(state).await.gql()?;
    Ok(UpdateBlueprintResult {
        blueprint: installed(ctx, &name, request.manifest, true)?,
    })
}

/// Uninstall a blueprint.
///
/// Runs that used it keep their own copy of the manifest, so their history is
/// unaffected: `run.blueprint` still answers. A name nothing is installed under
/// is `NOT_FOUND`.
pub(crate) async fn delete_blueprint(
    ctx: &Context<'_>,
    request: DeleteBlueprintRequest,
) -> async_graphql::Result<DeleteBlueprintResult> {
    let state = ctx.data_unchecked::<AppState>();
    let name = request.blueprint.installed(state).await.gql()?;
    blueprint_core::remove_blueprint(&name).gql()?;
    Ok(DeleteBlueprintResult {
        deleted_id: ID::from(name),
    })
}
