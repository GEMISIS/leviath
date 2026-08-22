//! What a left click does, and the registry that says so.
//!
//! Mouse capture takes the terminal's own click handling away, so everything
//! the pointer can do here has to be built. Rather than each handler
//! re-deriving where its widget landed - which is how a click ends up acting
//! on the row above the one under the pointer - every renderer registers the
//! rect it actually drew together with the [`ClickTarget`] it stands for, and
//! this module resolves a click against that frame's registry.
//!
//! Only a *plain* click arrives here: a press and release with no motion
//! between them. A drag is a text selection (see `selection.rs`), and the
//! graph canvases take their presses before either.

use ratatui::layout::{Position, Rect};

use super::state::Dashboard;
use super::types::{ClickTarget, MainPane, NewRunPane, StageContentMode};

/// How long after a click a second one on the same cell still counts as a
/// double click. 400ms is the interval most desktops ship as their default.
pub(super) const DOUBLE_CLICK_MS: u64 = 400;

impl Dashboard {
    /// Register `target` as clickable over `rect`. Renderers call this with
    /// the rect they actually drew into, every frame.
    pub(in crate::commands::dashboard) fn register_click(
        &mut self,
        rect: Rect,
        target: ClickTarget,
    ) {
        self.click_targets.push((rect, target));
    }

    /// The target under a cell: the last one registered that contains it, so
    /// a fold arrow drawn inside its row beats the row.
    fn click_target_at(&self, column: u16, row: u16) -> Option<ClickTarget> {
        self.click_targets
            .iter()
            .rev()
            .find(|(rect, _)| rect.contains(Position::new(column, row)))
            .map(|(_, target)| *target)
    }

    /// Route a press to the formatting toolbar of whichever long-form editor
    /// is on screen, reporting whether a button was under it.
    ///
    /// This runs ahead of the frame's click registry rather than through it.
    /// The registry answers "what was drawn here", and an editor's toolbar is
    /// drawn *over* whatever pane it floats above; a button press that fell
    /// through to the registry would act on the thing underneath. Clicking a
    /// button on an unfocused box also focuses it, because a press that
    /// formats text somewhere the cursor is not is a press nobody meant.
    pub(super) fn markdown_toolbar_click(&mut self, column: u16, row: u16) -> bool {
        if self.agent_builder.is_some() {
            return self.prompts_toolbar_click(column, row);
        }
        if self.new_run_screen {
            if self.new_run_task.click(column, row) {
                self.new_run_focus = NewRunPane::Task;
                return true;
            }
            return false;
        }
        self.input_mode && self.input_textarea.click(column, row)
    }

