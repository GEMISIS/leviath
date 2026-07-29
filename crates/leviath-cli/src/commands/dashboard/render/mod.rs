//! Rendering logic for the dashboard TUI.
//!
//! The main `draw()` method dispatches to sub-modules for each region of the UI.

mod content;
mod header;
mod input;
mod mcp;
mod overlays;
mod stages;
mod table;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};

use super::state::Dashboard;
use super::types::*;

impl Dashboard {
    pub(super) fn draw(&mut self, frame: &mut Frame) {
        // Selectable panes re-register their rects as they render; anything
        // still selected in a pane that does not come back this frame is
        // dropped by the overlay pass below.
        self.selection_regions.clear();

        if self.mcp_screen {
            self.draw_mcp_screen(frame, frame.area());
            // No pane registered a rect, so this drops any selection.
            self.validate_selection();
            self.draw_toasts(frame);
            return;
        }
        if self.detail_view {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(1), Constraint::Length(1)])
                .split(frame.area());
            self.draw_detail_panel(frame, chunks[0]);
            self.draw_help_bar(frame, chunks[1]);
        } else {
            // Normal layout: agent table + log panel + help bar
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Percentage(55),
                    Constraint::Percentage(44),
                    Constraint::Length(1),
                ])
                .split(frame.area());
            self.draw_agent_table(frame, chunks[0]);
            self.draw_log_panel(frame, chunks[1]);
            self.draw_help_bar(frame, chunks[2]);
        }

        // Mouse-selection highlight and released-selection copy, painted over
        // the panes but under the popups below.
        self.apply_selection_overlay(frame);

        // Render toasts (top-right overlay)
        self.draw_toasts(frame);

        // Help overlay
        if self.show_help {
            self.draw_help_overlay(frame);
        }

        // Delete confirmation popup
        if self.confirm_delete {
            self.draw_confirm_popup(frame);
        }
    }

    fn draw_detail_panel(&mut self, frame: &mut Frame, area: ratatui::layout::Rect) {
        use leviath_core::interaction::InteractionKind;

        let agent = match self.selected_agent() {
            Some(a) => a.clone(),
            None => {
                let msg = ratatui::widgets::Paragraph::new("No agent selected.").block(
                    ratatui::widgets::Block::default()
                        .borders(ratatui::widgets::Borders::ALL)
                        .title(" Detail "),
                );
                frame.render_widget(msg, area);
                return;
            }
        };

        // Clamp selected_stage to valid range
        let max_tab = agent.num_stages.saturating_sub(1);
        if self.selected_stage > max_tab {
            self.selected_stage = max_tab;
        }

        // ── Layout: header + tabs + context bar + content + [input] ──────────
        let is_waiting = matches!(
            agent.status,
            AgentDisplayStatus::Waiting | AgentDisplayStatus::CompleteInteractive
        );
        let pending_req = agent.pending_request.clone();
        let kind = pending_req.as_ref().map(|r| r.kind.clone());
        let options: Vec<String> = pending_req
            .as_ref()
            .map(|r| r.options.clone())
            .unwrap_or_default();

        // Only show input pane on the tab that can actually respond
        let has_prompt = is_waiting
            && (pending_req.is_some() || agent.waiting_prompt.is_some())
            && !matches!(agent.status, AgentDisplayStatus::Cancelled)
            && self.selected_stage_can_respond();
        // Agent is Active (not waiting on anything) but its current stage
        // supports mid-run messages — same 'i' key, same input pane.
        let accepts_messages = self.selected_agent_accepts_messages();

        let header_h: u16 = 1; // compact breadcrumb line
        let info_h: u16 = 4; // task + workdir/stats strip (2 content + 2 border lines)
        let is_graph_view = agent.graph_info.is_some();
        let tabs_h: u16 = if is_graph_view { 7 } else { 3 }; // graph needs more height
        let context_h: u16 = if agent.context_snapshot.is_some() || !agent.stages.is_empty() {
            5
        } else {
            0
        };

        // Review body: shown when the pending interaction carries markdown for
        // review. `EditText` also uses `body`, but that is the editable document
        // (rendered in the seeded textarea once the user starts editing), not a
        // read-only review doc — so it is excluded here. When the output pane is
        // already showing the document (output mode), the separate review pane is
        // suppressed to avoid rendering it twice.
        let content_shows_body =
            self.stage_content_mode == StageContentMode::Output && self.reviewing_body().is_some();
        let review_body = if !self.input_mode && has_prompt && !content_shows_body {
            pending_req
                .as_ref()
                .filter(|r| r.kind != InteractionKind::EditText)
                .and_then(|r| r.body.as_deref())
        } else {
            None
        };
        // Pre-render the markdown so we know how many lines it produces
        let review_lines: Vec<ratatui::text::Line<'static>> = if let Some(body) = review_body {
            let w = area.width.saturating_sub(4);
            crate::render::markdown_to_text(body, w).lines
        } else {
            Vec::new()
        };
        let review_h: u16 = if review_lines.is_empty() {
            0
        } else {
            // Allocate up to 40% of the panel height, minimum 8 lines + 2 border
            let max_review = (area.height as usize * 2 / 5).clamp(10, 24);
            (review_lines.len() + 2).min(max_review) as u16
        };

        // While editing a document the textarea is rendered in the content pane,
        // so the bottom input bar is suppressed (no double render).
        let editing_doc = self.editing_document();
        let prompt_height: u16 = if editing_doc {
            0
        } else if has_prompt
            || (self.input_mode && is_waiting && self.selected_stage_can_respond())
            || (self.input_mode && accepts_messages)
        {
            let n = options.len() as u16;
            if self.input_mode {
                match &kind {
                    Some(InteractionKind::FreeText) | None => 11,
                    _ => (n + 4).min(14),
                }
            } else {
                match &kind {
                    Some(InteractionKind::FreeText) | None => 6,
                    _ => (n + 5).min(14),
                }
            }
        } else {
            0
        };

        let mut constraints = vec![
            Constraint::Length(header_h),
            Constraint::Length(info_h),
            Constraint::Length(tabs_h),
        ];
        if context_h > 0 {
            constraints.push(Constraint::Length(context_h));
        }
        constraints.push(Constraint::Min(4)); // content pane
        if review_h > 0 {
            constraints.push(Constraint::Length(review_h));
        }
        if prompt_height > 0 {
            constraints.push(Constraint::Length(prompt_height));
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);

        let mut chunk_idx = 0;

        // ── Header breadcrumb ──
        self.render_header_breadcrumb(frame, chunks[chunk_idx], &agent);
        chunk_idx += 1;

        // ── Info strip ──
        self.render_info_strip(frame, chunks[chunk_idx], &agent, area.width);
        chunk_idx += 1;

        // ── Stage tabs / graph ──
        self.render_stage_tabs(frame, chunks[chunk_idx], &agent);
        chunk_idx += 1;

        // ── Context bar ──
        if context_h > 0 {
            self.render_context_bar(frame, chunks[chunk_idx], &agent);
            chunk_idx += 1;
        }

        // ── Content pane ──
        self.render_content_pane(frame, chunks[chunk_idx], &agent, area.width);
        chunk_idx += 1;

        // ── Review body ──
        if review_h > 0 {
            self.render_review_body(frame, chunks[chunk_idx], &review_lines, &pending_req);
            chunk_idx += 1;
        }

        // ── Input / prompt ──
        if prompt_height > 0 {
            self.render_input_pane(
                frame,
                chunks[chunk_idx],
                &agent,
                &pending_req,
                &kind,
                &options,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::dashboard::test_support::make_test_dashboard;
    use crate::commands::dashboard::types::{Toast, ToastLevel};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn make_test_agent(id: &str, status: AgentDisplayStatus) -> DashboardAgent {
        DashboardAgent {
            id: id.to_string(),
            blueprint_name: "test-agent".to_string(),
            stage: "main".to_string(),
            stage_index: 0,
            num_stages: 1,
            status,
            tokens_in: 100,
            tokens_out: 50,
            cached_tokens: 10,
            iteration: 3,
            waiting_prompt: None,
            pending_request: None,
            last_answered_request_id: None,
            context_snapshot: None,
            stages: vec![],
            workdir: "/tmp/test".to_string(),
            task: "test task".to_string(),
            title: Some("My Test".to_string()),
            model: Some("claude-sonnet-4-20250514".to_string()),
            parent_id: None,
            depth: 0,
            started_at: chrono::Utc::now().timestamp() - 60,
            active_until: None,
            waiting_secs: 0,
            graph_info: None,
            accepts_messages: true,
            taint_summary: vec![],
        }
    }

    #[test]
    fn draw_normal_mode_no_agents() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        terminal.draw(|f| dash.draw(f)).unwrap();
    }

    #[test]
    fn draw_normal_mode_with_agents() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.agents
            .push(make_test_agent("run-2", AgentDisplayStatus::Complete));
        dash.update_display_indices();
        terminal.draw(|f| dash.draw(f)).unwrap();
    }

    #[test]
    fn draw_detail_view() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-det", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.detail_view = true;
        terminal.draw(|f| dash.draw(f)).unwrap();
    }

    #[test]
    fn draw_reregisters_selection_regions_every_frame() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        terminal.draw(|f| dash.draw(f)).unwrap();
        let first_frame = dash.selection_regions.clone();
        assert!(!first_frame.is_empty());
        // A second draw replaces the registrations instead of accumulating.
        terminal.draw(|f| dash.draw(f)).unwrap();
        assert_eq!(dash.selection_regions, first_frame);
    }

    #[test]
    fn draw_mcp_screen_drops_any_selection() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        // Start a drag over the log panel on the normal screen.
        terminal.draw(|f| dash.draw(f)).unwrap();
        let region = dash.selection_regions[0];
        dash.handle_mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: region.x,
            row: region.y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert!(dash.selection.is_some());
        // The MCP screen has no selectable panes; switching to it clears.
        dash.mcp_screen = true;
        terminal.draw(|f| dash.draw(f)).unwrap();
        assert!(dash.selection.is_none());
        assert!(dash.selection_regions.is_empty());
    }

    #[test]
    fn draw_detail_view_clamps_out_of_range_selected_stage() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let agent = make_test_agent("run-det-clamp", AgentDisplayStatus::Active);
        assert_eq!(agent.num_stages, 1);
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.selected_stage = 99; // way beyond num_stages - 1

        terminal.draw(|f| dash.draw(f)).unwrap();
        assert_eq!(dash.selected_stage, 0);
    }

    #[test]
    fn draw_detail_view_input_mode_multiple_choice_pending() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-mc-input", AgentDisplayStatus::Waiting);
        agent.waiting_prompt = Some("Pick one".to_string());
        agent.pending_request = Some(
            leviath_core::interaction::InteractionRequest::multiple_choice(
                "mc1",
                "Pick one",
                vec!["A".to_string(), "B".to_string()],
                "main",
            ),
        );
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = true;

        terminal.draw(|f| dash.draw(f)).unwrap();
    }

    #[test]
    fn draw_detail_view_review_body_shows_when_not_output_mode() {
        // Logs mode + a pending review body ⇒ the output pane is not showing the
        // body, so the separate review pane renders it (review_body Some path).
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.stage_content_mode = StageContentMode::Logs;
        let mut agent = make_test_agent("run-review-logs", AgentDisplayStatus::Waiting);
        agent.stages = vec![crate::runstate::StageRecord::new("main".to_string(), 0)];
        let mut req = leviath_core::interaction::InteractionRequest::multiple_choice(
            "mc1",
            "Approve?",
            vec!["Approve".to_string()],
            "plan_approval",
        );
        req.body = Some("## Plan\n1. write it\n2. test it".to_string());
        agent.pending_request = Some(req);
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = false;
        dash.selected_stage = 0;

        terminal.draw(|f| dash.draw(f)).unwrap();
    }

    #[test]
    fn draw_detail_view_output_body_suppresses_review_pane() {
        // Output mode + a pending review body ⇒ the output pane shows the body,
        // so the separate review pane is suppressed (content_shows_body branch).
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.stage_content_mode = StageContentMode::Output;
        let mut agent = make_test_agent("run-review-detail", AgentDisplayStatus::Waiting);
        agent.stages = vec![crate::runstate::StageRecord::new("main".to_string(), 0)];
        let mut req = leviath_core::interaction::InteractionRequest::multiple_choice(
            "mc1",
            "Approve?",
            vec!["Approve".to_string()],
            "plan_approval",
        );
        req.body = Some("## Plan\n1. write it".to_string());
        agent.pending_request = Some(req);
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = false;
        dash.selected_stage = 0;

        terminal.draw(|f| dash.draw(f)).unwrap();
    }

    #[test]
    fn draw_detail_view_inline_document_edit_suppresses_bottom_input() {
        // Editing an EditText renders the textarea in the content pane, so the
        // bottom input bar is suppressed (prompt_height == 0 branch).
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-edit-detail", AgentDisplayStatus::Waiting);
        agent.pending_request = Some(leviath_core::interaction::InteractionRequest::edit_text(
            "et1",
            "Edit",
            "main",
            "the current plan text",
        ));
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = true;
        assert!(dash.editing_document());

        terminal.draw(|f| dash.draw(f)).unwrap();
    }

    #[test]
    fn draw_detail_view_waiting_multiple_choice_preview_not_input_mode() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-mc-preview", AgentDisplayStatus::Waiting);
        agent.waiting_prompt = Some("Pick one".to_string());
        agent.pending_request = Some(
            leviath_core::interaction::InteractionRequest::multiple_choice(
                "mc2",
                "Pick one",
                vec!["A".to_string(), "B".to_string()],
                "main",
            ),
        );
        agent.stages = vec![crate::runstate::StageRecord::new("main".to_string(), 0)];
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = false;
        dash.selected_stage = 0;

        terminal.draw(|f| dash.draw(f)).unwrap();
    }

    #[test]
    fn draw_detail_view_graph_agent_uses_taller_stage_tabs_area() {
        // `tabs_h` is `7` when `agent.graph_info.is_some()` and `3` otherwise
        // -- every other detail-view test above leaves `graph_info: None`, so
        // the `7` branch was never exercised. Build a minimal
        // `GraphTransitionInfo` (same shape used by `render/stages.rs`'s own
        // graph tests) purely to flip `is_graph_view` to `true`; the graph
        // rendering itself is already exhaustively covered there.
        use crate::commands::dashboard::graph::GraphTransitionInfo;
        use std::collections::HashMap;

        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-graph-tabs", AgentDisplayStatus::Active);
        agent.graph_info = Some(GraphTransitionInfo {
            edges: HashMap::new(),
            entry_stage: "main".to_string(),
            stage_names: vec!["main".to_string()],
        });
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;

        terminal.draw(|f| dash.draw(f)).unwrap();
    }

    #[test]
    fn draw_detail_view_input_mode_waiting_wrong_stage_no_prompt_pane() {
        // `prompt_height`'s condition is
        // `has_prompt || (input_mode && is_waiting && selected_stage_can_respond()) || (input_mode && accepts_messages)`.
        // Every other test either has `has_prompt == true` (short-circuiting
        // before the middle term's own `selected_stage_can_respond()` call
        // ever runs) or isn't in `input_mode` at all. Force `has_prompt` to
        // `false` via a wrong `selected_stage` while `input_mode` and
        // `is_waiting` are both `true`, and `accepts_messages` is `false`
        // (status is `Waiting`, not `Active`) so no term short-circuits away
        // the middle term's own `selected_stage_can_respond()` re-check.
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-wrong-stage-input", AgentDisplayStatus::Waiting);
        agent.waiting_prompt = Some("Pick one".to_string());
        agent.pending_request = Some(
            leviath_core::interaction::InteractionRequest::multiple_choice(
                "mc-wrong-stage",
                "Pick one",
                vec!["A".to_string(), "B".to_string()],
                "main",
            ),
        );
        agent.num_stages = 2;
        agent.stage_index = 0;
        agent.stages = vec![
            crate::runstate::StageRecord::new("main".to_string(), 0),
            crate::runstate::StageRecord::new("code".to_string(), 1),
        ];
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = true;
        dash.selected_stage = 1; // wrong stage: selected_stage_can_respond() -> false

        terminal.draw(|f| dash.draw(f)).unwrap();
    }

    // ─── Regression: mid-run message input pane must actually render ──────
    //
    // Pressing 'i' on an Active agent that accepts mid-run messages sets
    // input_mode = true. `prompt_height` must account for this case (not just
    // the `is_waiting` case), or the input pane never gets laid out or
    // rendered for an Active, non-waiting agent. Verify the textarea's hint
    // text actually appears in the rendered buffer.
    #[test]
    fn draw_detail_view_renders_input_pane_for_accepts_messages_while_active() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let agent = make_test_agent("run-msg", AgentDisplayStatus::Active);
        assert!(agent.accepts_messages);
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = true;

        terminal.draw(|f| dash.draw(f)).unwrap();

        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(rendered.contains("Provide input while this is running"));
    }

    #[test]
    fn draw_with_show_help() {
        let backend = TestBackend::new(120, 50);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.show_help = true;
        terminal.draw(|f| dash.draw(f)).unwrap();
    }

    #[test]
    fn draw_with_confirm_delete() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-del", AgentDisplayStatus::Complete));
        dash.update_display_indices();
        dash.confirm_delete = true;
        terminal.draw(|f| dash.draw(f)).unwrap();
    }

    #[test]
    fn draw_with_toasts() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.toasts.push(Toast {
            message: "Hello".to_string(),
            remaining_ticks: 25,
            level: ToastLevel::Info,
        });
        terminal.draw(|f| dash.draw(f)).unwrap();
    }

    #[test]
    fn draw_detail_panel_no_agent() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        terminal
            .draw(|f| {
                let area = f.area();
                dash.draw_detail_panel(f, area);
            })
            .unwrap();
    }

    #[test]
    fn draw_detail_panel_active_agent() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-dp", AgentDisplayStatus::Active));
        dash.update_display_indices();
        terminal
            .draw(|f| {
                let area = f.area();
                dash.draw_detail_panel(f, area);
            })
            .unwrap();
    }

    #[test]
    fn draw_detail_panel_waiting_agent() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-wait", AgentDisplayStatus::Waiting);
        agent.waiting_prompt = Some("What should I do?".to_string());
        agent.pending_request = Some(leviath_core::interaction::InteractionRequest {
            id: "req-1".to_string(),
            kind: leviath_core::interaction::InteractionKind::FreeText,
            prompt: "What should I do?".to_string(),
            options: vec![],
            tool_name: None,
            tool_arguments: None,
            required: true,
            stage_name: "main".to_string(),
            body: None,
            body_format: leviath_core::interaction::BodyFormat::Plain,
        });
        dash.agents.push(agent);
        dash.update_display_indices();
        terminal
            .draw(|f| {
                let area = f.area();
                dash.draw_detail_panel(f, area);
            })
            .unwrap();
    }

    #[test]
    fn draw_detail_panel_error_agent() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.agents.push(make_test_agent(
            "run-err",
            AgentDisplayStatus::Error("something broke".to_string()),
        ));
        dash.update_display_indices();
        terminal
            .draw(|f| {
                let area = f.area();
                dash.draw_detail_panel(f, area);
            })
            .unwrap();
    }

    #[test]
    fn draw_detail_panel_complete_interactive() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-ci", AgentDisplayStatus::CompleteInteractive);
        agent.waiting_prompt = Some("Anything else?".to_string());
        dash.agents.push(agent);
        dash.update_display_indices();
        terminal
            .draw(|f| {
                let area = f.area();
                dash.draw_detail_panel(f, area);
            })
            .unwrap();
    }

    #[test]
    fn draw_detail_panel_with_context_snapshot() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-cs", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(crate::runstate::ContextSnapshot {
            stage_name: "main".to_string(),
            total_tokens: 4000,
            max_tokens: 8000,
            regions: vec![],
        });
        dash.agents.push(agent);
        dash.update_display_indices();
        terminal
            .draw(|f| {
                let area = f.area();
                dash.draw_detail_panel(f, area);
            })
            .unwrap();
    }

    #[test]
    fn draw_detail_panel_with_review_body() {
        let backend = TestBackend::new(120, 50);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-rv", AgentDisplayStatus::Waiting);
        agent.waiting_prompt = Some("Review this plan".to_string());
        agent.pending_request = Some(leviath_core::interaction::InteractionRequest {
            id: "req-rv".to_string(),
            kind: leviath_core::interaction::InteractionKind::FreeText,
            prompt: "Review this plan".to_string(),
            options: vec![],
            tool_name: None,
            tool_arguments: None,
            required: true,
            stage_name: "main".to_string(),
            body: Some("# Plan\n\n- Step 1\n- Step 2\n- Step 3\n\nDo you approve?".to_string()),
            body_format: leviath_core::interaction::BodyFormat::Markdown,
        });
        dash.agents.push(agent);
        dash.update_display_indices();
        terminal
            .draw(|f| {
                let area = f.area();
                dash.draw_detail_panel(f, area);
            })
            .unwrap();
    }

    #[test]
    fn draw_detail_panel_with_stages() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-stg", AgentDisplayStatus::Active);
        agent.num_stages = 3;
        agent.stages = vec![
            crate::runstate::StageRecord {
                name: "plan".to_string(),
                index: 0,
                status: crate::runstate::StageRunStatus::Complete,
                prompt_tokens: 100,
                completion_tokens: 50,
                cached_tokens: 0,
                started_at: Some(chrono::Utc::now().timestamp() - 60),
                ended_at: Some(chrono::Utc::now().timestamp() - 30),
            },
            crate::runstate::StageRecord {
                name: "implement".to_string(),
                index: 1,
                status: crate::runstate::StageRunStatus::Active,
                prompt_tokens: 200,
                completion_tokens: 80,
                cached_tokens: 0,
                started_at: Some(chrono::Utc::now().timestamp() - 30),
                ended_at: None,
            },
            crate::runstate::StageRecord {
                name: "review".to_string(),
                index: 2,
                status: crate::runstate::StageRunStatus::Pending,
                prompt_tokens: 0,
                completion_tokens: 0,
                cached_tokens: 0,
                started_at: None,
                ended_at: None,
            },
        ];
        dash.agents.push(agent);
        dash.update_display_indices();
        terminal
            .draw(|f| {
                let area = f.area();
                dash.draw_detail_panel(f, area);
            })
            .unwrap();
    }

    #[test]
    fn draw_detail_panel_cancelled_agent() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-can", AgentDisplayStatus::Cancelled));
        dash.update_display_indices();
        terminal
            .draw(|f| {
                let area = f.area();
                dash.draw_detail_panel(f, area);
            })
            .unwrap();
    }
}
