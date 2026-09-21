//! The `createBlueprint`, `updateBlueprint` and `deleteBlueprint` fields:
//! installing, replacing and removing a blueprint on this machine.

use async_graphql::Context;

use super::super::super::core::blueprints as blueprint_core;
use super::super::super::types::AppState;
use super::super::error::IntoGraphql;
use super::super::types::blueprint::Blueprint;

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
/// blueprint is what `updateBlueprint` is for, and doing it silently here
/// is how a blueprint disappears without anybody asking for it.
pub(crate) async fn create_blueprint(
    ctx: &Context<'_>,
    name: String,
    manifest: String,
) -> async_graphql::Result<Blueprint> {
    installed(ctx, &name, manifest, false)
}

/// Replace an installed blueprint.
///
/// The name is the key and does not change. Runs already spawned keep their
/// own snapshot of what they executed, so this never rewrites history.
pub(crate) async fn update_blueprint(
    ctx: &Context<'_>,
    name: String,
    manifest: String,
) -> async_graphql::Result<Blueprint> {
    installed(ctx, &name, manifest, true)
}

/// Uninstall a blueprint.
///
/// Runs that used it keep their own copy of the manifest, so their history
/// is unaffected: `run.blueprint` still answers.
pub(crate) async fn delete_blueprint(name: String) -> async_graphql::Result<bool> {
    blueprint_core::remove_blueprint(&name).gql()?;
    Ok(true)
}
