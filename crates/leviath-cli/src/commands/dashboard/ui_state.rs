//! The dashboard's half of the shared UI memory (see [`crate::ui_state`]).
//!
//! Five things are remembered, and they are the five a person would be annoyed
//! to redo: which runs' sub-agents are folded away, how the run list is sorted,
//! which agent the new-run screen should open on, how each run's Context tree
//! was left, and which view the long-form editors open in. Everything else
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
        self.md_preview = saved.markdown_preview;
    }

    /// Write the remembered view state, keeping whatever `lev setup` put in the
    /// same file.
    ///
    /// Also drops per-run Context state for runs that no longer exist. That
    /// tidying happens here, on a write, rather than on the sync tick: only one
    /// run's tree is in memory at a time, so noticing a stale row means reading
    /// the file, and doing that ten times a second to find nothing would be a
    /// poor trade. Every write is a user action, which is often enough.
    pub(super) fn save_ui_state(&self) {
        let Some(path) = &self.ui_state_path else {
            return;
        };
        ui_state::update(path, |state| {
            state.dashboard.collapsed_runs = self.collapsed_runs.iter().cloned().collect();
            state.dashboard.sort_mode = Some(self.sort_mode.label().to_string());
            state.dashboard.last_agent = self.last_launched_agent.clone();
            state.dashboard.markdown_preview = self.md_preview;
            // Guarded on a non-empty list for the same reason the fold prune
            // is: "no runs" is also what a runs directory that could not be
            // read for a moment looks like, and pruning against that would
            // throw away every run's Context state at once.
            if !self.agents.is_empty() {
                let agents = &self.agents;
                state
                    .dashboard
                    .context
                    .retain(|id, _| agents.iter().any(|a| a.id == *id));
            }
            if let Some(run) = self.open_run_id() {
                let tree = &self.context_tree;
                let entry = ui_state::ContextUi {
                    collapsed_regions: tree.collapsed_regions.iter().cloned().collect(),
                    expanded_entries: tree.expanded_entries.iter().cloned().collect(),
                };
                // A run back at its defaults keeps no row of its own, so the
                // file does not accumulate one per run merely visited.
                if entry == ui_state::ContextUi::default() {
                    state.dashboard.context.remove(&run);
                } else {
                    state.dashboard.context.insert(run, entry);
                }
            }
        });
    }

    /// Put the Context tree back the way this run was left.
    ///
    /// Called when a run's page opens. Nothing saved (or no store) leaves the
    /// default - regions open, entries closed - which is what a run being seen
    /// for the first time should look like.
    pub(super) fn restore_context_tree(&mut self) {
        let (Some(path), Some(run)) = (&self.ui_state_path, self.open_run_id()) else {
            return;
        };
        let Some(saved) = ui_state::load(path).dashboard.context.remove(&run) else {
            return;
        };
        self.context_tree.collapsed_regions = saved.collapsed_regions.into_iter().collect();
        self.context_tree.expanded_entries = saved.expanded_entries.into_iter().collect();
    }

    /// The run whose page is open, which is the one the Context tree belongs
    /// to.
    fn open_run_id(&self) -> Option<String> {
        self.selected_agent().map(|a| a.id.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::dashboard::test_support::make_test_dashboard;
    use crate::commands::dashboard::types::{AgentDisplayStatus, DashboardAgent};

    /// The least an agent can be and still occupy a row.
    fn agent(id: &str) -> DashboardAgent {
        DashboardAgent {
            id: id.to_string(),
            blueprint_name: "test-agent".to_string(),
            stage: "main".to_string(),
            stage_index: 0,
            num_stages: 1,
            status: AgentDisplayStatus::Active,
            tokens_in: 0,
            tokens_out: 0,
            cached_tokens: 0,
            iteration: 0,
            broken_scripts: Vec::new(),
            waiting_prompt: None,
            wait_reason: None,
            pending_request: None,
            last_answered_request_id: None,
            context_snapshot: None,
            stages: vec![],
            workdir: "/tmp".to_string(),
            task: "task".to_string(),
            title: None,
            model: None,
            parent_id: None,
            depth: 0,
            started_at: 1000,
            last_progress_at: None,
            runtime_secs: 0,
            clock_now: 0,
            graph: None,
            accepts_messages: true,
            taint_summary: vec![],
        }
    }

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

    /// One run's Context folds come back on that run, and do not leak onto
    /// another. Folding `conversation` while reading run A says nothing about
    /// run B.
    #[test]
    fn the_context_tree_is_remembered_per_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ui-state.json");
        let mut dash = make_test_dashboard();
        dash.ui_state_path = Some(path);
        dash.agents.push(agent("run-a"));
        dash.agents.push(agent("run-b"));
        dash.update_display_indices();

        // On run A: fold a region and open an entry.
        dash.selected = dash.row_of_run("run-a").unwrap();
        dash.context_tree
            .collapsed_regions
            .insert("conversation".to_string());
        dash.context_tree
            .expanded_entries
            .insert(("system".to_string(), 2));
        dash.save_ui_state();

        // Opening run B starts clean.
        dash.selected = dash.row_of_run("run-b").unwrap();
        dash.open_detail_view();
        assert!(dash.context_tree.collapsed_regions.is_empty());
        assert!(dash.context_tree.expanded_entries.is_empty());

        // Back to run A, and it is as it was left.
        dash.selected = dash.row_of_run("run-a").unwrap();
        dash.open_detail_view();
        assert!(dash.context_tree.collapsed_regions.contains("conversation"));
        assert!(
            dash.context_tree
                .expanded_entries
                .contains(&("system".to_string(), 2))
        );
    }

    /// A run that is deleted takes its Context state with it, so the file does
    /// not keep a row per run ever opened.
    #[test]
    fn context_state_for_a_vanished_run_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ui-state.json");
        let mut dash = make_test_dashboard();
        dash.ui_state_path = Some(path.clone());
        dash.agents.push(agent("run-a"));
        dash.update_display_indices();
        dash.context_tree
            .collapsed_regions
            .insert("conversation".to_string());
        dash.save_ui_state();
        assert!(
            ui_state::load(&path)
                .dashboard
                .context
                .contains_key("run-a")
        );

        // Run A is gone and another run exists, so there is a list to prune
        // against.
        dash.agents.clear();
        dash.agents.push(agent("run-b"));
        dash.update_display_indices();
        dash.context_tree = Default::default();
        dash.save_ui_state();
        assert!(ui_state::load(&path).dashboard.context.is_empty());
    }

    /// A run put back to its defaults keeps no row, so merely visiting runs
    /// does not grow the file.
    #[test]
    fn a_run_back_at_its_defaults_keeps_no_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ui-state.json");
        let mut dash = make_test_dashboard();
        dash.ui_state_path = Some(path.clone());
        dash.agents.push(agent("run-a"));
        dash.update_display_indices();

        dash.context_tree
            .collapsed_regions
            .insert("conversation".to_string());
        dash.save_ui_state();
        assert!(
            ui_state::load(&path)
                .dashboard
                .context
                .contains_key("run-a")
        );

        dash.context_tree.collapsed_regions.clear();
        dash.save_ui_state();
        assert!(ui_state::load(&path).dashboard.context.is_empty());
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
