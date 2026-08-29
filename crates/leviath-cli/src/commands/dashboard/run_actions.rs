//! Killing and deleting runs from the dashboard: the confirmations, and
//! what runs when they are answered.

use super::helpers::truncate;
use super::state::Dashboard;
use super::types::*;
use crate::runstate;

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
            let lines = self.delete_lines(body, &run_ids);
            let dialog = Confirm::new("Delete runs?", lines, "Delete", "Cancel").danger();
            self.pending_confirm = Some((ConfirmAction::Delete { run_ids }, dialog));
            return;
        }
        let Some(agent) = self.selected_agent() else {
            return;
        };
        let run_id = agent.id.clone();
        let body = format!(
            "Delete '{}' and all of its on-disk state? This is permanent.",
            truncate(&run_id, 24)
        );
        let lines = self.delete_lines(body, std::slice::from_ref(&run_id));
        let dialog = Confirm::new("Delete run?", lines, "Delete", "Cancel").danger();
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
        self.close_input_box();
        self.add_log(format!("{run_id}: kill requested"));
    }

    /// The rows nested under `run_ids` that a delete of them takes too, beyond
    /// the runs named.
    ///
    /// Counted over the whole set at once rather than summed per run: marking
    /// a parent and one of its own children is ordinary, and that child must
    /// not be counted twice. The `seen` set is also what stops metadata
    /// claiming an ancestor as a child from walking forever.
    ///
    /// Read off the list on screen rather than off disk. These rows are the
    /// ones the user can see nested under the selection, which is the thing
    /// the dialog is about to describe, and a confirmation must not be the
    /// thing that stops to parse every `meta.json` on the machine.
    fn extra_sub_agent_rows(&self, run_ids: &[String]) -> usize {
        let mut seen: std::collections::HashSet<&str> =
            run_ids.iter().map(String::as_str).collect();
        let mut frontier: Vec<&str> = run_ids.iter().map(String::as_str).collect();
        while let Some(id) = frontier.pop() {
            let children = self
                .agents
                .iter()
                .filter(|a| a.parent_id.as_deref() == Some(id));
            for child in children {
                if seen.insert(&child.id) {
                    frontier.push(&child.id);
                }
            }
        }
        seen.len() - run_ids.len()
    }

    /// The confirmation body, plus a line naming the sub-agent runs that go
    /// with the runs named - when there are any.
    ///
    /// Said out loud because those are rows the user did not select. A dialog
    /// that says "1 run" and then removes nine is the prompt lying about what
    /// the key does, and a delete is not an action anyone gets to take back.
    fn delete_lines(&self, body: String, run_ids: &[String]) -> Vec<ratatui::text::Line<'static>> {
        use ratatui::text::Line;
        let subs = self.extra_sub_agent_rows(run_ids);
        let mut lines = vec![Line::from(body)];
        if subs > 0 {
            lines.push(Line::from(format!(
                "Also deletes {subs} sub-agent run{} below {}.",
                if subs == 1 { "" } else { "s" },
                if run_ids.len() == 1 { "it" } else { "them" },
            )));
        }
        lines
    }

    /// Cancel (via the daemon) then delete all on-disk state for `run_id` and
    /// every sub-agent run beneath it. Keyed by id rather than the current
    /// selection so the action confirmed in the dialog is the one that runs,
    /// whatever the list did since.
    ///
    /// The sub-agents go because they are drawn as this run's children and
    /// nothing else on disk accounts for them. Deleting the parent alone left
    /// them behind, and the list treats a run whose parent is absent as a
    /// root, so the rows that were nested under the deleted run jumped to the
    /// top level instead of disappearing.
    ///
    /// Nothing walks upwards: a delete on a child leaves its parent and its
    /// siblings alone.
    pub(super) fn perform_delete(&mut self, run_id: &str) {
        if !self.agents.iter().any(|a| a.id == run_id) {
            return;
        }
        // Deepest first, so a removal that fails part way through leaves a
        // parent whose children are gone rather than the orphans this is
        // fixing.
        let ids = runstate::family_of(run_id);
        for id in &ids {
            // The run the user named is always cancelled, as it always was. A
            // sub-agent is only cancelled if the list still says it is going:
            // a finished one has nothing to cancel, and asking anyway writes
            // "the daemon has no such run to cancel" into the activity log
            // once per row, so a fan-out of twenty would bury the delete it
            // came from.
            let cancel = id == run_id
                || self
                    .agents
                    .iter()
                    .any(|a| &a.id == id && a.status.is_killable());
            self.delete_one(id, cancel);
        }
        self.agents.retain(|a| !ids.contains(&a.id));
        self.update_display_indices();
    }

    /// Cancel and erase one run's on-disk state.
    fn delete_one(&mut self, id: &str, cancel: bool) {
        // Record the run terminal on disk *before* removing it, and ask the
        // daemon to cancel it too.
        //
        // The daemon command is asynchronous while the removal below is not, so
        // an in-flight persist job can `create_dir_all` the directory straight
        // back. Writing the terminal status first means that if it does
        // reappear, it reappears as a finished run rather than as a live one the
        // user just tried to delete. (The daemon cancel is what actually stops
        // it writing; this only bounds what a lost race looks like.)
        let _ = crate::runstate::force_cancel(id);
        if cancel {
            let _ = self.cmd_tx.send(DaemonCommand::Cancel {
                run_id: id.to_string(),
            });
        }
        // Remove run directory
        let run_dir = runstate::run_dir(id);
        if let Err(e) = std::fs::remove_dir_all(&run_dir) {
            self.add_log(format!("Delete failed: {}", e));
        } else {
            self.add_log(format!("Deleted run {}", id));
        }
        // Remove saved context state if present. Resolved through the shared
        // LEVIATH_HOME-aware helper so the delete hits the same data root the
        // run wrote to; map() avoids a dead None branch.
        let _ = leviath_core::paths::data_dir()
            .map(|d| std::fs::remove_dir_all(d.join("state").join(id)));
    }
}
