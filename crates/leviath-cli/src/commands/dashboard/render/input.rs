//! Input/prompt pane and review body rendering.

use ratatui::layout::{Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};
use ratatui::Frame;

use crate::commands::dashboard::helpers::truncate;
use crate::commands::dashboard::state::Dashboard;
use crate::commands::dashboard::theme::*;
use crate::commands::dashboard::types::*;
use crate::interaction;

impl Dashboard {
    pub(in crate::commands::dashboard) fn render_review_body(
        &mut self,
        frame: &mut Frame,
        review_area: Rect,
        review_lines: &[ratatui::text::Line<'static>],
        pending_req: &Option<interaction::InteractionRequest>,
    ) {
        let inner_h = review_area.height.saturating_sub(2) as usize;

        // Clamp scroll
        let max_rv_scroll = review_lines.len().saturating_sub(inner_h);
        if self.review_scroll > max_rv_scroll {
            self.review_scroll = max_rv_scroll;
        }
        let rv_start = review_lines
            .len()
            .saturating_sub(inner_h + self.review_scroll);
        let rv_end = (rv_start + inner_h).min(review_lines.len());
        let visible_review: Vec<Line> = review_lines[rv_start..rv_end].to_vec();

        let rv_title = if let Some(req) = &pending_req {
            format!(" {} ", truncate(&req.prompt, 50))
        } else {
            " Review ".to_string()
        };
        let rv_scroll_info = if review_lines.len() > inner_h {
            let pct = 100
                - (self.review_scroll.min(max_rv_scroll) * 100)
                    .checked_div(max_rv_scroll)
                    .unwrap_or(0);
            format!(" {}% ", pct)
        } else {
            String::new()
        };
        let review_widget = Paragraph::new(visible_review)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(C_WARN))
                    .title(Span::styled(
                        &rv_title,
                        Style::default().fg(C_WARN).add_modifier(Modifier::BOLD),
                    ))
                    .title_bottom(Span::styled(rv_scroll_info, Style::default().fg(C_DIM))),
            )
            .wrap(Wrap { trim: false });
        frame.render_widget(review_widget, review_area);

