//! Rendering logic for the dashboard TUI.
//!
//! The main `draw()` method dispatches to sub-modules for each region of the UI.

mod content;
mod header;
mod input;
mod overlays;
mod stages;
mod table;

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::Frame;

use super::state::Dashboard;
use super::types::*;

impl Dashboard {
    pub(super) fn draw(&mut self, frame: &mut Frame) {
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
        use crate::interaction::InteractionKind;

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

        let header_h: u16 = 1; // compact breadcrumb line
        let info_h: u16 = 4; // task + workdir/stats strip (2 content + 2 border lines)
        let is_graph_view = agent.graph_info.is_some();
        let tabs_h: u16 = if is_graph_view { 7 } else { 3 }; // graph needs more height
        let context_h: u16 = if agent.context_snapshot.is_some() || !agent.stages.is_empty() {
            5
        } else {
            0
        };

        // Review body: shown when the pending interaction carries markdown for review
        let review_body = if !self.input_mode && has_prompt {
            pending_req.as_ref().and_then(|r| r.body.as_deref())
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

        let prompt_height: u16 =
            if has_prompt || (self.input_mode && is_waiting && self.selected_stage_can_respond()) {
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
