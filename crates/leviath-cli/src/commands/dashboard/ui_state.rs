//! The dashboard's half of the shared UI memory (see [`crate::ui_state`]).
//!
//! Three things are remembered, and they are the three a person would be
//! annoyed to redo: which runs' sub-agents are folded away, how the run list is
//! sorted, and which agent the new-run screen should open on. Everything else
//! the dashboard holds is either transient (a filter, a search, the marks) or
//! deliberately reset (`--yolo`), and reads there rather than here.

use super::state::Dashboard;
use super::types::SortMode;
use crate::ui_state;

impl Dashboard {
    /// Restore the remembered view state, if this dashboard has a file.
    ///
    /// Called once at startup, before the first sync. Folds name runs by id, so
    /// they can be restored before the runs themselves are known; a fold for a
    /// run that no longer exists is dropped by the first sync that can see the
    /// run list.
    pub(super) fn load_ui_state(&mut self) {
        let Some(path) = &self.ui_state_path else {
            return;
        };
        let saved = ui_state::load(path).dashboard;
        self.collapsed_runs = saved.collapsed_runs.into_iter().collect();
        // An unknown label (a file from a build with different modes, or one
        // somebody edited) leaves the default rather than failing to start.
        if let Some(mode) = saved.sort_mode.as_deref().and_then(SortMode::from_label) {
            self.sort_mode = mode;
        }
        self.last_launched_agent = saved.last_agent;
    }

    /// Write the remembered view state, keeping whatever `lev setup` put in the
    /// same file.
    pub(super) fn save_ui_state(&self) {
        let Some(path) = &self.ui_state_path else {
            return;
        };
        ui_state::update(path, |state| {
            state.dashboard.collapsed_runs = self.collapsed_runs.iter().cloned().collect();
            state.dashboard.sort_mode = Some(self.sort_mode.label().to_string());
            state.dashboard.last_agent = self.last_launched_agent.clone();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::dashboard::test_support::make_test_dashboard;

    /// The round trip that makes the feature a feature: choose, quit, come
    /// back, and the choices are still there.
    #[test]
    fn folds_sort_and_the_last_agent_all_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dash").join("ui-state.json");

        let mut first = make_test_dashboard();
        first.ui_state_path = Some(path.clone());
        first.collapsed_runs.insert("run-parent".to_string());
        first.sort_mode = SortMode::StatusGrouped;
        first.last_launched_agent = Some("deep-researcher".to_string());
        first.save_ui_state();

        let mut second = make_test_dashboard();
        second.ui_state_path = Some(path);
        second.load_ui_state();
        assert!(second.collapsed_runs.contains("run-parent"));
        assert_eq!(second.sort_mode, SortMode::StatusGrouped);
        assert_eq!(
            second.last_launched_agent.as_deref(),
            Some("deep-researcher")
        );
    }

    /// A label this build does not know leaves the default sort rather than
    /// refusing to start.
    #[test]
    fn an_unknown_sort_label_falls_back_to_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ui-state.json");
        std::fs::write(&path, r#"{"dashboard":{"sort_mode":"by-vibes"}}"#).unwrap();

        let mut dash = make_test_dashboard();
        let default = dash.sort_mode;
        dash.ui_state_path = Some(path);
        dash.load_ui_state();
        assert_eq!(dash.sort_mode, default);
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

    /// Saving the dashboard's memory must not erase setup's, since they share
    /// one file.
    #[test]
    fn saving_keeps_what_setup_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ui-state.json");
        ui_state::update(&path, |s| {
            s.setup.declined_mcp.insert("cursor:linear".to_string());
        });

        let mut dash = make_test_dashboard();
        dash.ui_state_path = Some(path.clone());
        dash.collapsed_runs.insert("run-1".to_string());
        dash.save_ui_state();

        let got = ui_state::load(&path);
        assert!(got.setup.declined_mcp.contains("cursor:linear"));
        assert!(got.dashboard.collapsed_runs.contains("run-1"));
    }
}