        // Scrollbar for review body
        if review_lines.len() > inner_h {
            let rv_scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("↑"))
                .end_symbol(Some("↓"));
            let mut rv_sb = ScrollbarState::new(max_rv_scroll)
                .position(max_rv_scroll.saturating_sub(self.review_scroll));
            frame.render_stateful_widget(
                rv_scrollbar,
                review_area.inner(Margin {
                    vertical: 1,
                    horizontal: 0,
                }),
                &mut rv_sb,
            );
        }
    }

    pub(in crate::commands::dashboard) fn render_input_pane(
        &mut self,
        frame: &mut Frame,
        prompt_area: Rect,
        agent: &DashboardAgent,
        pending_req: &Option<interaction::InteractionRequest>,
        kind: &Option<interaction::InteractionKind>,
        options: &[String],
    ) {
        use interaction::InteractionKind;

        if self.input_mode
            && matches!(
                kind,
                Some(InteractionKind::FreeText) | Some(InteractionKind::EditText) | None
            )
        {
            // ── FreeText / EditText: render the multi-line tui-textarea widget ──
            // No pending interaction request/prompt means this is a mid-run
            // message to a still-running agent rather than a response to a
            // specific question — label it accordingly for consistent UX.
            let is_message_mode = pending_req.is_none() && agent.waiting_prompt.is_none();
            let hint = if is_message_mode {
                " Provide input while this is running  [Enter] send  [Alt+↵] newline  [Esc] cancel "
            } else {
                " Response  [Enter] send  [Alt+↵] newline  [Esc] cancel "
            };
            self.input_textarea.set_block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(C_SUCCESS))
                    .title(Span::styled(
                        hint,
                        Style::default().fg(C_SUCCESS).add_modifier(Modifier::BOLD),
                    )),
            );
            self.input_textarea.set_style(Style::default().fg(C_WHITE));
            self.input_textarea.set_cursor_style(
                Style::default()
                    .fg(ratatui::style::Color::Black)
                    .bg(C_ACCENT),
            );
            frame.render_widget(&self.input_textarea, prompt_area);
        } else {
            let (title, prompt_lines): (&str, Vec<Line>) = if self.input_mode {
                let mut lines: Vec<Line> = vec![];
                // MultipleChoice / ToolApproval / Confirm
                for (i, opt) in options.iter().enumerate() {
                    let sel = i == self.choice_selected;
                    let prefix = if sel { " > " } else { "   " };
                    let label = match &kind {
                        Some(InteractionKind::Confirm) => {
                            format!("{}{}) {}", prefix, if i == 0 { "y" } else { "n" }, opt)
                        }
                        _ => format!("{}[{}] {}", prefix, i + 1, opt),
                    };
                    let style = if sel {
                        Style::default().fg(C_WARN).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(C_MUTED)
                    };
                    lines.push(Line::from(Span::styled(label, style)));
                }
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    " [↑↓] select  [Enter] confirm  [Esc] cancel",
                    Style::default().fg(C_DIM),
                )));
                (" Response ", lines)
            } else {
                let mut lines: Vec<Line> = vec![];
                let prompt_text = pending_req
                    .as_ref()
                    .map(|r| r.prompt.as_str())
                    .or(agent.waiting_prompt.as_deref())
                    .unwrap_or("Waiting for input");
                lines.push(Line::from(Span::styled(
                    format!(" {}", prompt_text),
                    Style::default().fg(C_WARN),
                )));
                if !options.is_empty() {
                    lines.push(Line::from(""));
                    for (i, opt) in options.iter().enumerate() {
                        let label = match &kind {
                            Some(InteractionKind::Confirm) => {
                                format!("   {}) {}", if i == 0 { "y" } else { "n" }, opt)
                            }
                            _ => format!("   [{}] {}", i + 1, opt),
                        };
                        lines.push(Line::from(Span::styled(
                            label,
                            Style::default().fg(C_MUTED),
                        )));
                    }
                }
                lines.push(Line::from(""));
                let hint = if matches!(agent.status, AgentDisplayStatus::CompleteInteractive) {
                    " [i] respond"
                } else {
                    " [i] respond  [k] kill"
                };
                lines.push(Line::from(Span::styled(hint, Style::default().fg(C_DIM))));
                let title = if matches!(agent.status, AgentDisplayStatus::CompleteInteractive) {
                    " Input Allowed "
                } else {
                    " Input Required "
                };
                (title, lines)
            };

            let prompt_color = if self.input_mode { C_SUCCESS } else { C_WARN };
            let prompt_widget = Paragraph::new(prompt_lines)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(prompt_color))
                        .title(Span::styled(
                            title,
                            Style::default()
                                .fg(prompt_color)
                                .add_modifier(Modifier::BOLD),
                        )),
                )
                .wrap(Wrap { trim: true });
            frame.render_widget(prompt_widget, prompt_area);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::dashboard::test_support::make_test_dashboard;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;

    fn make_test_agent(id: &str, status: AgentDisplayStatus) -> DashboardAgent {
        DashboardAgent {
            id: id.to_string(),
            blueprint_name: "test-agent".to_string(),
            agent_path: "/path".to_string(),
            stage: "main".to_string(),
            stage_index: 0,
            num_stages: 1,
            status,
            tokens_in: 100,
            tokens_out: 50,
            cached_tokens: 10,
            context_tokens: (500, 8000),
            iteration: 3,
            waiting_prompt: Some("What should I do?".to_string()),
            pending_request: None,
            last_answered_request_id: None,
            context_snapshot: None,
            stages: vec![],
            entity: bevy_ecs::prelude::Entity::from_raw(0),
            is_run_state: true,
            pid: 0,
            workdir: "/tmp/test".to_string(),
            task: "test task".to_string(),
            title: Some("My Test".to_string()),
            model: None,
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

    fn make_pending_req(kind: interaction::InteractionKind) -> interaction::InteractionRequest {
        interaction::InteractionRequest {
            id: "req-1".to_string(),
            kind,
            prompt: "Choose wisely".to_string(),
            options: vec!["Option A".to_string(), "Option B".to_string()],
            tool_name: None,
            tool_arguments: None,
            required: true,
            stage_name: "main".to_string(),
            body: None,
            body_format: interaction::BodyFormat::Plain,
        }
    }

    #[test]
    fn render_review_body_basic() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let lines: Vec<ratatui::text::Line<'static>> = vec![
            ratatui::text::Line::from("Line 1"),
            ratatui::text::Line::from("Line 2"),
            ratatui::text::Line::from("Line 3"),
        ];
        let pending: Option<interaction::InteractionRequest> = None;
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 80, 10);
                dash.render_review_body(f, area, &lines, &pending);
            })
            .unwrap();
    }

    #[test]
    fn render_review_body_with_scrollbar() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        // Create more lines than fit in the area to trigger scrollbar
        let lines: Vec<ratatui::text::Line<'static>> = (0..50)
            .map(|i| ratatui::text::Line::from(format!("Line {}", i)))
            .collect();
        let pending: Option<interaction::InteractionRequest> = None;
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 80, 10);
                dash.render_review_body(f, area, &lines, &pending);
            })
            .unwrap();
    }

    #[test]
    fn render_review_body_clamps_out_of_range_scroll() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        // Scroll offset far beyond the content length must be clamped down.
        dash.review_scroll = usize::MAX;
        let lines: Vec<ratatui::text::Line<'static>> = vec![
            ratatui::text::Line::from("Line 1"),
            ratatui::text::Line::from("Line 2"),
        ];
        let pending: Option<interaction::InteractionRequest> = None;
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 80, 10);
                dash.render_review_body(f, area, &lines, &pending);
            })
            .unwrap();
        assert!(dash.review_scroll < usize::MAX);
    }

    #[test]
    fn render_review_body_with_pending_req() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let lines: Vec<ratatui::text::Line<'static>> =
            vec![ratatui::text::Line::from("Review content")];
        let pending = Some(make_pending_req(interaction::InteractionKind::FreeText));
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 80, 10);
                dash.render_review_body(f, area, &lines, &pending);
            })
            .unwrap();
    }

    #[test]
    fn render_input_pane_freetext_input_mode() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.input_mode = true;
        let agent = make_test_agent("run-ft", AgentDisplayStatus::Waiting);
        let pending: Option<interaction::InteractionRequest> = None;
        let kind = Some(interaction::InteractionKind::FreeText);
        let options: Vec<String> = vec![];
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 80, 11);
                dash.render_input_pane(f, area, &agent, &pending, &kind, &options);
            })
            .unwrap();
    }

    #[test]
    fn render_input_pane_freetext_input_mode_no_kind() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.input_mode = true;
        let agent = make_test_agent("run-ft2", AgentDisplayStatus::Waiting);
        let pending: Option<interaction::InteractionRequest> = None;
        let kind: Option<interaction::InteractionKind> = None;
        let options: Vec<String> = vec![];
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 80, 11);
                dash.render_input_pane(f, area, &agent, &pending, &kind, &options);
            })
            .unwrap();
    }

    #[test]
    fn render_input_pane_multiple_choice_input_mode() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.input_mode = true;
        let agent = make_test_agent("run-mc", AgentDisplayStatus::Waiting);
        let pending = Some(make_pending_req(
            interaction::InteractionKind::MultipleChoice,
        ));
        let kind = Some(interaction::InteractionKind::MultipleChoice);
        let options = vec!["Option A".to_string(), "Option B".to_string()];
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 80, 14);
                dash.render_input_pane(f, area, &agent, &pending, &kind, &options);
            })
            .unwrap();
    }

    #[test]
    fn render_input_pane_confirm_input_mode() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.input_mode = true;
        let agent = make_test_agent("run-conf", AgentDisplayStatus::Waiting);
        let pending = Some(make_pending_req(interaction::InteractionKind::Confirm));
        let kind = Some(interaction::InteractionKind::Confirm);
        let options = vec!["Yes".to_string(), "No".to_string()];
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 80, 14);
                dash.render_input_pane(f, area, &agent, &pending, &kind, &options);
            })
            .unwrap();
    }

    #[test]
    fn render_input_pane_not_input_mode_with_prompt() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.input_mode = false;
        let agent = make_test_agent("run-noinput", AgentDisplayStatus::Waiting);
        let pending = Some(make_pending_req(interaction::InteractionKind::FreeText));
        let kind = Some(interaction::InteractionKind::FreeText);
        let options: Vec<String> = vec![];
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 80, 8);
                dash.render_input_pane(f, area, &agent, &pending, &kind, &options);
            })
            .unwrap();
    }

    #[test]
    fn render_input_pane_preview_with_options() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.input_mode = false;
        let agent = make_test_agent("run-preview", AgentDisplayStatus::Waiting);
        let pending = Some(make_pending_req(
            interaction::InteractionKind::MultipleChoice,
        ));
        let kind = Some(interaction::InteractionKind::MultipleChoice);
        let options = vec![
            "Option A".to_string(),
            "Option B".to_string(),
            "Option C".to_string(),
        ];
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 80, 14);
                dash.render_input_pane(f, area, &agent, &pending, &kind, &options);
            })
            .unwrap();
    }

    #[test]
    fn render_input_pane_preview_confirm_with_options() {
        // Not-input-mode preview of a Confirm request must label options
        // "y)"/"n)" rather than "[1]"/"[2]".
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.input_mode = false;
        let agent = make_test_agent("run-preview-confirm", AgentDisplayStatus::Waiting);
        let pending = Some(make_pending_req(interaction::InteractionKind::Confirm));
        let kind = Some(interaction::InteractionKind::Confirm);
        let options = vec!["Yes".to_string(), "No".to_string()];
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 80, 14);
                dash.render_input_pane(f, area, &agent, &pending, &kind, &options);
            })
            .unwrap();
    }

    #[test]
    fn render_input_pane_complete_interactive() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.input_mode = false;
        let mut agent = make_test_agent("run-ci", AgentDisplayStatus::CompleteInteractive);
        agent.waiting_prompt = Some("Anything else?".to_string());
        let pending: Option<interaction::InteractionRequest> = None;
        let kind: Option<interaction::InteractionKind> = None;
        let options: Vec<String> = vec![];
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 80, 8);
                dash.render_input_pane(f, area, &agent, &pending, &kind, &options);
            })
            .unwrap();
    }
}
