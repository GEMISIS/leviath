//! Rendering logic for the dashboard TUI.
//!
//! The main `draw()` method dispatches to sub-modules for each region of the UI.

mod content;
mod graph_view;
mod header;
mod input;
mod mcp;
mod new_run;
mod overlays;
mod stages;
mod table;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};

use super::state::Dashboard;
use super::types::*;

impl Dashboard {
    pub(super) fn draw(&mut self, frame: &mut Frame) {
        // The whole frame accepts mouse selection, exactly like native
        // terminal selection; re-registered per frame so a resize (the one
        // event that moves text under a screen-anchored highlight without a
        // scroll) drops the selection.
        self.selection_regions.clear();
        self.selection_regions.push(frame.area());
        // Wheel hit-testing rects are also per-frame: each pane's renderer
        // registers where it actually drew.
        self.pane_rects.clear();

        if self.mcp_screen {
            self.draw_mcp_screen(frame, frame.area());
            self.apply_selection_overlay(frame);
            self.draw_toasts(frame);
            if self.show_help {
                self.draw_help_overlay(frame);
            }
            // The remove confirmation can be open over the MCP screen.
            if let Some((_, dialog)) = &self.pending_confirm {
                dialog.draw(frame, frame.area());
            }
            return;
        }
        if self.new_run_screen {
            self.draw_new_run_screen(frame, frame.area());
            self.apply_selection_overlay(frame);
            self.draw_toasts(frame);
            // This screen types text, so `?` is a question mark here and F1 is
            // the way in. Without drawing the overlay it would have no way out
            // of being pressed and nothing happening.
            if self.show_help {
                self.draw_help_overlay(frame);
            }
            // The unattended warning opens over this screen.
            if let Some((_, dialog)) = &self.pending_confirm {
                dialog.draw(frame, frame.area());
            }
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

        // Kill / delete / remove confirmation dialog
        if let Some((_, dialog)) = &self.pending_confirm {
            dialog.draw(frame, frame.area());
        }
    }

    fn draw_detail_panel(&mut self, frame: &mut Frame, area: ratatui::layout::Rect) {
        use leviath_core::interaction::InteractionKind;

        let agent = match self.selected_agent() {
            Some(a) => a.clone(),
            None => {
                let msg = ratatui::widgets::Paragraph::new("No agent run selected.").block(
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

        // The full-screen stage explorer replaces the whole detail stack
        // below the breadcrumb while it is open.
        if self.stage_explorer.is_some() {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Min(4)])
                .split(area);
            self.render_header_breadcrumb(frame, chunks[0], &agent);
            self.draw_stage_explorer(frame, chunks[1], &agent);
            return;
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
        // supports mid-run messages - same 'i' key, same input pane.
        let accepts_messages = self.selected_agent_accepts_messages();

        let header_h: u16 = 1; // compact breadcrumb line
        let info_h: u16 = 4; // task + workdir/stats strip (2 content + 2 border lines)
        // Graph agents get the same 3-row tab strip as linear ones; the real
        // graph lives in the full-screen explorer (`g`), which no longer
        // costs every other pane four rows.
        let tabs_h: u16 = 3;
        let context_h: u16 = if agent.context_snapshot.is_some() || !agent.stages.is_empty() {
            5
        } else {
            0
        };

        // Review body: shown when the pending interaction carries markdown for
        // review. `EditText` also uses `body`, but that is the editable document
        // (rendered in the seeded textarea once the user starts editing), not a
        // read-only review doc - so it is excluded here. When the output pane is
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
    use crate::commands::dashboard::test_support::{make_test_dashboard, rendered_buffer};
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
            wait_reason: None,
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
            last_progress_at: None,
            active_until: None,
            waiting_secs: 0,
            graph: None,
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
        let buf = rendered_buffer(&terminal);
        assert!(!buf.contains("My Test"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("My Test #1"), "{buf}");
        assert!(buf.contains("My Test #2"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("ACTIVE"), "{buf}");
    }

    #[test]
    fn draw_registers_the_whole_frame_for_selection() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        terminal.draw(|f| dash.draw(f)).unwrap();
        // One region, the frame itself: selection works anywhere, exactly
        // like native terminal selection.
        assert_eq!(
            dash.selection_regions,
            vec![ratatui::layout::Rect::new(0, 0, 120, 40)]
        );
        // A second draw replaces the registration instead of accumulating.
        terminal.draw(|f| dash.draw(f)).unwrap();
        assert_eq!(dash.selection_regions.len(), 1);
    }

    #[test]
    fn a_selection_survives_switching_to_the_mcp_screen() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        terminal.draw(|f| dash.draw(f)).unwrap();
        dash.handle_mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 2,
            row: 2,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert!(dash.selection.is_some());
        // The frame is the region, so a view switch keeps the drag alive -
        // native terminal selection behaves the same way when content
        // redraws underneath it.
        dash.mcp_screen = true;
        terminal.draw(|f| dash.draw(f)).unwrap();
        assert!(dash.selection.is_some());
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("WAITING"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("write it"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        // The pair to the Logs-mode case above: in Output mode the body is
        // already in the output pane, so the review pane must not draw a
        // second copy of it.
        assert_eq!(buf.matches("write it").count(), 1, "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("WAITING"), "{buf}");
    }

    #[test]
    fn draw_detail_view_graph_agent_shows_the_graph_hint() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-graph-tabs", AgentDisplayStatus::Active);
        agent.graph = Some(std::sync::Arc::new(
            crate::tui::flowgraph::StageGraph::from_blueprint(
                &leviath_core::manifest::parse_manifest("[agent]\nname = \"g\"\n[stages.main]\n")
                    .unwrap(),
            ),
        ));
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;

        terminal.draw(|f| dash.draw(f)).unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("ACTIVE"), "{buf}");
        assert!(buf.contains("[g] graph"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("WAITING"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("Agent run list"), "{buf}");
    }

    #[test]
    fn draw_with_confirm_delete() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-del", AgentDisplayStatus::Complete));
        dash.update_display_indices();
        dash.request_delete();
        terminal.draw(|f| dash.draw(f)).unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("Esc"), "{buf}");
    }

    #[test]
    fn draw_mcp_screen_with_help_and_confirm_overlays() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.mcp_screen = true;
        dash.show_help = true;
        dash.pending_confirm = Some((
            crate::commands::dashboard::types::ConfirmAction::McpRemove {
                name: "srv".to_string(),
            },
            crate::tui::widgets::confirm::Confirm::new(
                "Remove MCP server?",
                vec![ratatui::text::Line::from("Remove 'srv'?")],
                "Remove",
                "Cancel",
            ),
        ));
        terminal.draw(|f| dash.draw(f)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("Remove MCP server?"));
    }

    #[test]
    fn draw_detail_view_with_the_explorer_open() {
        let backend = TestBackend::new(140, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-exp", AgentDisplayStatus::Active);
        let graph = std::sync::Arc::new(crate::tui::flowgraph::StageGraph::from_blueprint(
            &leviath_core::manifest::parse_manifest(
                "[agent]\nname = \"g\"\n[stages.a]\n[stages.a.transitions.b]\n[stages.b]\n",
            )
            .unwrap(),
        ));
        agent.graph = Some(graph.clone());
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.stage_explorer = Some(crate::commands::dashboard::types::ExplorerState::new(
            crate::tui::flowgraph::FlowView::new(
                graph,
                crate::tui::flowgraph::NodeStyle::Full,
                false,
            ),
        ));
        terminal.draw(|f| dash.draw(f)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("Stage explorer"), "{text}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("Hello"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(!buf.contains("ACTIVE"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("ACTIVE"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("What should I do?"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("something broke"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("COMPLETE"), "{buf}");
    }

    #[test]
    fn draw_detail_panel_with_context_snapshot() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-cs", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(std::sync::Arc::new(crate::runstate::ContextSnapshot {
            stage_name: "main".to_string(),
            total_tokens: 4000,
            max_tokens: 8000,
            regions: vec![],
        }));
        dash.agents.push(agent);
        dash.update_display_indices();
        terminal
            .draw(|f| {
                let area = f.area();
                dash.draw_detail_panel(f, area);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("ctx"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("WAITING"), "{buf}");
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
                entered: true,
                prompt_tokens: 100,
                completion_tokens: 50,
                cached_tokens: 0,
                cache_write_tokens: 0,
                region_tokens: Default::default(),
                first_call_prompt_tokens: None,
                runaway_warned: false,
                started_at: Some(chrono::Utc::now().timestamp() - 60),
                ended_at: Some(chrono::Utc::now().timestamp() - 30),
            },
            crate::runstate::StageRecord {
                name: "implement".to_string(),
                index: 1,
                status: crate::runstate::StageRunStatus::Active,
                entered: true,
                prompt_tokens: 200,
                completion_tokens: 80,
                cached_tokens: 0,
                cache_write_tokens: 0,
                region_tokens: Default::default(),
                first_call_prompt_tokens: None,
                runaway_warned: false,
                started_at: Some(chrono::Utc::now().timestamp() - 30),
                ended_at: None,
            },
            crate::runstate::StageRecord {
                name: "review".to_string(),
                index: 2,
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("ACTIVE"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("CANCEL"), "{buf}");
    }

    /// The new-run screen draws its own help and its own dialog. It used to
    /// return before both, so F1 there did nothing visible and the unattended
    /// warning would have opened behind the screen that raised it.
    #[test]
    fn the_new_run_screen_draws_its_overlays() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = crate::commands::dashboard::test_support::make_test_dashboard();
        dash.new_run_ctx = crate::commands::dashboard::types::NewRunContext {
            agents_dir: dir.path().join("agents"),
            config_path: dir.path().join("config.toml"),
            workdir: dir.path().join("work"),
        };
        std::fs::create_dir_all(dir.path().join("work")).unwrap();
        dash.open_new_run_screen();

        dash.show_help = true;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(110, 30)).unwrap();
        terminal.draw(|f| dash.draw(f)).unwrap();
        assert!(
            crate::commands::dashboard::test_support::rendered_buffer(&terminal)
                .contains("New run: agent blueprints")
        );

        dash.show_help = false;
        dash.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('y'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        terminal.draw(|f| dash.draw(f)).unwrap();
        assert!(
            crate::commands::dashboard::test_support::rendered_buffer(&terminal)
                .contains("Run unattended?")
        );
    }
}
