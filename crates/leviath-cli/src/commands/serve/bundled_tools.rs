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

/// Install one bundled tool globally, for whichever surface asked.
///
/// Always written, even when the copy on disk already matches, so the answer
/// is the write path's own report rather than a second, partial one.
pub(super) fn install_one(
    dirs: &AgentDirs,
    name: &str,
) -> Result<BundledToolInstalled, ServeError> {
    let tool = crate::bundled::bundled_tool(name).ok_or_else(|| unknown_tool(name))?;
    // A file that cannot be read is treated as absent here. The write below
    // goes through the same containment checks and refuses it with the reason.
    let before = read_one(dirs, "tool", tool.name, None)
        .ok()
        .map(|script| script.content);
    let written = write_one(dirs, "tool", tool.name, None, tool.source)?;
    let outcome = match before.as_deref() {
        None => InstallOutcome::Installed,
        Some(text) if text == tool.source => InstallOutcome::Unchanged,
        Some(_) => InstallOutcome::Replaced,
    };
    Ok(BundledToolInstalled {
        name: tool.name.to_string(),
        written,
        outcome,
    })
}

/// `POST /api/tools/bundled/{name}` (admin only): install it.
///
/// Mounted only under `--allow-admin`, like the script write it is.
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
