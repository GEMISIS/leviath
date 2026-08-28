//! What the terminal UIs remember about the choices you have already made.
//!
//! A choice is not a view. Folding four finished fan-outs away, cycling the run
//! list to sort by activity, telling setup you do not want a colleague's MCP
//! server: each is a person saying what they want, and asking again next time
//! is the feature failing to be one. One small JSON file under the data dir
//! holds all of it, per home, so every surface behaves the same way rather than
//! each inventing its own answer.
//!
//! Deliberately *not* `config.toml`. That file is the user's to hand-edit and
//! is reloaded on change; this is residue the UI writes behind them, and mixing
//! the two would mean a fold rewriting a hand-edited config.
//!
//! ## What belongs here, and what does not
//!
//! A remembered choice must be one the user would be annoyed to repeat, and one
//! whose staleness cannot hurt them. So:
//!
//! * **Yes**: folds, sort order, the agent you launch most, the imports you
//!   have already declined.
//! * **No**: anything transient (a filter, a search, a selection of runs to
//!   kill), because reopening the app with yesterday's filter silently applied
//!   is a bug, not a convenience.
//! * **No**: anything consequential that is safer off. `--yolo` on the new-run
//!   screen is deliberately off every time; a setting that runs tools without
//!   asking, restored out of sight, is exactly the one nobody should inherit
//!   from last week.
//!
//! Every field defaults, so a file written by another build still reads, and a
//! missing or corrupt one is simply "no memory" - losing a convenience must
//! never be louder than the thing it was helping with.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Everything the terminal UIs remember, in one file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiState {
    /// `lev dash`.
    #[serde(default)]
    pub dashboard: DashboardUi,
    /// `lev setup`.
    #[serde(default)]
    pub setup: SetupUi,
}

/// What the dashboard remembers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DashboardUi {
    /// Run ids whose sub-agents are folded away in the run list. A set, so the
    /// file is stable rather than reordering itself on every save.
    #[serde(default)]
    pub collapsed_runs: BTreeSet<String>,
    /// The run list's sort order, by its short label (`started`, `activity`,
    /// `status`). Stored as the label rather than an index so reordering the
    /// modes cannot silently change what a saved file means.
    #[serde(default)]
    pub sort_mode: Option<String>,
    /// The agent the new-run screen should open on: the last one actually
    /// launched. Someone who runs `deep-researcher` all day should not scroll
    /// to it every time.
    #[serde(default)]
    pub last_agent: Option<String>,
    /// Which view the long-form editors open in: `true` for the rendered
    /// preview, `false` for the markdown you type. One preference for all of
    /// them, because it is a preference about reading, not about one box.
    #[serde(default)]
    pub markdown_preview: bool,
    /// How the Context view was left, per run.
    ///
    /// Per run rather than per region name: folding `conversation` on one run
    /// says nothing about another, and an entry index certainly does not - the
    /// third entry of one run's `system` is not the third of another's.
    /// Pruned with [`DashboardUi::collapsed_runs`] when a run is deleted, which
    /// is what stops this growing for every run ever opened.
    #[serde(default)]
    pub context: BTreeMap<String, ContextUi>,
}

/// How one run's Context tree was left.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextUi {
    /// Regions whose entry list is folded away.
    #[serde(default)]
    pub collapsed_regions: BTreeSet<String>,
    /// `(region, entry index)` pairs opened to their full content.
    ///
    /// An index, because that is what the view addresses an entry by. It can
    /// drift on a *live* run whose region evicts from the front, which is
    /// already true within a single session and is why this is a convenience
    /// rather than a promise.
    #[serde(default)]
    pub expanded_entries: BTreeSet<(String, usize)>,
}

/// What `lev setup` remembers.
///
/// Only *declines*. An acceptance needs no memory: importing a server puts it
/// in the config, installing a blueprint puts it on disk, and the next run sees
/// that and offers accordingly. A decline leaves no trace anywhere else, which
/// is exactly why it has to leave one here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupUi {
    /// MCP servers offered by the import step and left unchecked, as
    /// `<source>:<name>`. Still listed next time, just not pre-selected.
    #[serde(default)]
    pub declined_mcp: BTreeSet<String>,
    /// Bundled blueprints offered and left unchecked, agent name to the
    /// version that was declined.
    ///
    /// Keyed by version on purpose: "no thanks" is about the thing that was
    /// offered, not about the blueprint forever. A newer bundled version is a
    /// different offer and gets asked again, which is also what keeps a stale
    /// decline from hiding an upgrade.
    #[serde(default)]
    pub declined_agents: BTreeMap<String, String>,
}

