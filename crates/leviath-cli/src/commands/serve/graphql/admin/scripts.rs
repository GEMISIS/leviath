//! The `putScript` and `deleteScript` fields: writing and removing a Rhai
//! script this machine registers.

use async_graphql::Context;

use super::super::super::types::AppState;
use super::super::error::IntoGraphql;

/// What writing a script did.
#[derive(Debug, async_graphql::SimpleObject)]
pub(crate) struct ScriptWritten {
    /// Where it was written.
    pub(crate) path: String,
    /// Whether it compiles. A script that does not is still written: an editor
    /// saves work in progress, and the run is what refuses to use it.
    pub(crate) compiles: bool,
    /// Why it does not compile, when it does not.
    pub(crate) error: Option<String>,
}

/// Write a Rhai script.
///
/// Remote code execution by construction, like adding an MCP server: what is
/// written here is what a run then executes. The answer says whether it
/// compiles, so an editor does not have to save and wait for a run to fail.
pub(crate) async fn put_script(
    ctx: &Context<'_>,
    kind: String,
    name: String,
    content: String,
    blueprint: Option<String>,
) -> async_graphql::Result<ScriptWritten> {
    let state = ctx.data_unchecked::<AppState>();
    let written = super::super::super::scripts::write_one(
        &state.current_config(),
        &kind,
        &name,
        blueprint.as_deref(),
        &content,
    )
    .gql()?;
    Ok(ScriptWritten {
        path: written.path,
        compiles: written.compiles,
        error: written.error,
    })
}

/// Remove a script.
pub(crate) async fn delete_script(
    ctx: &Context<'_>,
    kind: String,
    name: String,
    blueprint: Option<String>,
) -> async_graphql::Result<bool> {
    let state = ctx.data_unchecked::<AppState>();
    super::super::super::scripts::remove_one(
        &state.current_config(),
        &kind,
        &name,
        blueprint.as_deref(),
    )
    .gql()?;
    Ok(true)
}
