//! Killing and deleting runs from the dashboard: the confirmations, and
//! what runs when they are answered.

use super::helpers::truncate;
use super::state::Dashboard;
use super::types::*;
use crate::runstate;
use crate::tui::widgets::markdown_edit::MarkdownEdit;

impl Dashboard {
    /// Open the kill confirmation. Acts on every marked run that is killable
    /// when any are marked, else on the selected run if it is killable.
    pub(super) fn request_kill(&mut self) {
        use crate::tui::widgets::confirm::Confirm;
        use ratatui::text::Line;
        if !self.marked.is_empty() {
            // Marked but already-finished runs are skipped, the same way `x`
            // on a finished run does nothing.
            let run_ids: Vec<String> = self
                .agents
                .iter()
                .filter(|a| self.marked.contains(&a.id) && a.status.is_killable())
                .map(|a| a.id.clone())
                .collect();
            if run_ids.is_empty() {
                return;
            }
            let body = if run_ids.len() == 1 {
                "Cancel 1 run? Its state stays on disk.".to_string()
            } else {
                format!("Cancel {} runs? Their state stays on disk.", run_ids.len())
            };
            let dialog =
                Confirm::new("Kill runs?", vec![Line::from(body)], "Kill", "Cancel").danger();
            self.pending_confirm = Some((ConfirmAction::Kill { run_ids }, dialog));
            return;
        }
        let Some(agent) = self.selected_agent() else {
            return;
        };
        if !agent.status.is_killable() {
            return;
        }
        let run_id = agent.id.clone();
        let name = agent
            .title
            .clone()
            .unwrap_or_else(|| truncate(&agent.blueprint_name, 24));
        let dialog = Confirm::new(
            "Kill run?",
            vec![Line::from(format!(
                "Cancel '{name}' ({})? Its state stays on disk.",
                truncate(&run_id, 20)
            ))],
            "Kill",
            "Cancel",
        )
        .danger();
        self.pending_confirm = Some((
            ConfirmAction::Kill {
                run_ids: vec![run_id],
            },
            dialog,
        ));
    }

    /// Open the delete confirmation. Acts on every marked run when any are
    /// marked, else on the selected run.
    pub(super) fn request_delete(&mut self) {
        use crate::tui::widgets::confirm::Confirm;
        use ratatui::text::Line;
        if !self.marked.is_empty() {
            // Every marked id names a live run: `update_display_indices` prunes
            // marks whenever the agent list changes, so no emptiness check is
            // needed here.
            let run_ids: Vec<String> = self
                .agents
                .iter()
                .filter(|a| self.marked.contains(&a.id))
                .map(|a| a.id.clone())
                .collect();
            let body = if run_ids.len() == 1 {
                "Delete 1 run and its on-disk state? This is permanent.".to_string()
            } else {
                format!(
                    "Delete {} runs and their on-disk state? This is permanent.",
                    run_ids.len()
                )
            };
            let dialog =
                Confirm::new("Delete runs?", vec![Line::from(body)], "Delete", "Cancel").danger();
            self.pending_confirm = Some((ConfirmAction::Delete { run_ids }, dialog));
            return;
        }
        let Some(agent) = self.selected_agent() else {
            return;
        };
        let run_id = agent.id.clone();
        let dialog = Confirm::new(
            "Delete run?",
            vec![Line::from(format!(
                "Delete '{}' and all of its on-disk state? This is permanent.",
                truncate(&run_id, 24)
            ))],
            "Delete",
            "Cancel",
        )
        .danger();
        self.pending_confirm = Some((
            ConfirmAction::Delete {
                run_ids: vec![run_id],
            },
            dialog,
        ));
    }

    /// Ask the daemon to cancel `run_id` and mark the row cancelled. The one
    /// implementation behind the list's and the detail view's kill key.
    pub(super) fn perform_kill(&mut self, run_id: &str) {
        let _ = self.cmd_tx.send(DaemonCommand::Cancel {
            run_id: run_id.to_string(),
        });
        if let Some(a) = self.agents.iter_mut().find(|a| a.id == run_id) {
            a.status = AgentDisplayStatus::Cancelled;
            a.waiting_prompt = None;
            a.pending_request = None;
        }
        // Any half-typed response to the killed run is moot.
        self.input_mode = false;
        self.input_textarea = MarkdownEdit::default();
        self.add_log(format!("{run_id}: kill requested"));
    }

    /// Cancel (via the daemon) then delete all on-disk state for `run_id`.
    /// Keyed by id rather than the current selection so the action confirmed
    /// in the dialog is the one that runs, whatever the list did since.
    pub(super) fn perform_delete(&mut self, run_id: &str) {
        let Some(raw_idx) = self.agents.iter().position(|a| a.id == run_id) else {
            return;
        };
        let id = run_id.to_string();
        // Record the run terminal on disk *before* removing it, and ask the
        // daemon to cancel it too.
        //
        // The daemon command is asynchronous while the removal below is not, so
        // an in-flight persist job can `create_dir_all` the directory straight
        // back. Writing the terminal status first means that if it does
        // reappear, it reappears as a finished run rather than as a live one the
        // user just tried to delete. (The daemon cancel is what actually stops
        // it writing; this only bounds what a lost race looks like.)
        let _ = crate::runstate::force_cancel(&id);
        let _ = self
            .cmd_tx
            .send(DaemonCommand::Cancel { run_id: id.clone() });
        // Remove run directory
        let run_dir = runstate::run_dir(&id);
        if let Err(e) = std::fs::remove_dir_all(&run_dir) {
            self.add_log(format!("Delete failed: {}", e));
        } else {
            self.add_log(format!("Deleted run {}", id));
        }
        // Remove saved context state if present. Resolved through the shared
        // LEVIATH_HOME-aware helper so the delete hits the same data root the
        // run wrote to; map() avoids a dead None branch.
        let _ = leviath_core::paths::data_dir()
            .map(|d| std::fs::remove_dir_all(d.join("state").join(&id)));
        // Remove agent from self.agents using the raw index (always valid because
        // selected_agent_raw_idx() succeeded above and agents hasn't changed).
        self.agents.remove(raw_idx);
        self.update_display_indices();
    }
}