/// How a declined MCP import is named in [`SetupUi::declined_mcp`].
///
/// One function so the write and the read cannot spell it differently - a
/// decline recorded under one key and looked up under another is a memory that
/// silently does nothing, and nothing about the UI would show it.
pub fn mcp_decline_key(source: &str, name: &str) -> String {
    format!("{source}:{name}")
}

/// The file under the data directory: `ui-state.json`.
///
/// `None` when there is no resolvable data dir, which is also what tests get:
/// nothing then reads or writes, so no test can touch the real one.
pub fn default_path() -> Option<PathBuf> {
    leviath_core::paths::data_dir().map(|d| d.join("ui-state.json"))
}

/// Read the state at `path`. A missing, unreadable or unparseable file is an
/// empty state.
pub fn load(path: &Path) -> UiState {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Write `state` to `path`, creating its directory.
///
/// Best effort: a UI that cannot write its memory still works, and a toast
/// about it would be noise in the middle of whatever the person was doing.
pub fn save(path: &Path, state: &UiState) {
    // A relative file name has an empty parent, which `create_dir_all` treats
    // as already there.
    let _ = std::fs::create_dir_all(path.parent().unwrap_or(Path::new("")));
    let text = serde_json::to_string_pretty(state).expect("plain strings and sets serialize");
    let _ = leviath_sys::write_atomic(path, text.as_bytes(), None);
}

/// Read, modify, write - so one surface's save cannot drop another's memory.
///
/// The dashboard and setup share a file and never run at once, but they do run
/// in either order, and a whole-struct write built from a stale read would let
/// the second one silently forget what the first recorded.
pub fn update(path: &Path, edit: impl FnOnce(&mut UiState)) {
    let mut state = load(path);
    edit(&mut state);
    save(path, &state);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_round_trip_keeps_every_section() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("ui-state.json");
        let mut state = UiState::default();
        state.dashboard.collapsed_runs.insert("run-1".to_string());
        state.dashboard.sort_mode = Some("activity".to_string());
        state.dashboard.last_agent = Some("deep-researcher".to_string());
        state
            .setup
            .declined_mcp
            .insert("claude-code:github".to_string());
        state
            .setup
            .declined_agents
            .insert("researcher".to_string(), "1.2.0".to_string());

        save(&path, &state);
        assert!(path.exists(), "the store made its own directory");
        assert_eq!(load(&path), state);
    }

    /// Nothing written yet, a truncated file, a file holding something else:
    /// all of them are "no memory", never an error.
    #[test]
    fn an_unreadable_store_is_an_empty_one() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            load(&dir.path().join("never-written.json")),
            UiState::default()
        );

        let junk = dir.path().join("junk.json");
        std::fs::write(&junk, "not json at all").unwrap();
        assert_eq!(load(&junk), UiState::default());

        // A file from a build that knew fewer sections still reads.
        let older = dir.path().join("older.json");
        std::fs::write(&older, r#"{"dashboard":{"collapsed_runs":["a"]}}"#).unwrap();
        let got = load(&older);
        assert!(got.dashboard.collapsed_runs.contains("a"));
        assert_eq!(got.setup, SetupUi::default());
    }

    /// The reason `update` exists: setup writing its declines must not erase
    /// the folds the dashboard put in the same file.
    #[test]
    fn update_leaves_the_other_surface_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ui-state.json");

        update(&path, |s| {
            s.dashboard.collapsed_runs.insert("run-1".to_string());
        });
        update(&path, |s| {
            s.setup.declined_mcp.insert("cursor:linear".to_string());
        });

        let got = load(&path);
        assert!(got.dashboard.collapsed_runs.contains("run-1"), "kept");
        assert!(got.setup.declined_mcp.contains("cursor:linear"), "added");
    }

    #[test]
    fn the_default_path_is_under_the_data_dir() {
        let path = default_path().expect("this machine has a data dir");
        assert!(path.ends_with("ui-state.json"), "{path:?}");
    }
}
