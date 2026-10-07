//! The agents a builder can open, and where a built one goes.
//!
//! The same catalog `lev list` and the dashboard's new-run picker show
//! (installed under the agents directory, configured in `agent_paths`, the
//! local `agent.toml`, and the bundled ones not installed yet), with the
//! file's text alongside so an editor can open it without a second read.

use std::path::{Path, PathBuf};

use leviath_blueprint::{BlueprintFile, FILE_NAME};

use crate::bundled::{
    AgentAction, BUNDLED_AGENTS, BundledAgent, install_bundled, plan_agent_actions,
};
use crate::commands::list::{ListFilter, build_list_report};
use crate::config::Config;

/// Where an agent lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    /// Under the agents directory (`~/.leviath/agents/<name>`).
    Installed,
    /// Under a directory named in the config's `agent_paths`.
    Configured,
    /// The `agent.toml` of the working directory.
    Local,
    /// Embedded in this binary and not installed; editing it installs it.
    Bundled,
}

impl Source {
    /// The word the catalog shows.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Source::Installed => "installed",
            Source::Configured => "configured",
            Source::Local => "local",
            Source::Bundled => "bundled",
        }
    }
}

/// One agent in the catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CatalogEntry {
    /// `[blueprint] name`.
    pub name: String,
    /// `[blueprint] version`.
    pub version: String,
    /// `[blueprint] description`.
    pub description: String,
    /// Where it lives.
    pub source: Source,
    /// Its directory on disk; `None` for a bundled agent not installed.
    pub dir: Option<PathBuf>,
    /// The `agent.toml` text, when it could be read (always, for a bundled
    /// one).
    pub manifest: Option<String>,
    /// Its stage names, in the graph's order.
    pub stages: Vec<String>,
    /// This binary bundles an agent of the same name.
    pub bundled: bool,
    /// An installed bundled agent whose files differ from the embedded copy:
    /// edited here, or from another version. Reset puts the embedded copy
    /// back.
    pub differs_from_bundled: bool,
}

impl CatalogEntry {
    /// Whether the entry can be deleted from here: only what lives under the
    /// agents directory. Configured and local agents are edited in place but
    /// belong to wherever they are.
    pub(crate) fn deletable(&self) -> bool {
        self.source == Source::Installed
    }
}

