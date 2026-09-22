//! The `upsertScript` and `deleteScript` fields: writing and removing a Rhai
//! script this machine registers.

use async_graphql::{Context, ID, InputObject, SimpleObject};

use super::super::super::types::AppState;
use super::super::error::IntoGraphql;
use super::super::script_ref::{ScriptRef, ScriptScope};
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
    Ok(UpsertScriptResult {
        script: Script {
            id: super::super::node::script_id(
                kind,
                reference.blueprint_name.as_deref(),
                &reference.name,
            ),
            kind: reference.kind,
            name: reference.name,
            scope: match reference.blueprint_name {
                Some(_) => ScriptScope::Blueprint,
                None => ScriptScope::Global,
            },
            blueprint_name: reference.blueprint_name,
            path: written.path,
            // The path a write is addressed by is the path it lands at, so
            // there is nothing to relativize that the reference did not say.
            relative_path: None,
            // Written by name into the registry the kind selects, which is
            // what being declared is.
            is_declared: true,
            compiles: Some(written.compiles),
            compile_error: written.error,
        },
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
