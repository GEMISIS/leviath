//! Input/prompt pane and review body rendering.

use ratatui::Frame;
use ratatui::layout::{Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};

use crate::commands::dashboard::helpers::truncate;
use crate::commands::dashboard::state::Dashboard;
use crate::commands::dashboard::theme::*;
use crate::commands::dashboard::types::*;
use crate::tui::widgets::markdown_edit::MdEditView;
use leviath_core::interaction;

/// The Send button's face. Fixed text, so the click rect is the drawn text.
pub(in crate::commands::dashboard) const SEND_BUTTON: &str = "[ Send ]";

/// The Save button's face, under an in-place document edit.
pub(in crate::commands::dashboard) const SAVE_BUTTON: &str = "[ Save document ]";

impl Dashboard {
    pub(in crate::commands::dashboard) fn render_review_body(
        &mut self,
        frame: &mut Frame,
        review_area: Rect,
        review_lines: &[ratatui::text::Line<'static>],
        pending_req: &Option<interaction::InteractionRequest>,
    ) {
        let inner_h = review_area.height.saturating_sub(2) as usize;
        let render_width = review_area.width.saturating_sub(2);

        // Scroll in display rows (long lines wrap at draw time), so the
        // bottom of the document is genuinely reachable - the same fix as the
        // content pane, measured the same way.
        let total_rows = super::content::wrapped_rows(review_lines, render_width);
        let max_rv_scroll = total_rows.saturating_sub(inner_h);
        if self.review_scroll > max_rv_scroll {
            self.review_scroll = max_rv_scroll;
        }
        let rv_scroll_y = max_rv_scroll - self.review_scroll;

        // The prompt is the pane's title, so it gets the top border less the
        // corners and its own padding, rather than a fixed 50 characters.
        let rv_title = if let Some(req) = &pending_req {
            let room = (review_area.width as usize).saturating_sub(4);
            format!(" {} ", truncate(&req.prompt, room))
        } else {
            " Review ".to_string()
        };
        let rv_scroll_info = if total_rows > inner_h {
            let pct = 100
                - (self.review_scroll.min(max_rv_scroll) * 100)
                    .checked_div(max_rv_scroll)
                    .unwrap_or(0);
            format!(" {}% ", pct)
        } else {
            String::new()
        };
        let review_widget = Paragraph::new(review_lines.to_vec())
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
            .wrap(Wrap { trim: false })
            .scroll((rv_scroll_y.min(u16::MAX as usize) as u16, 0));
        frame.render_widget(review_widget, review_area);

        // Scrollbar for review body, in display rows.
        if total_rows > inner_h {
            let rv_scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("↑"))
                .end_symbol(Some("↓"));
            let mut rv_sb = ScrollbarState::new(max_rv_scroll).position(rv_scroll_y);
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
            && (self.deny_feedback_open
                || matches!(
                    kind,
                    Some(InteractionKind::FreeText) | Some(InteractionKind::EditText) | None
                ))
        {
            // ── FreeText / EditText: render the multi-line tui-textarea widget ──
            // No pending interaction request/prompt means this is a mid-run
            // message to a still-running agent rather than a response to a
            // specific question - label it accordingly for consistent UX.
            let is_message_mode = pending_req.is_none() && agent.waiting_prompt.is_none();
            let hint = if self.deny_feedback_open {
                " Deny with feedback: what should it do instead?  [^Enter] send  [Enter] newline  [Tab] Send  [Esc] back "
            } else if is_message_mode {
                " Provide input while this is running  [^Enter] send  [Enter] newline  [Tab] Send button  [Esc] cancel "
            } else {
                " Response  [^Enter] send  [Enter] newline  [Tab] Send button  [Esc] cancel "
            };
            // The response box is a long-form field: it wraps, and it carries
            // the same formatting toolbar as the task editor. Enter breaks the
            // line there, so the Send button under it is how a terminal that
            // cannot send Ctrl+Enter submits.
            let (editor_area, button_row) = super::button::editor_and_button_rows(prompt_area);
            let view = MdEditView::new(hint, C_SUCCESS, !self.response_focus_send);
            self.input_textarea.render(frame, editor_area, &view);
            self.draw_action_button(
                frame,
                button_row,
                SEND_BUTTON,
                self.response_focus_send,
                ClickTarget::ResponseSend,
            );
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
                // An EditText opens the document for in-place editing in the pane
                // above, so label it as editing rather than a generic response.
                let is_edit = matches!(kind, Some(InteractionKind::EditText));
                let hint = match (is_edit, &agent.status) {
                    (true, _) => " [i] edit the document above  [k] kill",
                    (false, AgentDisplayStatus::CompleteInteractive) => " [i] respond",
                    (false, _) => " [i] respond  [k] kill",
                };
                lines.push(Line::from(Span::styled(hint, Style::default().fg(C_DIM))));
                let title = match (is_edit, &agent.status) {
                    (true, _) => " Edit Document ",
                    (false, AgentDisplayStatus::CompleteInteractive) => " Input Allowed ",
                    (false, _) => " Input Required ",
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
    use crate::commands::dashboard::test_support::{make_test_dashboard, rendered_buffer};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

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
            broken_scripts: Vec::new(),
            waiting_prompt: Some("What should I do?".to_string()),
            wait_reason: None,
            pending_request: None,
            last_answered_request_id: None,
            context_snapshot: None,
            stages: vec![],
            workdir: "/tmp/test".to_string(),
            task: "test task".to_string(),
            title: Some("My Test".to_string()),
            model: None,
            parent_id: None,
            started_at: chrono::Utc::now().timestamp() - 60,
            last_progress_at: None,
            runtime_secs: 0,
            clock_now: 0,
            graph: None,
            accepts_messages: true,
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("Line 1"), "{buf}");
        assert!(buf.contains("Line 2"), "{buf}");
        assert!(buf.contains("Line 3"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("\u{2191}"), "no scrollbar drawn: {buf}");
    }

    /// Same display-row fix as the content pane: a review document whose
    /// lines wrap must still show its final line when scrolled to the bottom.
    #[test]
    fn wrapped_review_shows_its_last_line_at_the_bottom() {
        let backend = TestBackend::new(50, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.review_scroll = 0;
        let lines: Vec<ratatui::text::Line<'static>> = vec![
            ratatui::text::Line::from("plan step ".repeat(30)),
            ratatui::text::Line::from("detail text ".repeat(25)),
            ratatui::text::Line::from("REVIEW-FINAL-LINE"),
        ];
        let pending: Option<interaction::InteractionRequest> = None;
        terminal
            .draw(|f| dash.render_review_body(f, Rect::new(0, 0, 48, 12), &lines, &pending))
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            screen.contains("REVIEW-FINAL-LINE"),
            "the review tail must be visible at review_scroll 0:\n{screen}"
        );
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("Review content"), "{buf}");
    }

    /// With the feedback box open under a tool approval, the pane is the
    /// response box labelled for the deny, not the choice list.
    #[test]
    fn render_input_pane_deny_feedback_box() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.input_mode = true;
        dash.deny_feedback_open = true;
        let agent = make_test_agent("run-df", AgentDisplayStatus::Waiting);
        let pending = Some(make_pending_req(interaction::InteractionKind::ToolApproval));
        let kind = Some(interaction::InteractionKind::ToolApproval);
        let options = vec!["Allow once".to_string(), "Deny with feedback".to_string()];
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 110, 11);
                dash.render_input_pane(f, area, &agent, &pending, &kind, &options);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("Deny with feedback"), "{buf}");
        assert!(buf.contains("[Esc] back"), "{buf}");
        assert!(buf.contains("Send"), "{buf}");
        assert!(
            !buf.contains("[1] Allow once"),
            "the choice list is gone: {buf}"
        );
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("Esc"), "{buf}");
        assert!(buf.contains("[Enter] newline"), "{buf}");
        assert!(buf.contains(SEND_BUTTON), "{buf}");
    }

    /// Tab lights the Send button the way it lights the Start button: the
    /// box's border goes quiet and the button takes the focus colour.
    #[test]
    fn the_send_button_lights_when_tab_reaches_it() {
        let agent = make_test_agent("run-ft", AgentDisplayStatus::Waiting);
        let pending: Option<interaction::InteractionRequest> = None;
        let kind = Some(interaction::InteractionKind::FreeText);
        let options: Vec<String> = vec![];
        let mut styles = vec![];
        for focus_send in [false, true] {
            let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
            let mut dash = make_test_dashboard();
            dash.input_mode = true;
            dash.response_focus_send = focus_send;
            terminal
                .draw(|f| {
                    let area = Rect::new(0, 0, 80, 12);
                    dash.render_input_pane(f, area, &agent, &pending, &kind, &options);
                })
                .unwrap();
            let button = dash
                .click_targets
                .iter()
                .find(|(_, t)| *t == ClickTarget::ResponseSend)
                .map(|(r, _)| *r)
                .expect("the button registered its rect");
            assert_eq!(button.y, 11, "on the pane's last row");
            assert_eq!(button.x + button.width, 80, "right-aligned");
            styles.push(terminal.backend().buffer()[(button.x, button.y)].style());
        }
        assert_ne!(styles[0], styles[1], "focus changes the button's look");
        assert_eq!(styles[1].bg, Some(C_BORDER_FOCUS));
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("Esc"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("Option A"), "{buf}");
        assert!(buf.contains("Option B"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("Yes"), "{buf}");
        assert!(buf.contains("No"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("[i] respond"), "{buf}");
        assert!(buf.contains("Choose wisely"), "{buf}");
    }

    #[test]
    fn render_input_pane_edit_text_preview_labels_editing() {
        // A pending EditText (not yet editing) shows the "Edit Document" title and
        // the edit-the-document hint, not the generic response labels.
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.input_mode = false;
        let agent = make_test_agent("run-edit-preview", AgentDisplayStatus::Waiting);
        let pending = Some(make_pending_req(interaction::InteractionKind::EditText));
        let kind = Some(interaction::InteractionKind::EditText);
        let options: Vec<String> = vec![];
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 80, 8);
                dash.render_input_pane(f, area, &agent, &pending, &kind, &options);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("Edit Document"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("Option A"), "{buf}");
        assert!(buf.contains("Option B"), "{buf}");
        assert!(buf.contains("Option C"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("Yes"), "{buf}");
        assert!(buf.contains("No"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("Anything else?"), "{buf}");
    }

    /// The prompt in the review pane's title is cut to the pane, not to a
    /// fixed 50 characters: whole on a wide pane, ellipsis on a narrow one.
    #[test]
    fn render_review_body_fits_the_prompt_to_the_pane() {
        let prompt = "Review the draft and say whether the executive summary is ready";
        let mut req = make_pending_req(interaction::InteractionKind::FreeText);
        req.prompt = prompt.to_string();
        let pending = Some(req);
        let lines: Vec<ratatui::text::Line<'static>> =
            vec![ratatui::text::Line::from("Review content")];
        let mut dash = make_test_dashboard();

        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| dash.render_review_body(f, Rect::new(0, 0, 120, 10), &lines, &pending))
            .unwrap();
        let wide = rendered_buffer(&terminal);
        assert!(wide.contains(prompt), "{wide}");

        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| dash.render_review_body(f, Rect::new(0, 0, 40, 10), &lines, &pending))
            .unwrap();
        let narrow = rendered_buffer(&terminal);
        assert!(!narrow.contains(prompt), "{narrow}");
        // 40 wide, 4 off: 36 columns, 35 of prompt and the ellipsis.
        assert!(
            narrow.contains("Review the draft and say whether th…"),
            "{narrow}"
        );
    }
}