/// Every agent the builder can open, name-sorted.
pub(crate) fn discover(agents_dir: &Path, cwd: &Path, config: &Config) -> Vec<CatalogEntry> {
    let report = build_list_report(agents_dir, cwd, config, ListFilter::All);
    let plan = plan_agent_actions(agents_dir);
    let mut entries: Vec<CatalogEntry> = report
        .agents
        .iter()
        .map(|a| {
            let path = PathBuf::from(&a.path);
            let (dir, manifest_path) = if path.is_dir() {
                (path.clone(), path.join(FILE_NAME))
            } else {
                (
                    path.parent().map(Path::to_path_buf).unwrap_or_default(),
                    path.clone(),
                )
            };
            let manifest = std::fs::read_to_string(&manifest_path).ok();
            let stages = manifest.as_deref().map(stage_names).unwrap_or_default();
            let source = match a.source {
                "installed" => Source::Installed,
                "configured" => Source::Configured,
                _ => Source::Local,
            };
            let bundled = plan.iter().find(|(b, _)| b.name == a.info.name);
            CatalogEntry {
                name: a.info.name.clone(),
                version: a.info.version.clone(),
                description: a.info.description.clone(),
                source,
                dir: Some(dir),
                manifest,
                stages,
                bundled: bundled.is_some(),
                differs_from_bundled: source == Source::Installed
                    && bundled.is_some_and(|(_, action)| !matches!(action, AgentAction::UpToDate)),
            }
        })
        .collect();
    for (agent, action) in &plan {
        if *action == AgentAction::Install && !entries.iter().any(|e| e.name == agent.name) {
            let manifest = bundled_manifest(agent);
            entries.push(CatalogEntry {
                name: agent.name.to_string(),
                version: agent.version.to_string(),
                description: BlueprintFile::parse(manifest)
                    .ok()
                    .and_then(|file| file.blueprint.description)
                    .unwrap_or_default(),
                source: Source::Bundled,
                dir: None,
                manifest: Some(manifest.to_string()),
                stages: stage_names(manifest),
                bundled: true,
                differs_from_bundled: false,
            });
        }
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

/// The stage names of an `agent.toml`, in the graph's order; none when it
/// does not read.
fn stage_names(text: &str) -> Vec<String> {
    BlueprintFile::parse(text)
        .map(|file| {
            file.graph
                .stages
                .iter()
                .map(|s| s.name.to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// The bundled agent of that name, when there is one.
pub(crate) fn bundled(name: &str) -> Option<&'static BundledAgent> {
    BUNDLED_AGENTS.iter().find(|a| a.name == name)
}

/// The embedded `agent.toml` of a bundled agent.
pub(crate) fn bundled_manifest(agent: &BundledAgent) -> &'static str {
    agent
        .files
        .iter()
        .find(|(rel, _)| *rel == FILE_NAME)
        .map(|(_, text)| *text)
        .expect("every bundled agent has a manifest")
}

/// Write an agent's `agent.toml` under the agents directory, creating its
/// directory. Returns the directory.
#[cfg(test)]
pub(crate) fn write_agent(
    agents_dir: &Path,
    name: &str,
    manifest: &str,
) -> std::io::Result<PathBuf> {
    let dir = agents_dir.join(name);
    std::fs::create_dir_all(&dir)?;
    leviath_sys::write_atomic(&dir.join(FILE_NAME), manifest.as_bytes(), None)?;
    Ok(dir)
}

/// Copy the files of a bundled agent other than its `agent.toml` (its `tools/`
/// scripts) into an agent's directory, for an agent cloned from it.
pub(crate) fn copy_bundled_extras(
    agents_dir: &Path,
    name: &str,
    from: &BundledAgent,
) -> std::io::Result<()> {
    let dir = agents_dir.join(name);
    for (rel, contents) in from.files.iter().filter(|(rel, _)| *rel != FILE_NAME) {
        let path = dir.join(rel);
        // A joined path always has a parent.
        std::fs::create_dir_all(path.parent().unwrap_or(&dir))?;
        leviath_sys::write_atomic(&path, contents.as_bytes(), None)?;
    }
    Ok(())
}

/// Rename an installed agent: its directory under the agents directory, and
/// the `name` in its `agent.toml` (comments and all else kept). Refuses a
/// name that will not do or is taken; an unreadable file is left as it is.
pub(crate) fn rename_agent(agents_dir: &Path, from: &str, to: &str) -> Result<PathBuf, String> {
    rename_agent_with(agents_dir, from, to, &mut |a, b| std::fs::rename(a, b))
}

/// [`rename_agent`] with the directory move injected, so a move the disk
/// refuses can be exercised without a read-only filesystem.
pub(crate) fn rename_agent_with(
    agents_dir: &Path,
    from: &str,
    to: &str,
    move_dir: &mut dyn FnMut(&Path, &Path) -> std::io::Result<()>,
) -> Result<PathBuf, String> {
    if !super::is_valid_name(to) {
        return Err("Letters, digits, `.`, `_` and `-` only.".to_string());
    }
    let old = agents_dir.join(from);
    let new = agents_dir.join(to);
    if to == from {
        return Ok(old);
    }
    if new.exists() {
        return Err(format!("An agent named {to} already exists."));
    }
    let manifest_path = old.join(FILE_NAME);
    let text = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("Could not read {}: {e}", manifest_path.display()))?;
    let mut doc = super::ManifestDoc::parse(&text).map_err(|e| e.to_string())?;
    doc.set_agent_name(to)
        .expect("the name passed the same check set_agent_name makes");
    // The file first, in place: if the directory cannot move the agent
    // is still whole, only under its old name with the new one inside,
    // which the next open shows and the next save writes.
    leviath_sys::write_atomic(&manifest_path, doc.to_toml().as_bytes(), None)
        .map_err(|e| format!("Could not write {}: {e}", manifest_path.display()))?;
    move_dir(&old, &new)
        .map_err(|e| format!("Could not move {} to {}: {e}", old.display(), new.display()))?;
    Ok(new)
}

/// Delete an installed agent's directory.
pub(crate) fn delete_agent(agents_dir: &Path, name: &str) -> std::io::Result<()> {
    std::fs::remove_dir_all(agents_dir.join(name))
}

/// Put the embedded copy of a bundled agent back.
pub(crate) fn reset_bundled(agents_dir: &Path, name: &str) -> Result<(), String> {
    let agent = bundled(name).ok_or_else(|| format!("{name} is not a bundled agent"))?;
    install_bundled(agent, agents_dir).map_err(|e| e.to_string())
}