    /// Act on a plain click. Returns whether anything was under it, purely so
    /// callers and tests can tell a hit from a click on empty background.
    pub(super) fn handle_click(&mut self, column: u16, row: u16) -> bool {
        let Some(target) = self.click_target_at(column, row) else {
            return false;
        };
        // A second click on the same cell, soon enough, opens what the first
        // one selected. Recorded for every click so a click elsewhere in
        // between cannot be mistaken for the first half of a double.
        let now = (self.mouse_clock)();
        let double = self.last_click.is_some_and(|(cell, at)| {
            cell == (column, row) && now.saturating_sub(at) <= DOUBLE_CLICK_MS
        });
        self.last_click = Some(((column, row), now));

        match target {
            ClickTarget::RunToggle(pos) => self.toggle_run_fold_at(pos),
            ClickTarget::RunRow(pos) => {
                self.main_focus = MainPane::RunList;
                // Clamped rather than trusted: the registry is a frame old, and
                // a tick between the draw and the release can have shortened
                // the list.
                let pos = pos.min(self.display_indices.len().saturating_sub(1));
                self.selected = pos;
                self.table_state.select(Some(pos));
                if double {
                    self.open_detail_view();
                }
            }
            ClickTarget::LogPanel => self.main_focus = MainPane::LogPane,
            ClickTarget::StageTab(idx) => self.select_stage_tab(idx),
            ClickTarget::ContentMode(mode) => {
                self.stage_content_mode = mode;
                self.detail_scroll = 0;
                if mode == StageContentMode::Context {
                    self.reset_context_history();
                }
            }
            ClickTarget::ContextRow(idx) => {
                self.context_tree.cursor = idx;
                self.toggle_context_row();
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::dashboard::state::Dashboard;
    use crate::commands::dashboard::test_support::make_test_dashboard;
    use crate::commands::dashboard::types::{AgentDisplayStatus, DashboardAgent};
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn make_test_agent(id: &str) -> DashboardAgent {
        DashboardAgent {
            id: id.to_string(),
            blueprint_name: "test-agent".to_string(),
            stage: "main".to_string(),
            stage_index: 0,
            num_stages: 3,
            status: AgentDisplayStatus::Active,
            tokens_in: 0,
            tokens_out: 0,
            cached_tokens: 0,
            iteration: 0,
            waiting_prompt: None,
            wait_reason: None,
            pending_request: None,
            last_answered_request_id: None,
            context_snapshot: None,
            stages: vec![],
            workdir: "/tmp".to_string(),
            task: "test".to_string(),
            title: Some(id.to_string()),
            model: None,
            parent_id: None,
            depth: 0,
            started_at: 1000,
            last_progress_at: None,
            active_until: None,
            waiting_secs: 0,
            graph: None,
            accepts_messages: true,
            taint_summary: vec![],
        }
    }

    /// Draw a whole frame the way the loop does, so the click targets under
    /// test are the ones the renderers actually registered.
    fn draw(dash: &mut Dashboard, width: u16, height: u16) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| dash.draw(f)).unwrap();
    }

    /// Where the long-form editor's `B` button landed in the drawn frame.
    ///
    /// Found in the buffer rather than computed from the layout, so the test
    /// clicks the cell a person would click and not the cell the test thinks
    /// the renderer should have used. The row reads `" B │ I │ ..."`, so a `B`
    /// with an `I` four columns later is the toolbar and nothing else is.
    fn find_bold_button(dash: &mut Dashboard, width: u16, height: u16) -> (u16, u16) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| dash.draw(f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let at = |x: u16, y: u16| buf.cell((x, y)).map(|c| c.symbol().to_string());
        (0..height)
            .flat_map(|y| (0..width.saturating_sub(4)).map(move |x| (x, y)))
            .find(|&(x, y)| {
                at(x, y).as_deref() == Some("B") && at(x + 4, y).as_deref() == Some("I")
            })
            .expect("a formatting toolbar was drawn")
    }

    fn press_and_release(dash: &mut Dashboard, column: u16, row: u16) {
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            dash.handle_mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers: crossterm::event::KeyModifiers::NONE,
            });
        }
    }

    /// A frozen clock, so a "double click" is two clicks and not a race with
    /// the test runner.
    fn frozen_clock() -> u64 {
        10_000
    }

    /// A clock far enough on that the second click is its own click.
    fn much_later() -> u64 {
        10_000 + DOUBLE_CLICK_MS + 1
    }

    /// The whole gesture, through the real renderers: draw the list, click the
    /// second run's row, and the selection follows the pointer.
    #[test]
    fn clicking_a_run_row_selects_that_run() {
        let mut dash = make_test_dashboard();
        dash.agents.push(make_test_agent("run-1"));
        dash.agents.push(make_test_agent("run-2"));
        dash.update_display_indices();
        dash.main_focus = MainPane::LogPane;
        draw(&mut dash, 120, 40);

        // Row 0 of the table sits below the border and the header row.
        press_and_release(&mut dash, 10, 3);
        assert_eq!(dash.selected, 1, "the second run");
        assert_eq!(
            dash.main_focus,
            MainPane::RunList,
            "clicking the list also focuses it"
        );
        assert!(!dash.detail_view, "one click selects, it does not open");
    }

    /// A second click on the same row opens the run; the same two clicks
    /// spread further apart do not.
    #[test]
    fn a_double_click_opens_the_run_and_two_slow_clicks_do_not() {
        let mut dash = make_test_dashboard();
        dash.agents.push(make_test_agent("run-1"));
        dash.update_display_indices();
        dash.mouse_clock = frozen_clock;
        draw(&mut dash, 120, 40);

        press_and_release(&mut dash, 10, 2);
        assert!(!dash.detail_view);
        press_and_release(&mut dash, 10, 2);
        assert!(dash.detail_view, "the second click opened it");

        dash.detail_view = false;
        dash.mouse_clock = much_later;
        draw(&mut dash, 120, 40);
        press_and_release(&mut dash, 10, 2);
        assert!(!dash.detail_view, "too slow to be a double click");
    }

    /// Clicking the fold arrow folds the subtree, and clicking it again puts
    /// it back - without opening anything, even though the two clicks land on
    /// the same cell in quick succession.
    #[test]
    fn clicking_the_fold_arrow_folds_the_subtree() {
        let mut dash = make_test_dashboard();
        dash.agents.push(make_test_agent("parent"));
        let mut child = make_test_agent("worker");
        child.parent_id = Some("parent".to_string());
        dash.agents.push(child);
        dash.update_display_indices();
        dash.mouse_clock = frozen_clock;
        draw(&mut dash, 120, 40);
        assert_eq!(dash.display_indices.len(), 2);

        // The arrow is the first two columns of the parent's title cell.
        press_and_release(&mut dash, 2, 2);
        assert_eq!(dash.display_indices.len(), 1, "the worker folded away");
        assert!(!dash.detail_view);

        draw(&mut dash, 120, 40);
        press_and_release(&mut dash, 2, 2);
        assert_eq!(dash.display_indices.len(), 2, "and came back");
        assert!(!dash.detail_view, "an arrow click never opens the run");
    }

    /// A drag is a text selection, so it must not also act on what it started
    /// over.
    #[test]
    fn dragging_across_a_row_selects_text_rather_than_the_row() {
        let mut dash = make_test_dashboard();
        dash.agents.push(make_test_agent("run-1"));
        dash.agents.push(make_test_agent("run-2"));
        dash.update_display_indices();
        draw(&mut dash, 120, 40);

        for (kind, column) in [
            (MouseEventKind::Down(MouseButton::Left), 10),
            (MouseEventKind::Drag(MouseButton::Left), 30),
            (MouseEventKind::Up(MouseButton::Left), 30),
        ] {
            dash.handle_mouse(MouseEvent {
                kind,
                column,
                row: 3,
                modifiers: crossterm::event::KeyModifiers::NONE,
            });
        }
        assert_eq!(dash.selected, 0, "the drag did not move the selection");
    }

    /// The detail view's stage tabs and mode chips are buttons.
    #[test]
    fn clicking_a_stage_tab_and_a_mode_chip_in_the_detail_view() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1");
        agent.stages = (0..3)
            .map(|i| crate::runstate::StageRecord {
                name: format!("stage{i}"),
                index: i,
                status: crate::runstate::StageRunStatus::Pending,
                entered: false,
                prompt_tokens: 0,
                completion_tokens: 0,
                cached_tokens: 0,
                cache_write_tokens: 0,
                region_tokens: Default::default(),
                first_call_prompt_tokens: None,
                runaway_warned: false,
                started_at: None,
                ended_at: None,
            })
            .collect();
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        // Short enough that the stage row is the linear tab strip, not the
        // graph band (which has its own click handling).
        draw(&mut dash, 120, 24);

        let tab = dash
            .click_targets
            .iter()
            .find(|(_, t)| *t == ClickTarget::StageTab(2))
            .map(|(r, _)| *r)
            .expect("three stages, three tabs");
        press_and_release(&mut dash, tab.x + 1, tab.y);
        assert_eq!(dash.selected_stage, 2);

        draw(&mut dash, 120, 24);
        let chip = dash
            .click_targets
            .iter()
            .find(|(_, t)| *t == ClickTarget::ContentMode(StageContentMode::Context))
            .map(|(r, _)| *r)
            .expect("the ctx chip is in the title");
        press_and_release(&mut dash, chip.x + 1, chip.y);
        assert_eq!(dash.stage_content_mode, StageContentMode::Context);
    }

    /// A click on a Context row folds it, exactly as enter would.
    #[test]
    fn clicking_a_context_region_folds_it() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1");
        agent.num_stages = 1;
        agent.context_snapshot = Some(std::sync::Arc::new(crate::runstate::ContextSnapshot {
            stage_name: "main".to_string(),
            total_tokens: 20,
            max_tokens: 100,
            regions: vec![crate::runstate::RegionSnapshot {
                name: "system".to_string(),
                kind: "pinned".to_string(),
                current_tokens: 10,
                max_tokens: 50,
                entries: vec![leviath_core::run_meta::RegionEntrySnapshot {
                    content: "hello".to_string(),
                    tokens: 5,
                    kind: Default::default(),
                    metadata: None,
                    key: None,
                    taint: Default::default(),
                }],
                description: None,
            }],
        }));
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.stage_content_mode = StageContentMode::Context;
        draw(&mut dash, 120, 40);

        let header = dash
            .click_targets
            .iter()
            .find(|(_, t)| *t == ClickTarget::ContextRow(0))
            .map(|(r, _)| *r)
            .expect("the region header is on screen");
        press_and_release(&mut dash, header.x + 2, header.y);
        assert!(
            dash.context_tree.collapsed_regions.contains("system"),
            "the click folded the region the pointer was over"
        );
    }

    /// A click resolves to the innermost target: the toggle registered over
    /// part of a row wins against the row it sits in.
    #[test]
    fn the_last_registered_target_over_a_cell_wins() {
        let mut dash = make_test_dashboard();
        dash.register_click(Rect::new(0, 5, 40, 1), ClickTarget::RunRow(0));
        dash.register_click(Rect::new(2, 5, 2, 1), ClickTarget::RunToggle(0));
        assert_eq!(
            dash.click_target_at(3, 5),
            Some(ClickTarget::RunToggle(0)),
            "inside the toggle"
        );
        assert_eq!(
            dash.click_target_at(20, 5),
            Some(ClickTarget::RunRow(0)),
            "elsewhere on the row"
        );
        assert_eq!(dash.click_target_at(20, 6), None, "off the row");
    }

    /// Clicking nothing is not an error, and does not disturb the selection.
    #[test]
    fn a_click_on_empty_background_does_nothing() {
        let mut dash = make_test_dashboard();
        assert!(!dash.handle_click(1, 1));
        assert_eq!(dash.selected, 0);
    }

    #[test]
    fn clicking_the_log_panel_moves_the_keyboard_there() {
        let mut dash = make_test_dashboard();
        dash.register_click(Rect::new(0, 0, 10, 10), ClickTarget::LogPanel);
        assert!(dash.handle_click(5, 5));
        assert_eq!(dash.main_focus, MainPane::LogPane);
    }

    /// Each chip switches to its own mode, and only the Context one leaves
    /// history browsing (the other two do not show a context window at all).
    #[test]
    fn clicking_a_mode_chip_switches_the_content_pane() {
        let mut dash = make_test_dashboard();
        dash.detail_scroll = 12;
        dash.context_history_idx = Some(3);
        dash.register_click(
            Rect::new(0, 0, 10, 1),
            ClickTarget::ContentMode(StageContentMode::Logs),
        );
        assert!(dash.handle_click(3, 0));
        assert_eq!(dash.stage_content_mode, StageContentMode::Logs);
        assert_eq!(dash.detail_scroll, 0, "the new pane starts at the bottom");
        assert_eq!(
            dash.context_history_idx,
            Some(3),
            "the Logs pane does not touch which context point is browsed"
        );

        dash.click_targets.clear();
        dash.register_click(
            Rect::new(0, 0, 10, 1),
            ClickTarget::ContentMode(StageContentMode::Context),
        );
        assert!(dash.handle_click(3, 0));
        assert_eq!(dash.stage_content_mode, StageContentMode::Context);
        assert_eq!(
            dash.context_history_idx, None,
            "the ctx chip shows the live window, like the `c` key"
        );
    }

    // ─── the long-form editors' formatting toolbars ─────────────────────────

    /// The new-run task box: clicking `B` wraps at the cursor and moves the
    /// keys to the pane you just formatted, so the next thing typed lands
    /// between the markers.
    #[test]
    fn clicking_the_task_boxs_bold_button_formats_it_and_takes_the_focus() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = make_test_dashboard();
        dash.new_run_ctx.workdir = dir.path().to_path_buf();
        dash.new_run_ctx.agents_dir = dir.path().join("agents");
        dash.new_run_ctx.config_path = dir.path().join("config.toml");
        dash.open_new_run_screen();
        assert_eq!(dash.new_run_focus, NewRunPane::Agents);

        let (x, y) = find_bold_button(&mut dash, 120, 40);
        press_and_release(&mut dash, x, y);
        assert_eq!(dash.new_run_task.text(), "****");
        assert_eq!(dash.new_run_focus, NewRunPane::Task);

        // A press inside the text itself is not a button press.
        draw(&mut dash, 120, 40);
        press_and_release(&mut dash, x, y + 2);
        assert_eq!(dash.new_run_task.text(), "****");
    }

    /// The same component, in the response box: the wiring has to find it
    /// there too, and nowhere else on the screen counts as a button.
    #[test]
    fn clicking_the_response_boxs_bold_button_formats_the_response() {
        let mut dash = make_test_dashboard();
        dash.agents.push(make_test_agent("run-1"));
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = true;

        let (x, y) = find_bold_button(&mut dash, 120, 40);
        press_and_release(&mut dash, x, y);
        assert_eq!(dash.input_textarea.text(), "****");

        // With the box closed there is no toolbar to hit, so the same cell is
        // an ordinary click again.
        dash.input_mode = false;
        draw(&mut dash, 120, 40);
        assert!(!dash.markdown_toolbar_click(x, y));
    }

    /// A press on the run list, with no long-form box open anywhere, must not
    /// be swallowed by the toolbar check that now runs ahead of everything.
    #[test]
    fn a_press_with_no_editor_open_falls_through_to_the_panes() {
        let mut dash = make_test_dashboard();
        dash.agents.push(make_test_agent("run-1"));
        dash.update_display_indices();
        draw(&mut dash, 120, 40);
        assert!(!dash.markdown_toolbar_click(5, 5));
    }
}
