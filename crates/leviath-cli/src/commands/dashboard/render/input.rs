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

        if self.input_mode && matches!(kind, Some(InteractionKind::FreeText) | None) {
            // ── FreeText: render the multi-line tui-textarea widget ──────────
            let hint = " Response  [Enter] send  [Alt+↵] newline  [Esc] cancel ";
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
