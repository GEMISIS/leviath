//! What the dashboard remembers about how you left it looking.
//!
//! Which runs' sub-agents you folded away is a decision, not a view: a person
//! who folds four finished fan-outs to see the two live ones has said something
//! about what they want on screen, and having to say it again every time they
//! open the dashboard is the feature failing to be one. It is kept the same way
//! the agent editor keeps its canvas arrangements - one small JSON file under
//! the data dir, per home.
//!
//! Deliberately *not* in `config.toml`. That file is the user's to edit and is
//! reloaded on change; this is UI residue the dashboard writes behind them, and
//! mixing the two would mean a fold rewriting a hand-edited config.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::state::Dashboard;

/// The dashboard's remembered view state.
///
/// A struct rather than a bare list of ids so the next thing worth remembering
/// can join it without a second file or a format migration. Missing fields
/// default, so an older file stays readable.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct UiState {
    /// Run ids whose sub-agents are folded away in the run list. A set, so the
    /// file is stable rather than reordering itself on every save.
    #[serde(default)]
    pub(super) collapsed_runs: BTreeSet<String>,
}

/// The file under the data directory: `dash/ui-state.json`.
///
/// `None` when there is no resolvable data dir, which is also what tests get:
/// nothing then reads or writes, so no test can touch the real one.
pub(super) fn default_path() -> Option<PathBuf> {
    leviath_core::paths::data_dir().map(|d| d.join("dash").join("ui-state.json"))
}

/// Read the state at `path`. A missing, unreadable or unparseable file is an
/// empty state: this is a convenience, and losing it must never be louder than
/// the thing it was helping with.
fn read(path: &Path) -> UiState {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

impl Dashboard {
    /// Restore the remembered view state, if this dashboard has a file.
    ///
    /// Called once at startup, before the first sync. Folds name runs by id, so
    /// they can be restored before the runs themselves are known - a fold for a
    /// run that no longer exists is dropped by the first sync that can see the
    /// run list.
    pub(super) fn load_ui_state(&mut self) {
        let Some(path) = &self.ui_state_path else {
            return;
        };
        self.collapsed_runs = read(path).collapsed_runs.into_iter().collect();
    }

    /// Write the remembered view state. Best effort: a dashboard that cannot
    /// write its folds still works, and a toast about it would be noise in the
    /// middle of whatever the person was actually doing.
    pub(super) fn save_ui_state(&self) {
        let Some(path) = &self.ui_state_path else {
            return;
        };
        let state = UiState {
            collapsed_runs: self.collapsed_runs.iter().cloned().collect(),
        };
        // A relative file name has an empty parent, which `create_dir_all`
        // treats as already there.
        let _ = std::fs::create_dir_all(path.parent().unwrap_or(Path::new("")));
        let text = serde_json::to_string_pretty(&state).expect("a set of strings serializes");
        let _ = std::fs::write(path, text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::dashboard::test_support::make_test_dashboard;

    /// The round trip that makes the feature a feature: fold, quit, come back,
    /// and it is still folded.
    #[test]
    fn a_fold_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dash").join("ui-state.json");

        let mut first = make_test_dashboard();
        first.ui_state_path = Some(path.clone());
        first.collapsed_runs.insert("run-parent".to_string());
        first.save_ui_state();
        assert!(path.exists(), "the store made its own directory");

        let mut second = make_test_dashboard();
        second.ui_state_path = Some(path);
        second.load_ui_state();
        assert!(second.collapsed_runs.contains("run-parent"));
    }

    /// Nothing saved yet, a file somebody truncated, a file holding something
    /// else entirely: all of them are "no folds", never an error.
    #[test]
    fn an_unreadable_store_is_an_empty_one() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("never-written.json");
        assert_eq!(read(&missing), UiState::default());

        let junk = dir.path().join("junk.json");
        std::fs::write(&junk, "not json at all").unwrap();
        assert_eq!(read(&junk), UiState::default());

        // A file from a build that knew fewer fields still reads.
        let older = dir.path().join("older.json");
        std::fs::write(&older, "{}").unwrap();
        assert_eq!(read(&older), UiState::default());
    }

    /// Loading replaces whatever was in memory, so a stale fold from a previous
    /// load cannot survive a store that no longer names it.
    #[test]
    fn loading_replaces_rather_than_merges() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ui-state.json");
        std::fs::write(&path, r#"{"collapsed_runs":["kept"]}"#).unwrap();

        let mut dash = make_test_dashboard();
        dash.ui_state_path = Some(path);
        dash.collapsed_runs.insert("stale".to_string());
        dash.load_ui_state();
        assert!(dash.collapsed_runs.contains("kept"));
        assert!(!dash.collapsed_runs.contains("stale"));
    }

    /// A dashboard with no store (every test one, and any home without a data
    /// dir) neither reads nor writes.
    #[test]
    fn without_a_path_nothing_touches_disk() {
        let mut dash = make_test_dashboard();
        assert!(
            dash.ui_state_path.is_none(),
            "tests get no store by default"
        );
        dash.collapsed_runs.insert("run-1".to_string());
        dash.save_ui_state();
        dash.load_ui_state();
        assert!(
            dash.collapsed_runs.contains("run-1"),
            "load with no store left memory alone"
        );
    }

    /// The production path lands under the data dir, beside the editor's
    /// arrangements rather than in the user's config.
    #[test]
    fn the_default_path_is_under_the_data_dir() {
        let path = default_path().expect("this machine has a data dir");
        assert!(path.ends_with("dash/ui-state.json"), "{path:?}");
    }
}
