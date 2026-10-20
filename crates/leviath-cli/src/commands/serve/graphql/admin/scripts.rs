//! The `upsertScript`, `deleteScript` and `installBundledTool` fields:
//! writing and removing a Rhai script this machine registers, and writing one
//! this build ships.

use async_graphql::{Context, ID, InputObject, SimpleObject};

use super::super::super::blocking::blocking;
use super::super::super::bundled_tools::InstallOutcome;
use super::super::super::scripts::ScriptWritten;
use super::super::super::tools::AgentDirs;
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
    // Resolved on this task, written on the blocking pool.
    let dirs = AgentDirs::of(&*state.current_config().await);
    let (name, owner, content) = (
        reference.name.clone(),
        reference.blueprint_name.clone(),
        request.content,
    );
    let written = blocking(move || {
        super::super::super::scripts::write_one(&dirs, kind, &name, owner.as_deref(), &content)
    })
    .await
    .gql()?;
    Ok(UpsertScriptResult {
        script: written_script(kind, reference.name, reference.blueprint_name, written),
    })
}

/// The script a write produced, as the schema answers with it.
///
/// Built by the constructor a listing builds its rows with, from the same
/// shape, so the script a write answers with and the script `script(ref:)`
/// answers with cannot describe the same file differently.
fn written_script(
    kind: &str,
    name: String,
    owner: Option<String>,
    written: ScriptWritten,
) -> Script {
    Script::from_item(super::super::super::scripts::ScriptItem {
        kind: kind.to_string(),
        name,
        // The two words a listing spells a scope with, and the two
        // `ScriptScope::from_wire` reads back.
        source: match owner {
            Some(_) => "agent",
            None => "global",
        }
        .to_string(),
        agent: owner,
        path: written.path,
        relative_path: written.relative_path,
        // Written by name into the registry the kind selects, which is
        // what being declared is.
        declared: true,
        compiles: Some(written.compiles),
        error: written.error,
        provider: None,
    })
}

/// Which bundled tool to install.
#[derive(Debug, InputObject)]
pub(crate) struct InstallBundledToolRequest {
    /// The tool, by the name a stage grants it by: `web_search`, say. A name
    /// this build ships no tool under is a miss that lists the ones it does.
    pub(crate) name: String,
}

/// The global tool as it now stands.
#[derive(Debug, SimpleObject)]
pub(crate) struct InstallBundledToolResult {
    /// The global script that was written, with `compiles` on it.
    pub(crate) script: Script,
    /// What installing did to the global copy.
    pub(crate) outcome: InstallOutcome,
}

/// Install a tool the bundled agents ship as a global tool.
pub(crate) async fn install_bundled_tool(
    ctx: &Context<'_>,
    request: InstallBundledToolRequest,
) -> async_graphql::Result<InstallBundledToolResult> {
    let state = ctx.data_unchecked::<AppState>();
    // Resolved on this task, written on the blocking pool.
    let dirs = AgentDirs::of(&*state.current_config().await);
    let installed =
        blocking(move || super::super::super::bundled_tools::install_one(&dirs, &request.name))
            .await
            .gql()?;
    Ok(InstallBundledToolResult {
        script: written_script("tool", installed.name, None, installed.written),
        outcome: installed.outcome,
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
    // Resolved on this task, removed on the blocking pool.
    let dirs = AgentDirs::of(&*state.current_config().await);
    let (name, owner) = (reference.name.clone(), reference.blueprint_name.clone());
    blocking(move || {
        super::super::super::scripts::remove_one(&dirs, kind, &name, owner.as_deref())
    })
    .await
    .gql()?;
    Ok(DeleteScriptResult {
        deleted_id: super::super::node::script_id(
            kind,
            reference.blueprint_name.as_deref(),
            &reference.name,
        ),
    })
}
