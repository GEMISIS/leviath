//! The `upsertScript` and `deleteScript` fields: writing and removing a Rhai
//! script this machine registers.

use async_graphql::{Context, ID, InputObject, SimpleObject};

use super::super::super::types::AppState;
use super::super::error::IntoGraphql;
use super::super::script_ref::ScriptRef;
use super::super::types::machine::Script;

/// Which script to write, and what to put in it.
#[derive(Debug, InputObject)]
pub(crate) struct UpsertScriptRequest {
    /// The script to write. A kind nothing can be written under, such as a
    /// file nothing has claimed, is refused before any path is built.
    pub(crate) script: ScriptRef,
    /// The script's source.
    pub(crate) content: String,
}

/// The script as it now stands.
#[derive(Debug, SimpleObject)]
pub(crate) struct UpsertScriptResult {
    /// The script that was written, with `compiles` and `compileError` on it,
    /// so an editor does not have to save and wait for a run to fail.
    pub(crate) script: Script,
}

/// Which script to remove.
#[derive(Debug, InputObject)]
pub(crate) struct DeleteScriptRequest {
    /// The script to remove. One that is not there is a miss.
    pub(crate) script: ScriptRef,
}

/// What was removed.
#[derive(Debug, SimpleObject)]
pub(crate) struct DeleteScriptResult {
    /// The node id the script had.
    pub(crate) deleted_id: ID,
}

/// Write a Rhai script.
///
/// Remote code execution by construction, like adding an MCP server: what is
/// written here is what a run then executes. The answer says whether it
/// compiles, so an editor does not have to save and wait for a run to fail. A
/// script that does not compile is still written: an editor saves work in
/// progress, and the run is what refuses to use it.
pub(crate) async fn upsert_script(
    ctx: &Context<'_>,
    request: UpsertScriptRequest,
) -> async_graphql::Result<UpsertScriptResult> {
    let state = ctx.data_unchecked::<AppState>();
    let reference = request.script;
    let kind = reference.kind.wire();
    let written = super::super::super::scripts::write_one(
        &state.current_config(),
        kind,
        &reference.name,
        reference.blueprint_name.as_deref(),
        &request.content,
    )
    .gql()?;
    // Built by the constructor a listing builds its rows with, from the same
    // shape, so the script this answers with and the script `script(ref:)`
    // answers with cannot describe the same file differently.
    Ok(UpsertScriptResult {
        script: Script::from_item(super::super::super::scripts::ScriptItem {
            kind: kind.to_string(),
            name: reference.name,
            // The two words a listing spells a scope with, and the two
            // `ScriptScope::from_wire` reads back.
            source: match reference.blueprint_name {
                Some(_) => "agent",
                None => "global",
            }
            .to_string(),
            agent: reference.blueprint_name,
            path: written.path,
            relative_path: written.relative_path,
            // Written by name into the registry the kind selects, which is
            // what being declared is.
            declared: true,
            compiles: Some(written.compiles),
            error: written.error,
            provider: None,
        }),
    })
}

/// Remove a script.
pub(crate) async fn delete_script(
    ctx: &Context<'_>,
    request: DeleteScriptRequest,
) -> async_graphql::Result<DeleteScriptResult> {
    let state = ctx.data_unchecked::<AppState>();
    let reference = request.script;
    let kind = reference.kind.wire();
    super::super::super::scripts::remove_one(
        &state.current_config(),
        kind,
        &reference.name,
        reference.blueprint_name.as_deref(),
    )
    .gql()?;
    Ok(DeleteScriptResult {
        deleted_id: super::super::node::script_id(
            kind,
            reference.blueprint_name.as_deref(),
            &reference.name,
        ),
    })
}
