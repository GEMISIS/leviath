//! `POST /api/tools/bundled/{name}` (admin only): install a tool the bundled
//! agents ship as a global tool, which every agent and every inline graph can
//! then be granted.
//!
//! `web_search` and `web_fetch` ship inside the research agents. Offering
//! them to every agent means a copy in `~/.leviath/tools/`, and the only way to
//! make one over the API was to read the source out of an installed agent and
//! write it back as a script. That fails when no such agent is installed, and
//! the copy then drifts from this build's own. This writes this build's copy,
//! through the same write path `PUT /api/scripts/tool/{name}` takes, so the
//! containment checks and the compile report are the ones a script write gets.
//!
//! Gated like that write, because it is one: a global tool is code every agent
//! may run.

use axum::extract::{Path as AxumPath, State};
use axum::response::Json;
use serde::{Deserialize, Serialize};

use super::core::error::ServeError;
use super::scripts::{ScriptWritten, read_one, write_one};
use super::tools::AgentDirs;
use super::types::{ApiError, AppState};

/// What installing did to the global copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, async_graphql::Enum)]
#[serde(rename_all = "snake_case")]
#[graphql(name = "BundledToolOutcome")]
pub(crate) enum InstallOutcome {
    /// There was no global tool by that name, and now there is.
    Installed,
    /// The global copy was already this build's, byte for byte.
    Unchanged,
    /// A different global copy was there, an older build's or an edited one,
    /// and this build's replaced it.
    Replaced,
}

/// The body of `POST /api/tools/bundled/{name}`.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct BundledToolInstalled {
    /// The tool that was installed.
    pub(super) name: String,
    /// Where it landed and whether it compiles, as a script write reports.
    #[serde(flatten)]
    pub(super) written: ScriptWritten,
    /// What installing did to the global copy.
    pub(super) outcome: InstallOutcome,
}

/// The miss for a name this build ships no tool under, naming the ones it
/// does so the caller can correct a typo without a second request.
fn unknown_tool(name: &str) -> ServeError {
    let shipped: Vec<&str> = crate::bundled::bundled_tools()
        .iter()
        .map(|tool| tool.name)
        .collect();
    ServeError::NotFound(format!(
        "No bundled tool '{name}'. This build ships: {}",
        shipped.join(", ")
    ))
}

/// The global copy of a bundled tool, if one can be read.
///
/// A file that cannot be read counts as absent. An install then goes through
/// the write path's containment checks, which refuse it with the reason.
fn global_copy(dirs: &AgentDirs, name: &str) -> Option<super::scripts::ScriptSource> {
    read_one(dirs, "tool", name, None).ok()
}

/// Install one bundled tool globally, for whichever surface asked.
///
/// A global copy that already matches is left alone and answered from what
/// was read, so a run loading the tool at that moment never races a rewrite
/// of identical bytes. Anything else goes through the script write path.
pub(super) fn install_one(
    dirs: &AgentDirs,
    name: &str,
) -> Result<BundledToolInstalled, ServeError> {
    let tool = crate::bundled::bundled_tool(name).ok_or_else(|| unknown_tool(name))?;
    let (written, outcome) = match global_copy(dirs, tool.name) {
        Some(script) if script.content == tool.source => (
            // A global tool is named by no row and no manifest, so it has no
            // relative spelling, the same as a write reports for one.
            ScriptWritten {
                path: script.path,
                compiles: script.compiles,
                error: script.error,
                relative_path: None,
            },
            InstallOutcome::Unchanged,
        ),
        before => {
            let outcome = match before {
                None => InstallOutcome::Installed,
                Some(_) => InstallOutcome::Replaced,
            };
            (
                write_one(dirs, "tool", tool.name, None, tool.source)?,
                outcome,
            )
        }
    };
    Ok(BundledToolInstalled {
        name: tool.name.to_string(),
        written,
        outcome,
    })
}

/// One tool this build can install globally, and where the machine stands
/// with it.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct BundledToolRow {
    /// The name a stage grants it by, and the `{name}` the install takes.
    pub(super) name: String,
    /// What the tool says it does, from its `@description`.
    pub(super) description: String,
    /// Whether a global tool by this name is on the machine.
    pub(super) installed: bool,
    /// Whether that global copy is this build's, byte for byte. `false` when
    /// it is missing, older, or edited, which is when an install changes it.
    pub(super) current: bool,
}

/// The body of `GET /api/tools/bundled`.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct BundledToolsResp {
    /// Every tool this build can install, sorted by name.
    pub(super) tools: Vec<BundledToolRow>,
}

/// Every tool this build can install globally, for whichever surface asked.
pub(super) fn list(dirs: &AgentDirs) -> Vec<BundledToolRow> {
    crate::bundled::bundled_tools()
        .into_iter()
        .map(|tool| {
            let on_disk = global_copy(dirs, tool.name).map(|script| script.content);
            BundledToolRow {
                name: tool.name.to_string(),
                description: leviath_scripting::tool::check_source(tool.name, tool.source)
                    .map(|meta| meta.description)
                    .unwrap_or_default(),
                installed: on_disk.is_some(),
                current: on_disk.as_deref() == Some(tool.source),
            }
        })
        .collect()
}

/// `GET /api/tools/bundled`: the tools `POST /api/tools/bundled/{name}` can
/// install, and whether each is installed and current.
///
/// Always mounted. Reading what is installable writes nothing, and it is how
/// a console without admin shows what an operator could turn on.
pub(super) async fn list_bundled_tools(State(state): State<AppState>) -> Json<BundledToolsResp> {
    let dirs = AgentDirs::of(&*state.current_config().await);
    let tools = super::blocking::blocking(move || list(&dirs)).await;
    Json(BundledToolsResp { tools })
}

/// `POST /api/tools/bundled/{name}` (admin only): install it.
///
/// Mounted only behind the admin gate, like the script write it is.
pub(super) async fn install_bundled_tool(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<BundledToolInstalled>, ApiError> {
    let dirs = AgentDirs::of(&*state.current_config().await);
    super::blocking::blocking(move || install_one(&dirs, &name))
        .await
        .map(Json)
        .map_err(|e| super::core::error::as_api_error(&e))
}

#[cfg(test)]
#[path = "bundled_tools_tests.rs"]
mod tests;
