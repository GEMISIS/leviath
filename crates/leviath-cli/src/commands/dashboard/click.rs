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
use crate::tui::widgets::markdown_edit::{MdMode, MdOutcome};

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

    /// Take the view a long-form box just switched to as the user's standing
    /// preference, so the next box opens the same way and so does the next
    /// session. Reports whether anything on the box happened at all.
    ///
    /// Every host funnels its keys and clicks through here, which is what
    /// keeps four boxes and one remembered choice in step: the box reports the
    /// switch, and exactly one place writes it down.
    pub(in crate::commands::dashboard) fn remember_md_mode(&mut self, outcome: MdOutcome) -> bool {
        match outcome {
            MdOutcome::ModeChanged(mode) => {
                self.md_preview = mode == MdMode::Preview;
                self.save_ui_state();
                true
            }
            MdOutcome::Edited => true,
            MdOutcome::Ignored => false,
        }
    }

    /// The view a newly opened box should start in.
    pub(in crate::commands::dashboard) fn md_mode(&self) -> MdMode {
        match self.md_preview {
            true => MdMode::Preview,
            false => MdMode::Source,
        }
    }

    /// Follow the pointer over whichever long-form editor is on screen, so the
    /// button under it lifts and the box's bottom border names it.
    ///
    /// Motion events already wake the loop (crossterm's mouse capture turns on
    /// any-motion tracking), so this costs a hit test, not a redraw.
    pub(super) fn markdown_toolbar_hover(&mut self, column: u16, row: u16) {
        if self.agent_builder.is_some() {
            self.prompts_toolbar_hover(column, row);
            return;
        }
        if self.new_run_screen {
            self.new_run_task.hover(column, row);
            return;
        }
        if self.input_mode {
            self.input_textarea.hover(column, row);
        }
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
            let outcome = self.new_run_task.click(column, row);
            if outcome != MdOutcome::Ignored {
                self.new_run_focus = NewRunPane::Task;
            }
            return self.remember_md_mode(outcome);
        }
        if !self.input_mode {
            return false;
        }
        let outcome = self.input_textarea.click(column, row);
        self.remember_md_mode(outcome)
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
            ClickTarget::NewRunStart => self.submit_new_run(),
            ClickTarget::ResponseSend => self.submit_input(),
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::dashboard::render::{SAVE_BUTTON, SEND_BUTTON};
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
            broken_scripts: Vec::new(),
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
            started_at: 1000,
            last_progress_at: None,
            runtime_secs: 0,
            clock_now: 0,
            graph: None,
            accepts_messages: true,
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
    /// the renderer should have used. The row reads `" B  i  S  U "`, and all
    /// four letters at that spacing are the toolbar and nothing else: the
    /// output pane's border above it prints the run's path, whose temp-dir
    /// suffix is random, and `rs-22643f03-B0aiU6` once matched a `B` with an
    /// `i` three columns on, so the click landed on a border instead.
    fn find_bold_button(dash: &mut Dashboard, width: u16, height: u16) -> (u16, u16) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| dash.draw(f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let at = |x: u16, y: u16| buf.cell((x, y)).map(|c| c.symbol().to_string());
        (0..height)
            .flat_map(|y| (0..width.saturating_sub(9)).map(move |x| (x, y)))
            .find(|&(x, y)| {
                [(0, "B"), (3, "i"), (6, "S"), (9, "U")]
                    .iter()
                    .all(|&(dx, glyph)| at(x + dx, y).as_deref() == Some(glyph))
            })
            .expect("a formatting toolbar was drawn")
    }

    /// The cell holding `glyph`. Scanned cell by cell rather than by searching
    /// the row as a string: a row is full of multi-byte box-drawing
    /// characters, so a byte offset into it is not a column.
    fn find_chip(dash: &mut Dashboard, glyph: &str, width: u16, height: u16) -> (u16, u16) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| dash.draw(f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..height)
            .flat_map(|y| (0..width).map(move |x| (x, y)))
            .find(|(x, y)| buf.cell((*x, *y)).is_some_and(|c| c.symbol() == glyph))
            .expect("the chip was drawn")
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
            .map(|i| crate::runstate::StageRecord::new(format!("stage{i}"), i))
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
                    reasoning: None,
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

    /// The `[f] final` chip switches to the Final view through the same
    /// path as the other chips.
    #[test]
    fn clicking_the_final_chip_opens_the_final_view() {
        crate::runstate::with_isolated_runs_dir(
            "clicking_the_final_chip_opens_the_final_view",
            |_d| {
                let mut dash = make_test_dashboard();
                let mut agent = make_test_agent("run-final-chip");
                agent.stages = (0..3)
                    .map(|i| crate::runstate::StageRecord::new(format!("stage{i}"), i))
                    .collect();
                crate::commands::dashboard::test_support::seed_run_with_final_output(
                    "run-final-chip",
                    "stage2",
                    "the answer",
                );
                dash.agents.push(agent);
                dash.update_display_indices();
                dash.detail_view = true;
                draw(&mut dash, 120, 24);

                let chip = dash
                    .click_targets
                    .iter()
                    .find(|(_, t)| *t == ClickTarget::ContentMode(StageContentMode::FinalOutput))
                    .map(|(r, _)| *r)
                    .expect("the final chip is in the title");
                press_and_release(&mut dash, chip.x + 1, chip.y);
                assert_eq!(dash.stage_content_mode, StageContentMode::FinalOutput);
            },
        );
    }

    /// The new-run screen's Start button starts the run with a click, the
    /// way Enter on it does: the whole point of the button is a terminal
    /// where Ctrl+Enter cannot be told from Enter.
    #[test]
    fn clicking_the_start_button_starts_the_run() {
        let mut dash = make_test_dashboard();
        dash.new_run_screen = true;
        dash.new_run_agents = vec![crate::commands::dashboard::types::NewRunAgent {
            name: "alpha".to_string(),
            source: "installed".to_string(),
            description: "first".to_string(),
            path: "/agents/alpha".to_string(),
        }];
        dash.new_run_task.area_mut().insert_str("ship it");
        draw(&mut dash, 120, 40);

        let button = dash
            .click_targets
            .iter()
            .find(|(_, t)| *t == ClickTarget::NewRunStart)
            .map(|(r, _)| *r)
            .expect("the Start button is on screen");
        press_and_release(&mut dash, button.x + 2, button.y);

        assert!(
            !dash.new_run_screen,
            "the run was dispatched and the form closed"
        );
        let cmd = dash
            .spawn_cmd_rx_for_test()
            .try_recv()
            .expect("a spawn was dispatched");
        assert_eq!(cmd.task, "ship it");
        assert_eq!(cmd.agent_path, "/agents/alpha");
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
        // The pointer reaching the button lights it before the press lands.
        dash.markdown_toolbar_hover(x, y);
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

    /// The Send button under the response box sends with a click, the way
    /// Enter on it does. It is the same widget as the new-run Start button,
    /// so its rect is the drawn text and a press beside it is not a press.
    #[test]
    fn clicking_the_send_button_sends_the_response() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1");
        agent.pending_request = Some(leviath_core::interaction::InteractionRequest::free_text(
            "ft1", "prompt", "main", true,
        ));
        agent.waiting_prompt = Some("prompt".to_string());
        agent.status = AgentDisplayStatus::Waiting;
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = true;
        dash.input_textarea.area_mut().insert_str("answer");

        draw(&mut dash, 120, 40);
        let button = dash
            .click_targets
            .iter()
            .find(|(_, t)| *t == ClickTarget::ResponseSend)
            .map(|(r, _)| *r)
            .expect("the Send button is on screen");
        assert_eq!(button.width as usize, SEND_BUTTON.chars().count());
        press_and_release(&mut dash, button.x.saturating_sub(1), button.y);
        assert!(
            dash.input_mode,
            "a press beside the button is not a press on it"
        );
        press_and_release(&mut dash, button.x + 2, button.y);
        assert!(!dash.input_mode, "the click sent");
        assert!(dash.agents[0].pending_request.is_none());
    }

    /// The Save button under an in-place document edit is the same wiring in
    /// the content pane.
    #[test]
    fn clicking_the_save_button_saves_the_document() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1");
        agent.pending_request = Some(leviath_core::interaction::InteractionRequest::edit_text(
            "et1", "Edit", "main", "old",
        ));
        agent.status = AgentDisplayStatus::Waiting;
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = true;
        dash.seed_input_textarea();

        draw(&mut dash, 120, 40);
        let button = dash
            .click_targets
            .iter()
            .find(|(_, t)| *t == ClickTarget::ResponseSend)
            .map(|(r, _)| *r)
            .expect("the Save button is on screen");
        assert_eq!(button.width as usize, SAVE_BUTTON.chars().count());
        press_and_release(&mut dash, button.x + 2, button.y);
        assert!(!dash.input_mode, "the click saved");
        assert!(dash.agents[0].pending_request.is_none());
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
        // Motion with nothing open is a no-op rather than a panic.
        dash.markdown_toolbar_hover(5, 5);
    }

    /// Switching the view is a preference, not a per-box setting: it is
    /// written down once and every box opened afterwards starts there.
    #[test]
    fn switching_the_view_is_remembered_for_the_next_box_and_the_next_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = make_test_dashboard();
        dash.ui_state_path = Some(dir.path().join("ui-state.json"));
        dash.new_run_ctx.workdir = dir.path().to_path_buf();
        dash.new_run_ctx.agents_dir = dir.path().join("agents");
        dash.new_run_ctx.config_path = dir.path().join("config.toml");
        dash.open_new_run_screen();
        assert_eq!(dash.new_run_task.mode(), MdMode::Source);

        // Press the view switch, found where it was drawn.
        let (switch, y) = find_chip(&mut dash, "⇄", 120, 40);
        press_and_release(&mut dash, switch, y);
        assert_eq!(dash.new_run_task.mode(), MdMode::Preview);
        assert!(dash.md_preview);

        // The next box opens the same way, and so does the next dashboard.
        dash.open_new_run_screen();
        assert_eq!(dash.new_run_task.mode(), MdMode::Preview);

        let mut next = make_test_dashboard();
        next.ui_state_path = Some(dir.path().join("ui-state.json"));
        next.load_ui_state();
        assert!(next.md_preview, "the choice outlived the session");
        assert_eq!(next.md_mode(), MdMode::Preview);
    }

    /// The pointer moving over a button lights it and names it, on whichever
    /// box is on screen.
    #[test]
    fn motion_over_a_toolbar_names_the_button_under_it() {
        let mut dash = make_test_dashboard();
        dash.agents.push(make_test_agent("run-1"));
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = true;

        let (x, y) = find_bold_button(&mut dash, 120, 40);
        dash.handle_mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: x,
            row: y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| dash.draw(f)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(screen.contains("bold"), "the border names it: {screen}");
    }
}
